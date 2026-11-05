//! Native dispatch intent survives both response loss and process restarts.
use super::*;
use sqlx::Row;

pub(super) struct Intent {
    pub project: Uuid,
    pub revision: u64,
    pub binding: Value,
    pub workspace: Uuid,
    pub service: Value,
    pub body: Vec<u8>,
    pub operations: usize,
}

pub(super) async fn retain(
    state: &AppState,
    user: &User,
    id: Uuid,
    record: &ProjectRecord,
    service: &Value,
    body: &[u8],
) -> Result<()> {
    // An ambiguous insert is never a send license. A committed intent can be
    // recovered by lookup; no intent means no native effect was dispatched.
    let inserted = sqlx::query(
        "INSERT INTO dashboard_graph_intents(operation_id,project_id,project_revision,project_binding,workspace_id,service_identity,idempotency_key,body)
         SELECT operation_id,$3,$4,$5,$6,$7,$8,$9 FROM dashboard_operations
         WHERE operation_id=$1 AND principal_id=$2 AND disposition='pending' AND retired_at IS NULL",
    )
    .bind(id)
    .bind(user.id)
    .bind(record.binding.project_id)
    .bind(record.revision as i64)
    .bind(json!(record.binding))
    .bind(record.binding.graph_workspace_id)
    .bind(service)
    .bind(format!("aq-dashboard-action:{id}"))
    .bind(body)
    .execute(&state.store.pool)
    .await
    .map_err(|_| unavailable())?
    .rows_affected();
    if inserted != 1 {
        return Err(unavailable());
    }
    Ok(())
}

pub(super) async fn load(
    state: &AppState,
    user: &User,
    id: Uuid,
    kind: &str,
) -> Result<Option<Intent>> {
    let Some(row) = sqlx::query(
        "SELECT i.* FROM dashboard_graph_intents i JOIN dashboard_operations d USING(operation_id)
         WHERE i.operation_id=$1 AND d.principal_id=$2 AND d.kind=$3 AND d.retired_at IS NULL",
    )
    .bind(id)
    .bind(user.id)
    .bind(kind)
    .fetch_optional(&state.store.pool)
    .await
    .map_err(|_| unavailable())?
    else {
        return Ok(None);
    };
    let body: Vec<u8> = row.get("body");
    let raw = super::super::super::scheduling::parse::parse(&body).map_err(|_| unavailable())?;
    let key: String = row.get("idempotency_key");
    if key != format!("aq-dashboard-action:{id}")
        || raw["idempotency_key"] != key
        || !contract::upstream_valid("MutationRequest", &raw)
    {
        return Err(unavailable());
    }
    Ok(Some(Intent {
        project: row.get("project_id"),
        revision: row.get::<i64, _>("project_revision") as u64,
        binding: row.get("project_binding"),
        workspace: row.get("workspace_id"),
        service: row.get("service_identity"),
        body,
        operations: raw["operations"].as_array().ok_or_else(unavailable)?.len(),
    }))
}
