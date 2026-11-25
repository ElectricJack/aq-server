use super::hex40;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
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
            && hex40(&self.oid)
            && hex40(&self.previous)
    }
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
