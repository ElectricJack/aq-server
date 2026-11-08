//! Actual qualified v2 inventory owner. No scoped report becomes placement data.
use super::{
    canonical, changed, now, private_inventory, qualification::Loaded, storage, unavailable,
};
use crate::{
    AppState, error::AppError, profiled_execution::registration, worker_api::AuthenticatedNode,
};
use aq_admission::scoped_presence::{InventoryIdentity, InventoryReceipt, InventoryV2};
use aq_selection::{CapabilitySnapshot, ExecutionTarget};
use sqlx::Row;
fn exact_request(a: &InventoryV2, b: &InventoryV2) -> Result<(), AppError> {
    if a.digest().map_err(|_| changed())? != b.digest().map_err(|_| changed())? {
        return Err(changed());
    }
    Ok(())
}
async fn replay(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    request: &InventoryV2,
) -> Result<Option<InventoryReceipt>, AppError> {
    if let Some(old) =
        storage::inventory(tx, request.identity.node_id, request.operation_id).await?
    {
        exact_request(&old.request, request)?;
        return Ok(Some(old.receipt));
    }
    let used: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM execution_receipts WHERE node_id=$1 AND operation_id=$2)",
    )
    .bind(request.identity.node_id)
    .bind(request.operation_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|_| AppError::Internal)?;
    if used {
        return Err(changed());
    }
    Ok(None)
}
pub async fn record(
    state: &AppState,
    auth: &AuthenticatedNode,
    request: &InventoryV2,
) -> Result<InventoryReceipt, AppError> {
    let reserved = state.presence_view();
    let state = &reserved;
    request
        .validate()
        .map_err(|_| AppError::BadRequest("invalid typed inventory".into()))?;
    if auth.id() != request.identity.node_id {
        return Err(changed());
    }
    let mut tx = super::begin(state).await?;
    crate::worker_api::lock_current_node(&mut tx, auth).await?;
    if let Some(old) = replay(&mut tx, request).await? {
        tx.commit().await.map_err(|_| unavailable())?;
        return Ok(old);
    }
    let runtime =
        registration::current_runtime_locked(state, &mut tx, auth.id(), request.identity.boot_id)
            .await?;
    let q = Loaded::load(state, &request.negotiation.qualification)?;
    q.matches_runtime(&runtime)?;
    if request.runtime != runtime.runtime
        || request.runtime_qualification != runtime.qualification
        || request.ttl_ms > q.body.maximum_ttl_ms
    {
        return Err(changed());
    }
    let admitted =
        registration::admitted_runtime_locked(&mut tx, auth.id(), request.identity.boot_id).await?;
    let delivery = state
        .config
        .profiled_execution
        .as_ref()
        .ok_or_else(changed)?
        .qualified(&admitted.request)
        .map_err(|_| changed())?
        .material_delivery
        .clone()
        .ok_or_else(changed)?;
    let registry = state.config.intelligence.clone().ok_or_else(changed)?;
    let mut harnesses = Vec::new();
    for reference in &request.qualifications {
        harnesses.push(
            registry
                .config
                .qualified_harnesses
                .iter()
                .find(|h| h.qualification == *reference)
                .cloned()
                .ok_or_else(changed)?,
        );
    }
    let enrolled = aq_machine_control::enrolled(&mut tx, auth.id()).await;
    let enrolled = enrolled.map_err(|_| unavailable())?;
    for profile in &request.provisionable_only {
        if !registry.config.credentials.iter().any(|c| {
            c.credential.profile == profile.profile
                && c.credential.revision == profile.revision
                && c.credential.enabled
                && c.delivery_mode.is_some()
                && c.admits(auth.id(), enrolled)
        }) {
            return Err(changed());
        }
    }
    tx.commit().await.map_err(|_| unavailable())?;
    let observed = now(state)?;
    let expires = observed
        .checked_add(request.ttl_ms)
        .filter(|t| *t <= i64::MAX as u64)
        .ok_or_else(unavailable)?;
    let mut bindings = private_inventory::observe(
        registry,
        auth.id(),
        request.identity.boot_id,
        delivery,
        observed,
        expires,
    )
    .await?;
    if !request.provisionable_only.is_empty() {
        bindings.retain(|b| {
            request
                .provisionable_only
                .iter()
                .any(|p| p.profile == b.credential_profile && p.revision == b.credential_revision)
        });
        if bindings.len() != request.provisionable_only.len() {
            return Err(changed());
        }
    }
    if bindings
        .iter()
        .any(|b| b.available || b.preparation.is_none())
    {
        return Err(changed());
    }
    let mut tx = super::begin(state).await?;
    crate::worker_api::lock_current_node(&mut tx, auth).await?;
    if let Some(old) = replay(&mut tx, request).await? {
        tx.commit().await.map_err(|_| unavailable())?;
        return Ok(old);
    }
    let runtime =
        registration::current_runtime_locked(state, &mut tx, auth.id(), request.identity.boot_id)
            .await?;
    q.matches_runtime(&runtime)?;
    let current = Loaded::load(state, &request.negotiation.qualification)?;
    if current.reference != q.reference {
        return Err(changed());
    }
    crate::intelligence::fence_current_registry(state, &mut tx).await?;
    let old=sqlx::query("SELECT boot_id,sequence,revision FROM intelligence_node_snapshots WHERE node_id=$1 FOR UPDATE").bind(auth.id()).fetch_optional(&mut *tx).await.map_err(|_|AppError::Internal)?;
    let revision = if let Some(old) = old {
        if old.get::<uuid::Uuid, _>("boot_id") == request.identity.boot_id
            && old.get::<i64, _>("sequence") as u64 >= request.sequence
        {
            return Err(changed());
        }
        old.get::<i64, _>("revision")
            .checked_add(1)
            .ok_or_else(unavailable)? as u64
    } else {
        1
    };
    let snapshot = CapabilitySnapshot {
        backend_kind: None,
        target: ExecutionTarget::Worker {
            node_id: auth.id(),
            boot_id: request.identity.boot_id,
        },
        revision,
        observed_at_ms: observed,
        expires_at_ms: expires,
        harnesses,
        credential_bindings: bindings,
        material_delivery: Some(
            state
                .config
                .profiled_execution
                .as_ref()
                .ok_or_else(changed)?
                .qualified(&admitted.request)
                .map_err(|_| changed())?
                .material_delivery
                .clone()
                .ok_or_else(changed)?,
        ),
    };
    let epoch = storage::expected_epoch(&mut tx, request, &snapshot).await?;
    let bytes = canonical(&snapshot)?;
    if bytes.len() > 131072 {
        return Err(AppError::PayloadTooLarge(
            "normalized inventory bound".into(),
        ));
    }
    let digest = aq_admission::Digest::of(&bytes);
    let value = serde_json::to_value(&snapshot).map_err(|_| AppError::Internal)?;
    sqlx::query("INSERT INTO intelligence_node_snapshots(node_id,boot_id,sequence,revision,snapshot) VALUES($1,$2,$3,$4,$5) ON CONFLICT(node_id) DO UPDATE SET boot_id=excluded.boot_id,sequence=excluded.sequence,revision=excluded.revision,snapshot=excluded.snapshot").bind(auth.id()).bind(request.identity.boot_id).bind(request.sequence as i64).bind(revision as i64).bind(&value).execute(&mut *tx).await.map_err(|_|AppError::Internal)?;
    let recorded = now(state)?;
    if recorded < observed
        || recorded >= observed.checked_add(2000).ok_or_else(unavailable)?
        || recorded >= expires
    {
        return Err(unavailable());
    }
    let receipt = InventoryReceipt {
        schema_version: 1,
        identity: request.identity.clone(),
        negotiation: request.negotiation.clone(),
        inventory: InventoryIdentity {
            operation_id: request.operation_id,
            sequence: request.sequence,
            revision,
            request_digest: request.digest().map_err(|_| changed())?,
            snapshot_digest: digest,
        },
        recorded_at_ms: recorded,
        expires_at_ms: expires,
    };
    storage::retain_inventory(&mut tx, request, &receipt, epoch).await?;
    sqlx::query("INSERT INTO execution_receipts(node_id,operation_id,kind,payload_digest,response) VALUES($1,$2,'scoped_inventory_v2',$3,$4)").bind(auth.id()).bind(request.operation_id).bind(request.digest().map_err(|_|changed())?.as_str()).bind(serde_json::to_value(&receipt).map_err(|_|AppError::Internal)?).execute(&mut *tx).await.map_err(|_|AppError::Internal)?;
    let final_time = now(state)?;
    if final_time < recorded
        || final_time >= observed.checked_add(2000).ok_or_else(unavailable)?
        || final_time >= expires
    {
        return Err(unavailable());
    }
    tx.commit().await.map_err(|_| unavailable())?;
    Ok(receipt)
}
