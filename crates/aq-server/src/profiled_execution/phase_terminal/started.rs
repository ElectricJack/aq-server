//! Qualified phase start observation. No lease extension or task/resource release.
use super::common::{self, Loaded};
use crate::{AppState, error::AppError, execution::profiled::Stop, worker_api::AuthenticatedNode};
use aq_admission::{Digest, started};
use aq_phase_terminal_protocol as wire;
use sqlx::{Postgres, Row, Transaction, postgres::PgRow};

pub async fn record(
    state: &AppState,
    node: &AuthenticatedNode,
    request: &wire::StartedRequest,
) -> Result<wire::StartedReceipt, AppError> {
    request
        .validate()
        .map_err(|_| AppError::BadRequest("invalid phase Started report".into()))?;
    let _permit = common::admit_work()?;
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        record_inner(state, node, request),
    )
    .await
    .map_err(|_| AppError::IdentityUnavailable("phase Started deadline exceeded".into()))?
}
fn replay(
    row: &PgRow,
    loaded: &Loaded,
    request: &wire::StartedRequest,
) -> Result<wire::StartedReceipt, AppError> {
    let q = &request.original;
    if row.get::<uuid::Uuid, _>("node_id") != loaded.registration.metadata.request.identity.node_id
        || row.get::<uuid::Uuid, _>("attempt_id") != q.binding.attempt_id
        || row.get::<uuid::Uuid, _>("operation_id") != q.operation_id
        || row.get::<String, _>("initial_authority_kind") != "phase_execution_v2"
        || row
            .get::<Option<String>, _>("initial_authority_receipt_digest")
            .as_deref()
            != Some(loaded.history.receipt_digest.as_str())
        || row.get::<String, _>("request_digest")
            != q.digest().map_err(|_| common::conflict())?.as_str()
        || row
            .get::<Option<String>, _>("phase_request_digest")
            .as_deref()
            != Some(request.digest().map_err(|_| common::conflict())?.as_str())
    {
        return Err(common::conflict());
    }
    let envelope = row
        .get::<Option<Vec<u8>>, _>("phase_envelope_bytes")
        .ok_or_else(common::conflict)?;
    let ack = row
        .get::<Option<Vec<u8>>, _>("phase_ack_bytes")
        .ok_or_else(common::conflict)?;
    if envelope.len() > wire::MAX_BYTES
        || ack.len() > wire::MAX_BYTES
        || row
            .get::<Option<String>, _>("phase_envelope_digest")
            .as_deref()
            != Some(Digest::of(&envelope).as_str())
        || row.get::<Option<String>, _>("phase_ack_digest").as_deref()
            != Some(Digest::of(&ack).as_str())
    {
        return Err(common::conflict());
    }
    let stored: wire::StartedRequest =
        serde_json::from_slice(&envelope).map_err(|_| common::conflict())?;
    stored
        .validate_for(&loaded.context())
        .map_err(|_| common::conflict())?;
    let receipt: wire::StartedReceipt =
        serde_json::from_slice(&ack).map_err(|_| common::conflict())?;
    receipt
        .validate_for(request)
        .map_err(|_| common::conflict())?;
    if common::canonical(&stored)? != common::canonical(request)?
        || common::canonical(&stored)? != envelope
        || common::canonical(&receipt)? != ack
        || row.get::<Vec<u8>, _>("request_bytes") != common::canonical(q)?
        || row.get::<Vec<u8>, _>("receipt_bytes") != common::canonical(&receipt.original)?
    {
        return Err(common::conflict());
    }
    Ok(receipt)
}
async fn record_inner(
    state: &AppState,
    node: &AuthenticatedNode,
    request: &wire::StartedRequest,
) -> Result<wire::StartedReceipt, AppError> {
    let q = &request.original;
    let mut tx = state.store.pool.begin().await.map_err(common::database)?;
    for setting in [
        "SET LOCAL lock_timeout='500ms'",
        "SET LOCAL statement_timeout='2s'",
    ] {
        sqlx::query(setting)
            .execute(&mut *tx)
            .await
            .map_err(common::database)?;
    }
    let mut loaded = common::load_locked(state, &mut tx, node, &q.binding, q.grant_id).await?;
    request
        .validate_for(&loaded.context())
        .map_err(|_| common::conflict())?;
    let prior=sqlx::query("SELECT * FROM profiled_started_receipts WHERE attempt_id=$1 OR(node_id=$2 AND operation_id=$3) ORDER BY attempt_id LIMIT 2 FOR SHARE").bind(q.binding.attempt_id).bind(node.id()).bind(q.operation_id).fetch_all(&mut *tx).await.map_err(common::database)?;
    if !prior.is_empty() {
        if prior.len() != 1 {
            return Err(common::conflict());
        }
        let receipt = replay(&prior[0], &loaded, request)?;
        // Historical ACK only; no current eligibility, clock, hot or journal mutation.
        tx.commit().await.map_err(common::database)?;
        return Ok(receipt);
    }
    if !matches!(loaded.hot.stop, Stop::Unknown) {
        return Err(common::conflict());
    }
    let head = sqlx::query(
        "SELECT phase,revision,digest FROM profiled_owner_journals WHERE attempt_id=$1 FOR UPDATE",
    )
    .bind(q.binding.attempt_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(common::database)?
    .ok_or_else(common::conflict)?;
    if head.get::<String, _>("phase") != "prepared"
        || head.get::<i64, _>("revision") as u64 != loaded.prepared.revision
        || head.get::<String, _>("digest") != loaded.prepared.digest.as_str()
    {
        return Err(common::conflict());
    }
    common::require_start_open_locked(state, &mut tx, &loaded).await?;
    loaded.set_writer_marker(&mut tx).await?;
    let recorded_at_ms = common::now(state)?;
    if recorded_at_ms < loaded.history.receipt.issued_at_ms {
        return Err(common::conflict());
    }
    let receipt = wire::StartedReceipt {
        schema_version: 1,
        negotiation: request.negotiation.clone(),
        origin: request.origin.clone(),
        request_digest: request.digest().map_err(|_| common::conflict())?,
        original: started::Receipt {
            schema_version: 1,
            operation_id: q.operation_id,
            binding: q.binding.clone(),
            grant_id: q.grant_id,
            request_digest: q.digest().map_err(|_| common::conflict())?,
            recorded_at_ms,
            authority_revision_at_record: loaded
                .versions
                .authority_revision
                .checked_add(1)
                .ok_or(AppError::Internal)?,
        },
    };
    receipt
        .validate_for(request)
        .map_err(|_| AppError::Internal)?;
    write_started(&mut tx, node, request, &receipt, &loaded).await?;
    super::super::journal::record_started(&mut tx, q).await?;
    loaded.hot.stop = Stop::Running;
    loaded
        .hot
        .validate_against(&q.binding)
        .map_err(|_| AppError::Internal)?;
    let updated=sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1 RETURNING record_revision,authority_revision,grant_revision").bind(q.binding.attempt_id).bind(serde_json::to_value(&loaded.hot).map_err(|_|AppError::Internal)?).fetch_one(&mut *tx).await.map_err(common::database)?;
    if u64::try_from(updated.get::<i64, _>("authority_revision")).ok()
        != Some(receipt.original.authority_revision_at_record)
        || u64::try_from(updated.get::<i64, _>("record_revision")).ok()
            != loaded.versions.record_revision.checked_add(1)
        || u64::try_from(updated.get::<i64, _>("grant_revision")).ok()
            != Some(loaded.versions.grant_revision)
    {
        return Err(AppError::Internal);
    }
    // Every mutable eligibility row remains locked. The final awaited writes
    // may consume the remaining lease; never acknowledge a newly expired start.
    let final_now = common::now(state)?;
    if final_now < recorded_at_ms {
        return Err(common::conflict());
    }
    loaded.view.active_at(final_now)?;
    tx.commit().await.map_err(common::database)?;
    Ok(receipt)
}
async fn write_started(
    tx: &mut Transaction<'_, Postgres>,
    node: &AuthenticatedNode,
    q: &wire::StartedRequest,
    r: &wire::StartedReceipt,
    loaded: &Loaded,
) -> Result<(), AppError> {
    sqlx::query("INSERT INTO profiled_started_receipts(attempt_id,node_id,operation_id,request_bytes,request_digest,receipt_bytes,before_record_revision,initial_authority_kind,initial_authority_receipt_digest,phase_request_digest,phase_envelope_bytes,phase_ack_bytes,phase_terminal_qualification) VALUES($1,$2,$3,$4,$5,$6,$7,'phase_execution_v2',$8,$9,$10,$11,$12)")
        .bind(q.original.binding.attempt_id).bind(node.id()).bind(q.original.operation_id)
        .bind(common::canonical(&q.original)?).bind(q.original.digest().map_err(|_|common::conflict())?.as_str())
        .bind(common::canonical(&r.original)?).bind(i64::try_from(loaded.versions.record_revision).map_err(|_|AppError::Internal)?)
        .bind(loaded.history.receipt_digest.as_str()).bind(q.digest().map_err(|_|common::conflict())?.as_str())
        .bind(common::canonical(q)?).bind(common::canonical(r)?)
        .bind(serde_json::to_value(&loaded.qualification).map_err(|_|AppError::Internal)?)
        .execute(&mut **tx).await.map_err(common::database)?;
    Ok(())
}
