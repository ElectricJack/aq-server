//! Immutable pinned policy/budget inputs, not current admission or authorization.
//! Callers hold current node/attempt ownership and separately fence live facts.
use crate::error::AppError;
use aq_admission::{ArtifactRef, LaunchBinding, RetainedArtifact};
use aq_profiles::SelectionPolicy;
use aq_selection::BudgetSnapshot;
use serde_json::Value;
use sqlx::{Postgres, Row, Transaction};
#[track_caller]
fn refused() -> AppError {
    tracing::debug!(target: "aq_refusal", at = %std::panic::Location::caller(), "profiled refusal");
    AppError::Conflict("pinned native policy budget unavailable or exhausted".into())
}
fn decode(reference: ArtifactRef, body: &[u8]) -> Result<SelectionPolicy, AppError> {
    let policy = policy_of(&reference, body)?;
    policy.validate().map_err(|_| refused())?;
    Ok(policy)
}
fn policy_of(reference: &ArtifactRef, body: &[u8]) -> Result<SelectionPolicy, AppError> {
    if body.is_empty()
        || body.len() > 2_097_152
        || aq_admission::Digest::of(body) != reference.digest
    {
        return Err(refused());
    }
    // The level as the attempt's offer retained it: the document and the
    // selection policy in effect for the launch (the defaults of its
    // snapshot, the level's own keys and the execution profile's limits).
    #[derive(serde::Deserialize)]
    struct PinnedLevel {
        id: String,
        selection_policy: SelectionPolicy,
    }
    // An attempt offered before the lineage time budgets were removed pinned a
    // policy that still has their keys; they are left out of what is read.
    let mut pinned: Value = serde_json::from_slice(body).map_err(|_| refused())?;
    if let Some(policy) = pinned.get_mut("selection_policy") {
        aq_profiles::roles::drop_removed_keys(policy);
    }
    let level: PinnedLevel = serde_json::from_value(pinned).map_err(|_| refused())?;
    if reference.id.as_str() != format!("intelligence:{}", level.id) {
        return Err(refused());
    }
    Ok(level.selection_policy)
}
/// The attempt's one retained definition body for `reference`, verified.
async fn pinned_body(
    conn: &mut sqlx::PgConnection,
    attempt: uuid::Uuid,
    reference: &ArtifactRef,
    lock: &str,
) -> Result<Vec<u8>, AppError> {
    let rows=sqlx::query(&format!("SELECT a.descriptor,a.body FROM profiled_launch_artifacts x JOIN admission_artifacts a USING(artifact_id) WHERE x.attempt_id=$1 AND a.descriptor->>'kind'='definition' AND a.descriptor#>>'{{artifact,id}}'=$2 AND a.descriptor#>>'{{artifact,version}}'=$3 ORDER BY a.artifact_id LIMIT 2{lock}")).bind(attempt).bind(reference.id.as_str()).bind(reference.version.as_str()).fetch_all(&mut *conn).await.map_err(|_|AppError::Internal)?;
    if rows.len() != 1 {
        return Err(refused());
    }
    let descriptor: RetainedArtifact =
        serde_json::from_value(rows[0].get("descriptor")).map_err(|_| refused())?;
    let body: Vec<u8> = rows[0]
        .get::<Option<Vec<u8>>, _>("body")
        .ok_or_else(|| refused())?;
    if descriptor.artifact != *reference {
        return Err(refused());
    }
    descriptor
        .verify(&body, descriptor.retained_at_ms)
        .map_err(|_| refused())?;
    Ok(body)
}
/// The selection policy the attempt's launch pinned; `None` when it cannot be read (spec 0007
/// Q1). Its limits are the task's retry budget (`replacement::limits`).
pub(crate) async fn pinned_policy(
    conn: &mut sqlx::PgConnection,
    attempt: uuid::Uuid,
) -> Result<Option<SelectionPolicy>, AppError> {
    let reference: Option<Value> = sqlx::query_scalar(
        "SELECT launch_manifest->'intelligence_profile' FROM profiled_launches WHERE attempt_id=$1",
    )
    .bind(attempt)
    .fetch_optional(&mut *conn)
    .await
    .map_err(|_| AppError::Internal)?;
    let Some(Ok(reference)) = reference.map(serde_json::from_value::<ArtifactRef>) else {
        return Ok(None);
    };
    let Ok(body) = pinned_body(conn, attempt, &reference, "").await else {
        return Ok(None);
    };
    Ok(policy_of(&reference, &body).ok())
}
pub(crate) async fn load_locked(
    tx: &mut Transaction<'_, Postgres>,
    binding: &LaunchBinding,
) -> Result<SelectionPolicy, AppError> {
    let row=sqlx::query("SELECT l.launch_binding,l.launch_manifest->'intelligence_profile' AS policy_ref,l.launch_manifest#>>'{bindings,project_id}' AS project,l.launch_manifest#>>'{bindings,graph_id}' AS workspace,l.launch_manifest#>>'{bindings,task_id}' AS task,d.decision#>'{inputs,budget}' AS budget FROM profiled_launches l JOIN intelligence_decisions d ON d.decision_id=l.decision_id WHERE l.attempt_id=$1 FOR SHARE OF l,d").bind(binding.attempt_id).fetch_optional(&mut **tx).await.map_err(|_|AppError::Internal)?.ok_or_else(|| refused())?;
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
    let body = pinned_body(
        &mut *tx,
        binding.attempt_id,
        &reference,
        " FOR SHARE OF a,x",
    )
    .await?;
    decode(reference, &body)
}
#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (ArtifactRef, Vec<u8>) {
        // The level as an offer retains it: its id and the selection policy
        // in effect, under the identity `intelligence:<id>`.
        let id = "standard-high";
        let policy = aq_profiles::roles::SelectionDefaults::default().policy(4096, Vec::new());
        let mut json = serde_json::json!({"id": id, "selection_policy": policy});
        json.sort_all_objects();
        let body = serde_json::to_vec(&json).unwrap();
        let reference = ArtifactRef {
            id: aq_admission::Text::new(format!("intelligence:{id}")).unwrap(),
            version: aq_admission::Text::new("1").unwrap(),
            digest: aq_admission::Digest::of(&body),
        };
        (reference, body)
    }
    #[test]
    fn a_policy_pinned_before_the_time_budgets_were_removed_still_loads_without_them() {
        let (reference, body) = fixture();
        let loaded = decode(reference, &body).unwrap();
        let mut json: Value = serde_json::from_slice(&body).unwrap();
        // What an attempt offered before spec 0021 retained.
        json["selection_policy"]["max_lineage_elapsed_ms"] = 3_600_000.into();
        json["selection_policy"]["max_lineage_sandbox_ms"] = 14_400_000.into();
        json.sort_all_objects();
        let bytes = serde_json::to_vec(&json).unwrap();
        let legacy = ArtifactRef {
            digest: aq_admission::Digest::of(&bytes),
            ..fixture().0
        };
        assert_eq!(decode(legacy, &bytes).unwrap(), loaded);
    }
    #[test]
    fn mismatched_definition_and_unsupported_legacy_policy_never_get_defaults() {
        let (reference, body) = fixture();
        let mut changed = reference.clone();
        changed.digest = aq_admission::Digest::of(b"different");
        assert!(decode(changed, &body).is_err());
        let mut json: Value = serde_json::from_slice(&body).unwrap();
        json["selection_policy"] = Value::Null;
        let bytes = serde_json::to_vec(&json).unwrap();
        let mut legacy = reference.clone();
        legacy.digest = aq_admission::Digest::of(&bytes);
        assert!(decode(legacy, &bytes).is_err());
        // A reference that names another level than the body does.
        let mut wrong = reference;
        wrong.id = aq_admission::Text::new("intelligence:another").unwrap();
        assert!(decode(wrong, &body).is_err());
    }
}
