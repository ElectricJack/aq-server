//! B6a2 projections of actual retained owners. A finished runner report is not
//! Graph completion, task acceptance, claim release, or fresh execution authority.
use crate::{AppState, error::AppError, worker_api::AuthenticatedNode};
use aq_admission::{Digest, result_record::Receipt, transport::ResultReport};
use axum::{
    Json,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const HEADER: &str = "aq-runner-receipts";
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttemptKey {
    pub launch_id: Uuid,
    pub attempt_id: Uuid,
    pub manifest_digest: Digest,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CentralReceipt {
    pub receipt_id: Uuid,
    pub attempt: AttemptKey,
    pub artifact_digest: Digest,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinishedReceipt {
    pub receipt_id: Uuid,
    pub attempt: AttemptKey,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub schema_version: u32,
    pub source: Option<CentralReceipt>,
    pub result: Option<CentralReceipt>,
    pub finished: Option<FinishedReceipt>,
}
fn attach(body: impl Serialize, envelope: Option<Envelope>) -> Result<Response, AppError> {
    let mut response = Json(body).into_response();
    response
        .headers_mut()
        .insert("cache-control", "no-store".parse().unwrap());
    if let Some(envelope) = envelope {
        let value = serde_json::to_string(&envelope).map_err(|_| AppError::Internal)?;
        if value.len() > 8192 {
            return Err(AppError::Internal);
        }
        response
            .headers_mut()
            .insert(HEADER, value.parse().map_err(|_| AppError::Internal)?);
    }
    Ok(response)
}
pub(crate) fn result(
    node: &AuthenticatedNode,
    request: &ResultReport,
    receipt: Receipt,
) -> Result<Response, AppError> {
    receipt
        .validate_for(request)
        .map_err(|_| AppError::Internal)?;
    let envelope = node.delegated().map(|_| {
        let attempt = AttemptKey {
            launch_id: request.stop.launch_id,
            attempt_id: receipt.binding.attempt_id,
            manifest_digest: receipt.binding.launch_manifest_digest.clone(),
        };
        Envelope {
            schema_version: 1,
            source: None,
            result: Some(CentralReceipt {
                receipt_id: receipt.report_id,
                attempt: attempt.clone(),
                artifact_digest: receipt.report_digest.clone(),
            }),
            finished: Some(FinishedReceipt {
                receipt_id: receipt.report_id,
                attempt,
            }),
        }
    });
    attach(receipt, envelope)
}
pub(crate) async fn source(
    state: &AppState,
    node: &AuthenticatedNode,
    status: aq_worker_protocol::TransferStatus,
) -> Result<Response, AppError> {
    let envelope = if let (Some(delegate), Some(receipt)) = (node.delegated(), &status.receipt) {
        let mut tx = state.store.pool.begin().await.map_err(super::runtime::db)?;
        sqlx::query("SET LOCAL lock_timeout = '2s'")
            .execute(&mut *tx)
            .await
            .map_err(super::runtime::db)?;
        sqlx::query("SET LOCAL statement_timeout = '3s'")
            .execute(&mut *tx)
            .await
            .map_err(super::runtime::db)?;
        crate::worker_api::lock_current_node(&mut tx, node).await?;
        let original = delegate
            .current
            .authority
            .as_ref()
            .ok_or_else(super::runtime::refused)?;
        let bytes: Vec<u8> = sqlx::query_scalar("SELECT request_bytes FROM profiled_started_receipts WHERE attempt_id=$1 AND node_id=$2")
            .bind(delegate.current.record.binding.attempt_id).bind(node.id())
            .fetch_one(&mut *tx).await.map_err(super::runtime::db)?;
        let started =
            aq_admission::started::decode_request(&bytes).map_err(|_| AppError::Internal)?;
        if started.binding != original.grant.binding || started.grant_id != original.grant.grant_id
        {
            return Err(super::runtime::refused());
        }
        let artifact_digest =
            serde_json::from_value(serde_json::Value::String(receipt.manifest_digest.clone()))
                .map_err(|_| AppError::Internal)?;
        tx.commit().await.map_err(super::runtime::db)?;
        Some(Envelope {
            schema_version: 1,
            source: Some(CentralReceipt {
                receipt_id: receipt.receipt_id,
                attempt: AttemptKey {
                    launch_id: started.launch_id,
                    attempt_id: started.binding.attempt_id,
                    manifest_digest: started.binding.launch_manifest_digest,
                },
                artifact_digest,
            }),
            result: None,
            finished: None,
        })
    } else {
        None
    };
    attach(status, envelope)
}
