//! Actual immutable operation retention and semantic/negative fences.
use super::{canonical, changed, unavailable};
use crate::error::AppError;
use aq_admission::{
    Digest,
    scoped_presence::{InventoryReceipt, InventoryV2, decode_inventory},
};
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;
#[derive(Clone)]
pub(super) struct Inventory {
    pub request: InventoryV2,
    pub receipt: InventoryReceipt,
    pub epoch: u64,
}
#[derive(Clone)]
pub(super) struct Head {
    pub boot: Uuid,
    pub mode: String,
    pub inventory_revision: u64,
    pub snapshot_digest: Digest,
    pub operation: Option<Uuid>,
    pub epoch: u64,
    pub binding_set: Option<serde_json::Value>,
}
/// Lock and statement timeouts are retryable (503); anything else is a logged 500.
pub(super) fn db(e: sqlx::Error) -> AppError {
    if e.as_database_error().is_some_and(|e| {
        matches!(
            e.code().as_deref(),
            Some("54000" | "55P03" | "57014" | "40001")
        )
    }) {
        unavailable()
    } else {
        tracing::warn!(error = %e, "scoped presence store error");
        AppError::Internal
    }
}
pub async fn inventory(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    operation: Uuid,
) -> Result<Option<Inventory>, AppError> {
    let row=sqlx::query("SELECT kind,request_bytes,request_digest,receipt_bytes,compatibility_epoch FROM profiled_presence_operations WHERE node_id=$1 AND operation_id=$2 FOR SHARE").bind(node).bind(operation).fetch_optional(&mut **tx).await.map_err(db)?;
    let Some(row) = row else { return Ok(None) };
    if row.get::<String, _>("kind") != "inventory_v2" {
        return Err(changed());
    }
    let request =
        decode_inventory(&row.get::<Vec<u8>, _>("request_bytes")).map_err(|_| changed())?;
    let receipt: InventoryReceipt =
        serde_json::from_slice(&row.get::<Vec<u8>, _>("receipt_bytes")).map_err(|_| changed())?;
    receipt.validate_for(&request).map_err(|_| changed())?;
    if request.identity.node_id != node
        || request.operation_id != operation
        || request.digest().map_err(|_| changed())?.as_str()
            != row.get::<String, _>("request_digest")
    {
        return Err(changed());
    }
    Ok(Some(Inventory {
        request,
        receipt,
        epoch: row.get::<i64, _>("compatibility_epoch") as u64,
    }))
}
pub async fn head(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
) -> Result<Option<Head>, AppError> {
    let row=sqlx::query("SELECT boot_id,mode,inventory_revision,snapshot_digest,operation_id,compatibility_epoch,provisionable_binding_set FROM profiled_inventory_semantic_heads WHERE node_id=$1 FOR UPDATE").bind(node).fetch_optional(&mut **tx).await.map_err(db)?;
    row.map(|r| {
        Ok(Head {
            boot: r.get("boot_id"),
            mode: r.get("mode"),
            inventory_revision: r.get::<i64, _>("inventory_revision") as u64,
            snapshot_digest: serde_json::from_value(serde_json::Value::String(
                r.get("snapshot_digest"),
            ))
            .map_err(|_| changed())?,
            operation: r.get("operation_id"),
            epoch: r.get::<i64, _>("compatibility_epoch") as u64,
            binding_set: r.get("provisionable_binding_set"),
        })
    })
    .transpose()
}
pub async fn expected_epoch(
    tx: &mut Transaction<'_, Postgres>,
    request: &InventoryV2,
    snapshot: &aq_selection::CapabilitySnapshot,
) -> Result<u64, AppError> {
    let Some(old) = head(tx, request.identity.node_id).await? else {
        return Ok(1);
    };
    let compatible = if old.boot == request.identity.boot_id && old.mode == "scoped_v1" {
        let previous = inventory(
            tx,
            request.identity.node_id,
            old.operation.ok_or_else(changed)?,
        )
        .await?
        .ok_or_else(changed)?;
        if previous.epoch != old.epoch
            || previous.receipt.inventory.revision != old.inventory_revision
            || previous.receipt.inventory.snapshot_digest != old.snapshot_digest
        {
            return Err(changed());
        }
        let binding_set: serde_json::Value =
            sqlx::query_scalar("SELECT profiled_provisionable_binding_set($1)")
                .bind(serde_json::to_value(snapshot).map_err(|_| changed())?)
                .fetch_one(&mut **tx)
                .await
                .map_err(db)?;
        old.binding_set.as_ref() == Some(&binding_set)
            && previous.request.runtime == request.runtime
            && previous.request.runtime_qualification == request.runtime_qualification
            && previous.request.negotiation.qualification == request.negotiation.qualification
            && previous.request.negotiation.protocol == request.negotiation.protocol
    } else {
        false
    };
    if compatible {
        Ok(old.epoch)
    } else {
        old.epoch
            .checked_add(1)
            .filter(|n| *n <= i64::MAX as u64)
            .ok_or_else(unavailable)
    }
}
pub async fn retain_inventory(
    tx: &mut Transaction<'_, Postgres>,
    request: &InventoryV2,
    receipt: &InventoryReceipt,
    epoch: u64,
) -> Result<(), AppError> {
    receipt.validate_for(request).map_err(|_| changed())?;
    let req = canonical(request)?;
    let ack = canonical(receipt)?;
    sqlx::query("INSERT INTO profiled_presence_operations(node_id,operation_id,kind,boot_id,sequence,inventory_revision,compatibility_epoch,snapshot_digest,request_bytes,request_digest,receipt_bytes,receipt_digest,recorded_at_ms,expires_at_ms) VALUES($1,$2,'inventory_v2',$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)").bind(request.identity.node_id).bind(request.operation_id).bind(request.identity.boot_id).bind(request.sequence as i64).bind(receipt.inventory.revision as i64).bind(epoch as i64).bind(receipt.inventory.snapshot_digest.as_str()).bind(&req).bind(request.digest().map_err(|_|changed())?.as_str()).bind(&ack).bind(Digest::of(&ack).as_str()).bind(receipt.recorded_at_ms as i64).bind(receipt.expires_at_ms as i64).execute(&mut **tx).await.map_err(db)?;
    let updated=sqlx::query("UPDATE profiled_inventory_semantic_heads SET boot_id=$2,mode='scoped_v1',inventory_revision=$3,snapshot_digest=$4,operation_id=$5,compatibility_epoch=$6 WHERE node_id=$1").bind(request.identity.node_id).bind(request.identity.boot_id).bind(receipt.inventory.revision as i64).bind(receipt.inventory.snapshot_digest.as_str()).bind(request.operation_id).bind(epoch as i64).execute(&mut **tx).await.map_err(db)?.rows_affected();
    if updated == 0 {
        sqlx::query("INSERT INTO profiled_inventory_semantic_heads(node_id,boot_id,mode,inventory_revision,snapshot_digest,operation_id,compatibility_epoch,revision) VALUES($1,$2,'scoped_v1',$3,$4,$5,$6,1)").bind(request.identity.node_id).bind(request.identity.boot_id).bind(receipt.inventory.revision as i64).bind(receipt.inventory.snapshot_digest.as_str()).bind(request.operation_id).bind(epoch as i64).execute(&mut **tx).await.map_err(db)?;
    }
    Ok(())
}
/// Called only for a NEW accepted legacy inventory under the same node lock.
/// Exact legacy receipt replay must bypass this call and preserve the barrier.
pub(crate) async fn legacy(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    boot: Uuid,
    revision: u64,
    snapshot: &serde_json::Value,
) -> Result<(), AppError> {
    sqlx::query("UPDATE profiled_inventory_semantic_heads SET boot_id=$2,mode='legacy',inventory_revision=$3,snapshot_digest=$4,operation_id=NULL WHERE node_id=$1").bind(node).bind(boot).bind(revision as i64).bind(aq_selection::digest(snapshot).map_err(|_|AppError::Internal)?.as_str()).execute(&mut **tx).await.map_err(db)?;
    Ok(())
}

#[derive(Clone)]
pub(super) struct Fact {
    pub request: aq_admission::scoped_presence::PresenceReport,
    pub receipt: aq_admission::scoped_presence::PresenceReceipt,
    pub epoch: u64,
}
pub async fn fact(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    operation: Uuid,
) -> Result<Option<Fact>, AppError> {
    let row=sqlx::query("SELECT kind,request_bytes,request_digest,receipt_bytes,compatibility_epoch FROM profiled_presence_operations WHERE node_id=$1 AND operation_id=$2 FOR SHARE").bind(node).bind(operation).fetch_optional(&mut **tx).await.map_err(db)?;
    let Some(row) = row else { return Ok(None) };
    if row.get::<String, _>("kind") != "presence" {
        return Err(changed());
    }
    let request =
        aq_admission::scoped_presence::decode_report(&row.get::<Vec<u8>, _>("request_bytes"))
            .map_err(|_| changed())?;
    let receipt: aq_admission::scoped_presence::PresenceReceipt =
        serde_json::from_slice(&row.get::<Vec<u8>, _>("receipt_bytes")).map_err(|_| changed())?;
    receipt.validate_for(&request).map_err(|_| changed())?;
    if request.identity.node_id != node
        || request.operation_id != operation
        || request.digest().map_err(|_| changed())?.as_str()
            != row.get::<String, _>("request_digest")
    {
        return Err(changed());
    }
    Ok(Some(Fact {
        request,
        receipt,
        epoch: row.get::<i64, _>("compatibility_epoch") as u64,
    }))
}
pub async fn latest_fact(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    attempt: Uuid,
    lease: Uuid,
) -> Result<Option<Fact>, AppError> {
    let operation:Option<Uuid>=sqlx::query_scalar("SELECT operation_id FROM profiled_material_presence_heads WHERE node_id=$1 AND attempt_id=$2 AND lease_id=$3 FOR UPDATE").bind(node).bind(attempt).bind(lease).fetch_optional(&mut **tx).await.map_err(db)?;
    if let Some(operation) = operation {
        Ok(Some(fact(tx, node, operation).await?.ok_or_else(changed)?))
    } else {
        Ok(None)
    }
}
pub async fn retain_fact(
    tx: &mut Transaction<'_, Postgres>,
    request: &aq_admission::scoped_presence::PresenceReport,
    receipt: &aq_admission::scoped_presence::PresenceReceipt,
    epoch: u64,
) -> Result<(), AppError> {
    receipt.validate_for(request).map_err(|_| changed())?;
    let req = canonical(request)?;
    let ack = canonical(receipt)?;
    sqlx::query("INSERT INTO profiled_presence_operations(node_id,operation_id,kind,boot_id,attempt_id,grant_id,lease_id,sequence,inventory_revision,compatibility_epoch,snapshot_digest,request_bytes,request_digest,receipt_bytes,receipt_digest,recorded_at_ms,expires_at_ms) VALUES($1,$2,'presence',$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16)").bind(request.identity.node_id).bind(request.operation_id).bind(request.identity.boot_id).bind(request.binding.attempt_id).bind(request.grant_id).bind(request.lease_id).bind(request.sequence as i64).bind(request.inventory.revision as i64).bind(epoch as i64).bind(request.inventory.snapshot_digest.as_str()).bind(req).bind(request.digest().map_err(|_|changed())?.as_str()).bind(&ack).bind(Digest::of(&ack).as_str()).bind(receipt.recorded_at_ms as i64).bind(receipt.expires_at_ms as i64).execute(&mut **tx).await.map_err(db)?;
    let updated=sqlx::query("UPDATE profiled_material_presence_heads SET operation_id=$4,sequence=$5,compatibility_epoch=$6 WHERE node_id=$1 AND attempt_id=$2 AND lease_id=$3").bind(request.identity.node_id).bind(request.binding.attempt_id).bind(request.lease_id).bind(request.operation_id).bind(request.sequence as i64).bind(epoch as i64).execute(&mut **tx).await.map_err(db)?.rows_affected();
    if updated == 0 {
        sqlx::query("INSERT INTO profiled_material_presence_heads(node_id,attempt_id,lease_id,boot_id,grant_id,operation_id,sequence,compatibility_epoch,revision) VALUES($1,$2,$3,$4,$5,$6,$7,$8,1)").bind(request.identity.node_id).bind(request.binding.attempt_id).bind(request.lease_id).bind(request.identity.boot_id).bind(request.grant_id).bind(request.operation_id).bind(request.sequence as i64).bind(epoch as i64).execute(&mut **tx).await.map_err(db)?;
    }
    Ok(())
}
pub async fn accepted(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    identity: &aq_admission::scoped_presence::InventoryIdentity,
) -> Result<Inventory, AppError> {
    let accepted = inventory(tx, node, identity.operation_id)
        .await?
        .ok_or_else(changed)?;
    if canonical(&accepted.receipt.inventory)? != canonical(identity)? {
        return Err(changed());
    }
    Ok(accepted)
}
