//! Paired actual phase Started→Stopped owner. No renewal or release is implied.
use super::common::{self, canonical, conflict, database, now};
use super::wire;
use crate::{
    AppState,
    error::AppError,
    execution::profiled::{Status, Stop},
    profiled_execution::journal,
    worker_api::AuthenticatedNode,
};
use aq_admission::Digest;
use sqlx::Row;

pub async fn record(
    state: &AppState,
    node: &AuthenticatedNode,
    q: &wire::StoppedRequest,
) -> Result<wire::StoppedReceipt, AppError> {
    q.validate()
        .map_err(|_| AppError::BadRequest("invalid phase stopped report".into()))?;
    let _permit = common::admit_control()?;
    let state = state.control_view();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        record_inner(&state, node, q),
    )
    .await
    .map_err(|_| AppError::IdentityUnavailable("phase stopped commit outcome unknown".into()))?
}
async fn record_inner(
    state: &AppState,
    node: &AuthenticatedNode,
    q: &wire::StoppedRequest,
) -> Result<wire::StoppedReceipt, AppError> {
    let request = &q.original;
    let binding = &request.evidence.binding;
    let grant_id = request.grant_id().map_err(|_| conflict())?;
    let mut tx = state.store.pool.begin().await.map_err(database)?;
    for sql in [
        "SET LOCAL lock_timeout='500ms'",
        "SET LOCAL statement_timeout='2s'",
    ] {
        sqlx::query(sql).execute(&mut *tx).await.map_err(database)?;
    }
    let mut loaded = common::load_locked(state, &mut tx, node, binding, grant_id).await?;
    let started_row=sqlx::query("SELECT request_bytes,request_digest,receipt_bytes,phase_envelope_bytes,phase_ack_bytes,initial_authority_kind,initial_authority_receipt_digest FROM profiled_started_receipts WHERE attempt_id=$1 FOR SHARE").bind(binding.attempt_id).fetch_optional(&mut *tx).await.map_err(database)?.ok_or_else(conflict)?;
    if started_row.get::<String, _>("initial_authority_kind") != "phase_execution_v2"
        || started_row
            .get::<Option<String>, _>("initial_authority_receipt_digest")
            .as_deref()
            != Some(loaded.history.receipt_digest.as_str())
    {
        return Err(conflict());
    }
    let started_bytes = started_row
        .get::<Option<Vec<u8>>, _>("phase_envelope_bytes")
        .ok_or_else(conflict)?;
    let started = wire::decode_started_request(&started_bytes).map_err(|_| conflict())?;
    let started_ack = started_row
        .get::<Option<Vec<u8>>, _>("phase_ack_bytes")
        .ok_or_else(conflict)?;
    let acknowledged =
        wire::decode_started_receipt(&started_ack, &started).map_err(|_| conflict())?;
    if canonical(&started)? != started_bytes
        || canonical(&acknowledged)? != started_ack
        || canonical(&started.original)? != started_row.get::<Vec<u8>, _>("request_bytes")
        || canonical(&acknowledged.original)? != started_row.get::<Vec<u8>, _>("receipt_bytes")
        || started.original.digest().map_err(|_| conflict())?.as_str()
            != started_row.get::<String, _>("request_digest")
    {
        return Err(conflict());
    }
    started
        .validate_for(&loaded.context())
        .map_err(|_| conflict())?;
    q.validate_for(&loaded.context(), &started)
        .map_err(|_| conflict())?;
    let outer = canonical(q)?;
    let digest = q.digest().map_err(|_| conflict())?;
    let inner_digest = request.digest().map_err(|_| conflict())?;
    let operation = request.evidence.stop_operation_id;
    let prior=sqlx::query("SELECT attempt_id,operation_id,phase_request_digest,phase_envelope_bytes,phase_ack_bytes,request_bytes,request_digest,receipt_bytes,proof_bytes,proof_digest,initial_authority_receipt_digest,initial_authority_kind FROM profiled_stopped_receipts WHERE attempt_id=$1 OR(node_id=$2 AND operation_id=$3) FOR SHARE").bind(binding.attempt_id).bind(node.id()).bind(operation).fetch_all(&mut *tx).await.map_err(database)?;
    if !prior.is_empty() {
        if prior.len() != 1 {
            return Err(conflict());
        }
        let r = &prior[0];
        if r.get::<uuid::Uuid, _>("attempt_id") != binding.attempt_id
            || r.get::<uuid::Uuid, _>("operation_id") != operation
            || r.get::<String, _>("initial_authority_kind") != "phase_execution_v2"
            || r.get::<Option<String>, _>("initial_authority_receipt_digest")
                .as_deref()
                != Some(loaded.history.receipt_digest.as_str())
            || r.get::<Option<String>, _>("phase_request_digest")
                .as_deref()
                != Some(digest.as_str())
            || r.get::<Option<Vec<u8>>, _>("phase_envelope_bytes")
                .as_deref()
                != Some(outer.as_slice())
        {
            return Err(conflict());
        }
        let bytes = r
            .get::<Option<Vec<u8>>, _>("phase_ack_bytes")
            .ok_or_else(conflict)?;
        let receipt = wire::decode_stopped_receipt(&bytes, q).map_err(|_| conflict())?;
        let proof = canonical(&request.proof)?;
        if canonical(&receipt)? != bytes
            || r.get::<Vec<u8>, _>("request_bytes") != canonical(request)?
            || r.get::<String, _>("request_digest") != inner_digest.as_str()
            || r.get::<Vec<u8>, _>("receipt_bytes") != canonical(&receipt.original)?
            || r.get::<Vec<u8>, _>("proof_bytes") != proof
            || r.get::<String, _>("proof_digest") != Digest::of(&proof).as_str()
        {
            return Err(conflict());
        }
        if let Some(delegate) = node.delegated() {
            let current = &delegate.current;
            let context = crate::delegated_permits::runtime::context(&mut tx, current).await?;
            let retained = serde_json::from_slice(&r.get::<Vec<u8>, _>("request_bytes"))
                .map_err(|_| conflict())?;
            let incoming =
                serde_json::from_value(serde_json::to_value(request).map_err(|_| conflict())?)
                    .map_err(|_| conflict())?;
            current
                .record
                .delivery(crate::delegated_permits::runtime::key(state)?)
                .map_err(|_| conflict())?
                .validate_stopped_replay(&context, &retained, &incoming, now(state)?)
                .map_err(|_| conflict())?;
        }
        tx.commit().await.map_err(database)?;
        return Ok(receipt);
    }
    if !loaded.view.holds_task
        || !matches!(
            loaded.hot.status,
            Status::Granted | Status::Cancelling | Status::Unknown
        )
        || !matches!(loaded.hot.stop, Stop::Running | Stop::Unknown)
    {
        return Err(conflict());
    }
    let head = sqlx::query(
        "SELECT phase,revision FROM profiled_owner_journals WHERE attempt_id=$1 FOR UPDATE",
    )
    .bind(binding.attempt_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(database)?
    .ok_or_else(conflict)?;
    if head.get::<String, _>("phase") != "started"
        || request.journal().map_err(|_| conflict())?.0 as i64 <= head.get::<i64, _>("revision")
    {
        return Err(conflict());
    }
    let recorded = now(state)?;
    if recorded < loaded.history.receipt.issued_at_ms {
        return Err(conflict());
    }
    let revision = loaded
        .versions
        .authority_revision
        .checked_add(1)
        .ok_or(AppError::Internal)?;
    let original = aq_admission::stopped::Receipt {
        schema_version: 1,
        operation_id: operation,
        binding: binding.clone(),
        grant_id,
        request_digest: inner_digest.clone(),
        recorded_at_ms: recorded,
        authority_revision_at_record: revision,
    };
    let ack = wire::StoppedReceipt {
        schema_version: 1,
        negotiation: q.negotiation.clone(),
        origin: q.origin.clone(),
        request_digest: digest.clone(),
        original,
    };
    ack.validate_for(q).map_err(|_| AppError::Internal)?;
    let proof = canonical(&request.proof)?;
    loaded.set_writer_marker(&mut tx).await?;
    sqlx::query("INSERT INTO profiled_stopped_receipts(attempt_id,node_id,operation_id,request_bytes,request_digest,proof_bytes,proof_digest,receipt_bytes,before_record_revision,before_grant_revision,initial_authority_kind,initial_authority_receipt_digest,phase_request_digest,phase_envelope_bytes,phase_ack_bytes,phase_terminal_qualification) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,'phase_execution_v2',$11,$12,$13,$14,$15)").bind(binding.attempt_id).bind(node.id()).bind(operation).bind(canonical(request)?).bind(inner_digest.as_str()).bind(&proof).bind(Digest::of(&proof).as_str()).bind(canonical(&ack.original)?).bind(loaded.versions.record_revision as i64).bind(loaded.versions.grant_revision as i64).bind(loaded.history.receipt_digest.as_str()).bind(digest.as_str()).bind(outer).bind(canonical(&ack)?).bind(serde_json::to_value(&loaded.qualification).map_err(|_|AppError::Internal)?).execute(&mut *tx).await.map_err(database)?;
    journal::record_stopped(&mut tx, request).await?;
    loaded.hot.status = Status::Finished;
    loaded.hot.stop = Stop::Stopped {
        receipt: ack.original.artifact().map_err(|_| AppError::Internal)?,
    };
    loaded
        .hot
        .validate_against(binding)
        .map_err(|_| AppError::Internal)?;
    let updated=sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1 RETURNING record_revision,authority_revision,grant_revision").bind(binding.attempt_id).bind(serde_json::to_value(&loaded.hot).map_err(|_|AppError::Internal)?).fetch_one(&mut *tx).await.map_err(database)?;
    if updated.get::<i64, _>("authority_revision") != revision as i64
        || updated.get::<i64, _>("record_revision") != loaded.versions.record_revision as i64 + 1
        || updated.get::<i64, _>("grant_revision") != loaded.versions.grant_revision as i64
        || now(state)? < recorded
    {
        return Err(conflict());
    }
    crate::profiled_execution::cancellation::settle_stopped_locked(
        &mut tx,
        binding.attempt_id,
        recorded,
    )
    .await?;
    tx.commit().await.map_err(database)?;
    Ok(ack)
}
