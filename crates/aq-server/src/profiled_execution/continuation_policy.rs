//! Immutable pinned policy/budget inputs, not current admission or authorization.
//! Callers hold current node/attempt ownership and separately fence live facts.
use crate::error::AppError;
use aq_admission::{ArtifactRef, LaunchBinding, RetainedArtifact};
use aq_profiles::{Intelligence, SelectionPolicy};
use aq_selection::BudgetSnapshot;
use serde_json::Value;
use sqlx::{Postgres, Row, Transaction};
pub(crate) type PinnedPolicy = (SelectionPolicy, u64);
fn refused() -> AppError {
    AppError::Conflict("pinned native policy budget unavailable or exhausted".into())
}
pub(crate) fn remaining(
    policy: &SelectionPolicy,
    budget: &BudgetSnapshot,
) -> Result<u64, AppError> {
    policy.validate().map_err(|_| refused())?;
    policy
        .max_lineage_elapsed_ms
        .checked_sub(budget.elapsed_ms.ok_or_else(refused)?)
        .filter(|n| *n > 0)
        .ok_or_else(refused)
}
fn decode(
    reference: ArtifactRef,
    body: &[u8],
    budget: BudgetSnapshot,
) -> Result<PinnedPolicy, AppError> {
    if body.is_empty()
        || body.len() > 2_097_152
        || aq_admission::Digest::of(body) != reference.digest
    {
        return Err(refused());
    }
    let definition: Intelligence = serde_json::from_slice(body).map_err(|_| refused())?;
    if reference.id.as_str()
        != format!(
            "intelligence:{}",
            aq_profiles::key(&definition.id, definition.version)
        )
        || reference.version.as_str() != definition.version.to_string()
    {
        return Err(refused());
    }
    let policy = definition.selection_policy.ok_or_else(refused)?;
    let remaining_lineage_ms = remaining(&policy, &budget)?;
    Ok((policy, remaining_lineage_ms))
}
pub(crate) async fn load_locked(
    tx: &mut Transaction<'_, Postgres>,
    binding: &LaunchBinding,
) -> Result<PinnedPolicy, AppError> {
    let row=sqlx::query("SELECT l.launch_binding,l.launch_manifest->'intelligence_profile' AS policy_ref,l.launch_manifest#>>'{bindings,project_id}' AS project,l.launch_manifest#>>'{bindings,graph_id}' AS workspace,l.launch_manifest#>>'{bindings,task_id}' AS task,d.decision#>'{inputs,budget}' AS budget FROM profiled_launches l JOIN intelligence_decisions d ON d.decision_id=l.decision_id WHERE l.attempt_id=$1 FOR SHARE OF l,d").bind(binding.attempt_id).fetch_optional(&mut **tx).await.map_err(|_|AppError::Internal)?.ok_or_else(refused)?;
    if row.get::<Value, _>("launch_binding")
        != serde_json::to_value(binding).map_err(|_| AppError::Internal)?
    {
        return Err(refused());
    }
    let reference: ArtifactRef =
        serde_json::from_value(row.get("policy_ref")).map_err(|_| refused())?;
    let budget: BudgetSnapshot =
        serde_json::from_value(row.get("budget")).map_err(|_| refused())?;
    if budget.lineage.project_id.to_string() != row.get::<String, _>("project")
        || budget.lineage.workspace_id.to_string() != row.get::<String, _>("workspace")
        || budget.lineage.task_id.to_string() != row.get::<String, _>("task")
    {
        return Err(refused());
    }
    let rows=sqlx::query("SELECT a.descriptor,a.body FROM profiled_launch_artifacts x JOIN admission_artifacts a USING(artifact_id) WHERE x.attempt_id=$1 AND a.descriptor->>'kind'='definition' AND a.descriptor#>>'{artifact,id}'=$2 AND a.descriptor#>>'{artifact,version}'=$3 ORDER BY a.artifact_id LIMIT 2 FOR SHARE OF a,x").bind(binding.attempt_id).bind(reference.id.as_str()).bind(reference.version.as_str()).fetch_all(&mut **tx).await.map_err(|_|AppError::Internal)?;
    if rows.len() != 1 {
        return Err(refused());
    }
    let descriptor: RetainedArtifact =
        serde_json::from_value(rows[0].get("descriptor")).map_err(|_| refused())?;
    let body: Vec<u8> = rows[0]
        .get::<Option<Vec<u8>>, _>("body")
        .ok_or_else(refused)?;
    if descriptor.artifact != reference {
        return Err(refused());
    }
    descriptor
        .verify(&body, descriptor.retained_at_ms)
        .map_err(|_| refused())?;
    decode(reference, &body, budget)
}
#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (ArtifactRef, Vec<u8>, BudgetSnapshot) {
        let value: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/selection-v2.json")).unwrap();
        let catalog = aq_profiles::ValidatedCatalog::from_artifact(
            value["catalog_artifact_json"].as_str().unwrap().as_bytes(),
        )
        .unwrap();
        let definition = &catalog.artifact().catalog.intelligence[0];
        let mut json = serde_json::to_value(definition).unwrap();
        json.sort_all_objects();
        let body = serde_json::to_vec(&json).unwrap();
        let source = catalog
            .source(
                "intelligence",
                &aq_profiles::key(&definition.id, definition.version),
            )
            .unwrap();
        let reference = ArtifactRef {
            id: aq_admission::Text::new(source.identity).unwrap(),
            version: aq_admission::Text::new(source.revision).unwrap(),
            digest: serde_json::from_value(serde_json::json!(source.digest)).unwrap(),
        };
        (
            reference,
            body,
            serde_json::from_value(value["inputs"]["budget"].clone()).unwrap(),
        )
    }
    #[test]
    fn actual_pinned_policy_keeps_known_elapsed_and_refuses_missing_or_exhausted() {
        let (reference, body, mut budget) = fixture();
        budget.elapsed_ms = Some(123);
        let loaded = decode(reference.clone(), &body, budget.clone()).unwrap();
        assert_eq!(loaded.1, loaded.0.max_lineage_elapsed_ms - 123);
        budget.elapsed_ms = None;
        assert!(decode(reference.clone(), &body, budget.clone()).is_err());
        budget.elapsed_ms = Some(loaded.0.max_lineage_elapsed_ms);
        assert!(decode(reference.clone(), &body, budget.clone()).is_err());
        budget.elapsed_ms = Some(u64::MAX);
        assert!(decode(reference, &body, budget).is_err());
    }
    #[test]
    fn mismatched_definition_and_unsupported_legacy_policy_never_get_defaults() {
        let (reference, body, budget) = fixture();
        let mut changed = reference.clone();
        changed.digest = aq_admission::Digest::of(b"different");
        assert!(decode(changed, &body, budget.clone()).is_err());
        let mut json: Value = serde_json::from_slice(&body).unwrap();
        json["selection_policy"] = Value::Null;
        let bytes = serde_json::to_vec(&json).unwrap();
        let mut legacy = reference.clone();
        legacy.digest = aq_admission::Digest::of(&bytes);
        assert!(decode(legacy, &bytes, budget.clone()).is_err());
        let mut wrong = reference;
        wrong.version = aq_admission::Text::new("999").unwrap();
        assert!(decode(wrong, &body, budget).is_err());
    }
}
