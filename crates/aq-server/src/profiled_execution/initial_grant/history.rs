//! Exact historical initial authority only. Call while holding the current node
//! and exact attempt/hot locks; this reader grants no current launch permission.
use super::{StoredInitialGrant, db, refused};
use crate::error::AppError;
use aq_admission::{Digest, LaunchBinding, control::Versions, transport::GrantedLaunch};
use aq_session_protocol::Validate;
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;
pub(crate) struct OriginalGrant {
    pub operation_id: Uuid,
    pub request_digest: Digest,
    pub body: GrantedLaunch,
    pub versions: Versions,
    pub execution_deadline_ms: u64,
}
pub(crate) async fn load_original_grant_locked(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    binding: &LaunchBinding,
    grant_id: Uuid,
) -> Result<OriginalGrant, AppError> {
    binding.validate().map_err(|_| refused())?;
    if node.is_nil() || grant_id.is_nil() {
        return Err(refused());
    }
    let rows=sqlx::query("SELECT operation_id,request_digest,response FROM profiled_execution_receipts WHERE node_id=$1 AND kind='prepare' AND response->>'kind'='grant' AND response#>>'{body,grant,binding,attempt_id}'=$2 AND response#>>'{body,grant,grant_id}'=$3 ORDER BY operation_id LIMIT 2 FOR SHARE")
        .bind(node).bind(binding.attempt_id.to_string()).bind(grant_id.to_string()).fetch_all(&mut **tx).await.map_err(db)?;
    if rows.len() != 1 {
        return Err(refused());
    }
    let row = &rows[0];
    let original = StoredInitialGrant::decode(row.get("response"))?;
    if original.body.grant.binding != *binding || original.body.grant.grant_id != grant_id {
        return Err(refused());
    }
    let request_digest: Digest =
        serde_json::from_value(serde_json::Value::String(row.get("request_digest")))
            .map_err(|_| refused())?;
    Ok(OriginalGrant {
        operation_id: row.get("operation_id"),
        request_digest,
        body: original.body,
        versions: original.versions,
        execution_deadline_ms: original.execution_deadline_ms,
    })
}
