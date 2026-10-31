//! Native metadata is admitted beside the common incarnation identity. It is
//! never reconstructed from a config allowlist or a legacy fixture capability.
use crate::{AppState, error::AppError};
use aq_admission::{
    ArtifactRef, Digest,
    transport::{Register, TransportDocument},
};
use serde::{Deserialize, Serialize};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeRegistration {
    pub schema_version: u32,
    pub request: Register,
    pub semantic_digest: Digest,
    pub qualification: ArtifactRef,
    pub configuration_revision: u64,
    pub registered_at_ms: u64,
}
#[derive(Debug, Clone)]
pub struct RegisteredRuntime {
    pub runtime: ArtifactRef,
    pub qualification: ArtifactRef,
    pub boot_id: Uuid,
}
pub fn semantic_digest(request: &Register) -> Result<Digest, AppError> {
    let mut value = serde_json::to_value(request).map_err(|_| AppError::Internal)?;
    value
        .as_object_mut()
        .ok_or(AppError::Internal)?
        .remove("operation_id");
    aq_selection::digest(&value).map_err(|_| AppError::Internal)
}
/// Caller already holds current node lock. This reads the actual admitted
/// metadata plus common identity; missing metadata always refuses. All current
/// machine authorization/credential checks remain the caller's responsibility.
pub async fn current_runtime_locked(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    boot: Uuid,
) -> Result<RegisteredRuntime, AppError> {
    let metadata = admitted_runtime_locked(tx, node, boot).await?;
    let config = state
        .config
        .profiled_execution
        .as_deref()
        .ok_or_else(|| AppError::Conflict("native policy is not configured".into()))?;
    super::policies::fence_configuration_locked(tx, config).await?;
    let qualified = config
        .qualified(&metadata.request)
        .map_err(|_| AppError::Conflict("native runtime qualification unavailable".into()))?;
    if metadata.configuration_revision > config.revision
        || metadata.qualification != qualified.qualification
    {
        return Err(AppError::Conflict(
            "native runtime qualification changed".into(),
        ));
    }
    Ok(RegisteredRuntime {
        runtime: metadata.request.runtime,
        qualification: metadata.qualification,
        boot_id: boot,
    })
}

/// Recover the originally admitted runtime without granting new launch rights.
/// Caller must hold the current authenticated node lock; retirement of a launch
/// allowlist must not prevent recording this same owner's actual stop evidence.
pub(super) async fn admitted_runtime_locked(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    boot: Uuid,
) -> Result<NativeRegistration, AppError> {
    let record: serde_json::Value =
        sqlx::query_scalar("SELECT state FROM execution_nodes WHERE node_id=$1 FOR SHARE")
            .bind(node)
            .fetch_optional(&mut **tx)
            .await
            .map_err(|_| AppError::Internal)?
            .ok_or_else(|| AppError::Conflict("native runtime is not registered".into()))?;
    let metadata: NativeRegistration = serde_json::from_value(
        record
            .get("native")
            .cloned()
            .ok_or_else(|| AppError::Conflict("native runtime is not registered".into()))?,
    )
    .map_err(|_| AppError::Conflict("invalid native registration".into()))?;
    TransportDocument::Register(metadata.request.clone())
        .validate()
        .map_err(|_| AppError::Conflict("invalid native registration".into()))?;
    if metadata.schema_version != 1
        || metadata.configuration_revision == 0
        || metadata.registered_at_ms == 0
        || metadata.request.identity.node_id != node
        || metadata.request.identity.boot_id != boot
        || record["identity"]
            != serde_json::to_value(&metadata.request.identity).map_err(|_| AppError::Internal)?
        || metadata.semantic_digest != semantic_digest(&metadata.request)?
    {
        return Err(AppError::Conflict(
            "native registration scope mismatch".into(),
        ));
    }
    Ok(metadata)
}

/// Admit an exact qualified runtime under the authenticated machine's current
/// credential lock. The returned Register is an acknowledgment only: it grants
/// no execution, source access, or terminal capability.
pub async fn register(
    state: &AppState,
    auth: &crate::worker_api::AuthenticatedNode,
    request: &Register,
) -> Result<Register, AppError> {
    use sqlx::Row;
    TransportDocument::Register(request.clone())
        .validate()
        .map_err(|_| AppError::BadRequest("invalid native registration".into()))?;
    if request.identity.node_id != auth.id() {
        return Err(AppError::Forbidden("registration node mismatch".into()));
    }
    if serde_json::to_vec(request)
        .map_err(|_| AppError::Internal)?
        .len()
        > 32_768
    {
        return Err(AppError::PayloadTooLarge(
            "native registration exceeds receipt bound".into(),
        ));
    }
    let digest = aq_selection::digest(request).map_err(|_| AppError::Internal)?;
    let semantic = semantic_digest(request)?;
    let mut tx = state
        .store
        .pool
        .begin()
        .await
        .map_err(|_| AppError::Internal)?;
    crate::worker_api::lock_current_node(&mut tx, auth).await?;
    if let Some(row) = sqlx::query("SELECT request_digest,response FROM native_registration_receipts WHERE node_id=$1 AND operation_id=$2")
        .bind(auth.id()).bind(request.operation_id).fetch_optional(&mut *tx).await.map_err(|_| AppError::Internal)? {
        if row.get::<String,_>("request_digest") != digest.as_str() {
            return Err(AppError::Conflict("native operation ID reused".into()));
        }
        return serde_json::from_value(row.get("response")).map_err(|_| AppError::Internal);
    }
    let config = state
        .config
        .profiled_execution
        .as_deref()
        .ok_or_else(|| AppError::Conflict("native policy is not configured".into()))?;
    super::policies::fence_configuration_locked(&mut tx, config).await?;
    let qualification = config
        .qualified(request)
        .map_err(|_| AppError::Conflict("native runtime qualification unavailable".into()))?
        .qualification
        .clone();
    let existing: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT state FROM execution_nodes WHERE node_id=$1 FOR UPDATE")
            .bind(auth.id())
            .fetch_optional(&mut *tx)
            .await
            .map_err(|_| AppError::Internal)?;
    let unchanged = existing
        .as_ref()
        .and_then(|v| v.get("native"))
        .and_then(|v| serde_json::from_value::<NativeRegistration>(v.clone()).ok())
        .is_some_and(|v| {
            v.schema_version == 1
                && v.configuration_revision > 0
                && v.registered_at_ms > 0
                && v.semantic_digest == semantic
                && v.qualification == qualification
                && existing.as_ref().and_then(|v| v.get("identity"))
                    == serde_json::to_value(&request.identity).ok().as_ref()
        });
    if existing.is_some() && !unchanged {
        let held: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM execution_attempts WHERE node_id=$1 AND holds_task)",
        )
        .bind(auth.id())
        .fetch_one(&mut *tx)
        .await
        .map_err(|_| AppError::Internal)?;
        if held {
            return Err(AppError::Conflict(
                "prior execution claims remain held".into(),
            ));
        }
    }
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM native_registration_receipts WHERE node_id=$1")
            .bind(auth.id())
            .fetch_one(&mut *tx)
            .await
            .map_err(|_| AppError::Internal)?;
    if count >= 1024 {
        return Err(AppError::Conflict(
            "native registration receipt capacity exhausted".into(),
        ));
    }
    if !unchanged {
        let now = (state.clock)().timestamp_millis();
        if now <= 0 {
            return Err(AppError::Conflict("registration clock unavailable".into()));
        }
        let metadata = NativeRegistration {
            schema_version: 1,
            request: request.clone(),
            semantic_digest: semantic,
            qualification,
            configuration_revision: config.revision,
            registered_at_ms: now as u64,
        };
        let body = serde_json::json!({"identity":request.identity,"capabilities":[],"worker_version":request.runtime.version,"replacement_boot":null,"native":metadata});
        sqlx::query("INSERT INTO execution_nodes(node_id,state) VALUES($1,$2) ON CONFLICT(node_id) DO UPDATE SET state=excluded.state")
            .bind(auth.id()).bind(body).execute(&mut *tx).await.map_err(|_|AppError::Internal)?;
    }
    sqlx::query("INSERT INTO native_registration_receipts(node_id,operation_id,request_digest,response) VALUES($1,$2,$3,$4)")
        .bind(auth.id()).bind(request.operation_id).bind(digest.as_str()).bind(serde_json::to_value(request).map_err(|_|AppError::Internal)?)
        .execute(&mut *tx).await.map_err(|_|AppError::Internal)?;
    tx.commit().await.map_err(|_| AppError::Internal)?;
    Ok(request.clone())
}
