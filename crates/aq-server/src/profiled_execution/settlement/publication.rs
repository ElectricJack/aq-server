//! D1 delivery owner for D3's default completion policy (`delivery =
//! required`). After D3 has accepted a result and holds only on missing
//! delivery evidence, this owner publishes the report's server-confirmed
//! retained source through the project's D1b publication policy, records the
//! server-owned delivery evidence D3 reads, and advances the project's source
//! head. Every aq-git call goes through D1a's durable intents and receipts; a
//! dispatched publication is never sent again.
//!
//! Epic 0 (spec 0016 section 7): with the cargo feature `delivery_playbook`
//! the automatic publish ([`automatic_publish`]) is compiled out; only the
//! recorded source head stays. A playbook publishes with `branch.publish`.
use super::owner::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;
#[cfg(not(feature = "delivery_playbook"))]
mod automatic_publish;
#[cfg(not(feature = "delivery_playbook"))]
pub use automatic_publish::{awaits_delivery, deliver_then_settle};

/// The project's integration-branch head as last published by AQ. Recorded in
/// the `published` delivery evidence that established it; the newest
/// `sequence` per project, repository, generation and target is current.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceHead {
    pub schema_version: u32,
    pub project_id: Uuid,
    pub repository_id: Uuid,
    pub binding_generation: u64,
    pub target: String,
    pub sequence: u64,
    /// The published commit (40 lowercase hex digits).
    pub oid: String,
    /// The aq-git candidate retention handle that retains `oid` centrally.
    pub retention_handle: Uuid,
    pub candidate_operation_id: Uuid,
    pub publication_operation_id: Uuid,
    /// The `expected_old` this publication fast-forwarded.
    pub previous: String,
}
impl SourceHead {
    pub fn validate(&self) -> bool {
        let hex = |s: &str| {
            s.len() == 40
                && s.bytes().any(|b| b != b'0')
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        };
        self.schema_version == 1
            && ![
                self.project_id,
                self.repository_id,
                self.retention_handle,
                self.candidate_operation_id,
                self.publication_operation_id,
            ]
            .iter()
            .any(Uuid::is_nil)
            && (1..=i64::MAX as u64).contains(&self.binding_generation)
            && (1..=i64::MAX as u64).contains(&self.sequence)
            && self.target.starts_with("refs/heads/")
            && self.target.len() <= 1024
            && hex(&self.oid)
            && hex(&self.previous)
    }
}

/// The current recorded source head, if AQ has published to this target.
pub async fn current_head<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    project: Uuid,
    repository: Uuid,
    generation: u64,
    target: &str,
) -> Result<Option<SourceHead>> {
    let row: Option<Value> = sqlx::query_scalar(
        "SELECT evidence->'source_head' FROM profiled_delivery_evidence \
         WHERE state='published' AND evidence->'source_head'->>'project_id'=$1 \
         AND evidence->'source_head'->>'repository_id'=$2 \
         AND evidence->'source_head'->>'binding_generation'=$3 \
         AND evidence->'source_head'->>'target'=$4 \
         ORDER BY (evidence->'source_head'->>'sequence')::bigint DESC LIMIT 1",
    )
    .bind(project.to_string())
    .bind(repository.to_string())
    .bind(generation.to_string())
    .bind(target)
    .fetch_optional(executor)
    .await
    .map_err(db)?;
    let Some(value) = row else { return Ok(None) };
    let head: SourceHead = serde_json::from_value(value).map_err(|_| conflict())?;
    if !head.validate() {
        return Err(conflict());
    }
    Ok(Some(head))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn head() -> SourceHead {
        SourceHead {
            schema_version: 1,
            project_id: Uuid::from_u128(1),
            repository_id: Uuid::from_u128(2),
            binding_generation: 1,
            target: "refs/heads/main".into(),
            sequence: 1,
            oid: "a".repeat(40),
            retention_handle: Uuid::from_u128(3),
            candidate_operation_id: Uuid::from_u128(3),
            publication_operation_id: Uuid::from_u128(4),
            previous: "b".repeat(40),
        }
    }
    #[test]
    fn source_head_shape_is_exact() {
        assert!(head().validate());
        let mut h = head();
        h.oid = "0".repeat(40);
        assert!(!h.validate());
        let mut h = head();
        h.target = "refs/tags/v1".into();
        assert!(!h.validate());
        let mut h = head();
        h.sequence = 0;
        assert!(!h.validate());
        let mut h = head();
        h.retention_handle = Uuid::nil();
        assert!(!h.validate());
        let mut v = serde_json::to_value(head()).unwrap();
        v["extra"] = json!(1);
        assert!(serde_json::from_value::<SourceHead>(v).is_err());
    }
}
