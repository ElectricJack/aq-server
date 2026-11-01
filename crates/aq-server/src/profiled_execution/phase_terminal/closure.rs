//! No-Started phase closure: revoke permission, retain task/source/effect holds.
use super::common::{self, canonical, conflict, database, now};
use crate::{
    AppState,
    error::AppError,
    execution::profiled::{Status, Stop},
    worker_api::AuthenticatedNode,
};
use aq_admission::granted_closure::{Disposition, Evidence};
use aq_phase_terminal_protocol as wire;
use sqlx::Row;

pub async fn record(
    state: &AppState,
    node: &AuthenticatedNode,
    q: &wire::ClosureRequest,
) -> Result<wire::ClosureReceipt, AppError> {
    q.validate()
        .map_err(|_| AppError::BadRequest("invalid phase launch closure".into()))?;
    let _permit = common::admit_control()?;
    let state = state.control_view();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        record_inner(&state, node, q),
    )
    .await
    .map_err(|_| AppError::IdentityUnavailable("phase closure commit outcome unknown".into()))?
}
async fn record_inner(
    state: &AppState,
    node: &AuthenticatedNode,
    q: &wire::ClosureRequest,
) -> Result<wire::ClosureReceipt, AppError> {
    let r = &q.original;
    let mut tx = state.store.pool.begin().await.map_err(database)?;
    for sql in [
        "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE",
        "SET LOCAL lock_timeout='500ms'",
        "SET LOCAL statement_timeout='2s'",
    ] {
        sqlx::query(sql).execute(&mut *tx).await.map_err(database)?;
    }
    let mut loaded = common::load_locked(state, &mut tx, node, &r.binding, r.grant_id).await?;
    q.validate_for(&loaded.context()).map_err(|_| conflict())?;
    let outer = canonical(q)?;
    let digest = q.digest().map_err(|_| conflict())?;
    let prior=sqlx::query("SELECT * FROM profiled_launch_closures WHERE attempt_id=$1 OR(node_id=$2 AND operation_id=$3) LIMIT 2 FOR SHARE").bind(r.binding.attempt_id).bind(node.id()).bind(r.operation_id).fetch_all(&mut *tx).await.map_err(database)?;
    if !prior.is_empty() {
        if prior.len() != 1 {
            return Err(conflict());
        }
        let old = &prior[0];
        if old.get::<uuid::Uuid, _>("attempt_id") != r.binding.attempt_id
            || old.get::<uuid::Uuid, _>("operation_id") != r.operation_id
            || old.get::<String, _>("initial_authority_kind") != "phase_execution_v2"
            || old
                .get::<Option<String>, _>("initial_authority_receipt_digest")
                .as_deref()
                != Some(loaded.history.receipt_digest.as_str())
            || old
                .get::<Option<String>, _>("phase_request_digest")
                .as_deref()
                != Some(digest.as_str())
            || old
                .get::<Option<Vec<u8>>, _>("phase_envelope_bytes")
                .as_deref()
                != Some(outer.as_slice())
        {
            return Err(conflict());
        }
        let bytes = old
            .get::<Option<Vec<u8>>, _>("phase_ack_bytes")
            .ok_or_else(conflict)?;
        let ack = wire::decode_closure_receipt(&bytes, q).map_err(|_| conflict())?;
        let qualification: aq_admission::ArtifactRef = serde_json::from_value(
            old.get::<Option<serde_json::Value>, _>("phase_terminal_qualification")
                .ok_or_else(conflict)?,
        )
        .map_err(|_| conflict())?;
        let (expected_context, verifier) = retained_context(&loaded, q, &qualification)?;
        let context_bytes = old.get::<Vec<u8>, _>("context_bytes");
        if canonical(&ack)? != bytes
            || old.get::<Vec<u8>, _>("request_bytes") != canonical(r)?
            || old.get::<String, _>("request_digest") != ack.original.request_digest.as_str()
            || old.get::<Vec<u8>, _>("receipt_bytes") != canonical(&ack.original)?
            || context_bytes != expected_context
            || old.get::<String, _>("context_digest")
                != aq_admission::Digest::of(&context_bytes).as_str()
            || old.get::<serde_json::Value, _>("verifier") != verifier
            || old.get::<uuid::Uuid, _>("initial_prepare_operation_id")
                != loaded.history.request.original.operation_id
            || old.get::<uuid::Uuid, _>("registration_id") != loaded.registration.id
            || old.get::<serde_json::Value, _>("binding")
                != serde_json::to_value(&r.binding).map_err(|_| AppError::Internal)?
        {
            return Err(conflict());
        }

        tx.commit().await.map_err(database)?;
        return Ok(ack);
    }
    if !loaded.view.holds_task
        || !matches!(loaded.hot.stop, Stop::Unknown)
        || matches!(
            loaded.hot.status,
            Status::NeverGranted | Status::Finished | Status::Declined
        )
    {
        return Err(conflict());
    }
    let started: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM profiled_started_receipts WHERE attempt_id=$1)",
    )
    .bind(r.binding.attempt_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(database)?;
    if started {
        return Err(conflict());
    }
    let head = sqlx::query(
        "SELECT phase,revision,digest FROM profiled_owner_journals WHERE attempt_id=$1 FOR UPDATE",
    )
    .bind(r.binding.attempt_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(database)?
    .ok_or_else(conflict)?;
    if head.get::<String, _>("phase") != "prepared"
        || head.get::<i64, _>("revision") != loaded.prepared.revision as i64
        || head.get::<String, _>("digest") != loaded.prepared.digest.as_str()
    {
        return Err(conflict());
    }
    // No phase renewal adapter is qualified. Any unexpected renewal is corruption,
    // never a source from which to fabricate a later Prepared checkpoint.
    if crate::profiled_execution::renewal::latest_renewal_revision_locked(
        &mut tx,
        node.id(),
        &r.binding,
    )
    .await?
    .is_some()
    {
        return Err(conflict());
    }
    let evidence = match &r.evidence {
        Evidence::Suppressed { .. } => "suppressed",
        Evidence::HostStopped { .. } => "host_stopped",
    };
    let (context_bytes, verifier) = retained_context(&loaded, q, &loaded.qualification)?;
    let leases=sqlx::query("SELECT lease_id FROM admission_material_leases WHERE prospective_attempt_id=$1 ORDER BY lease_id LIMIT 65 FOR UPDATE").bind(r.binding.attempt_id).fetch_all(&mut *tx).await.map_err(database)?;
    if leases.len() > 64 {
        return Err(conflict());
    }
    sqlx::query("UPDATE admission_material_leases SET state='revoked' WHERE prospective_attempt_id=$1 AND state IN ('pending','admitted')").bind(r.binding.attempt_id).execute(&mut *tx).await.map_err(database)?;
    sqlx::query("UPDATE agent_launch_bindings SET revoked=true,credential_verifier=NULL,credential_expires_at_ms=NULL,credential_version=credential_version+1,authority_revision=authority_revision+1 WHERE attempt_id=$1").bind(r.binding.attempt_id).execute(&mut *tx).await.map_err(database)?;
    loaded.hot.status = Status::Cancelling;
    loaded.hot.revoked = true;
    loaded
        .hot
        .validate_against(&r.binding)
        .map_err(|_| conflict())?;
    let authority:i64=sqlx::query_scalar("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1 RETURNING authority_revision").bind(r.binding.attempt_id).bind(serde_json::to_value(&loaded.hot).map_err(|_|AppError::Internal)?).fetch_one(&mut *tx).await.map_err(database)?;
    let recorded = now(state)?;
    if recorded < loaded.history.receipt.issued_at_ms {
        return Err(conflict());
    }
    let original = aq_admission::granted_closure::Receipt {
        schema_version: 1,
        operation_id: r.operation_id,
        binding: r.binding.clone(),
        grant_id: r.grant_id,
        request_digest: r.digest().map_err(|_| conflict())?,
        recorded_at_ms: recorded,
        authority_revision_at_record: u64::try_from(authority).map_err(|_| conflict())?,
        disposition: Disposition::LaunchClosedAwaitingSettlement,
    };
    let ack = wire::ClosureReceipt {
        schema_version: 1,
        negotiation: q.negotiation.clone(),
        origin: q.origin.clone(),
        request_digest: digest.clone(),
        original,
    };
    ack.validate_for(q).map_err(|_| conflict())?;
    loaded.set_writer_marker(&mut tx).await?;
    sqlx::query("INSERT INTO profiled_launch_closures(attempt_id,node_id,operation_id,binding,grant_id,initial_prepare_operation_id,registration_id,evidence_kind,verifier,terminal_revision,request_digest,request_bytes,context_bytes,receipt_bytes,recorded_at_ms,authority_revision,initial_authority_kind,initial_authority_receipt_digest,phase_request_digest,phase_envelope_bytes,phase_ack_bytes,phase_terminal_qualification) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,'phase_execution_v2',$17,$18,$19,$20,$21)").bind(r.binding.attempt_id).bind(node.id()).bind(r.operation_id).bind(serde_json::to_value(&r.binding).map_err(|_|AppError::Internal)?).bind(r.grant_id).bind(loaded.history.request.original.operation_id).bind(loaded.registration.id).bind(evidence).bind(verifier).bind(r.journal.revision as i64).bind(ack.original.request_digest.as_str()).bind(canonical(r)?).bind(context_bytes).bind(canonical(&ack.original)?).bind(recorded as i64).bind(authority).bind(loaded.history.receipt_digest.as_str()).bind(digest.as_str()).bind(outer).bind(canonical(&ack)?).bind(serde_json::to_value(&loaded.qualification).map_err(|_|AppError::Internal)?).execute(&mut *tx).await.map_err(database)?;
    if now(state)? < recorded {
        return Err(conflict());
    }
    tx.commit().await.map_err(database)?;
    Ok(ack)
}

fn retained_context(
    loaded: &common::Loaded,
    q: &wire::ClosureRequest,
    qualification: &aq_admission::ArtifactRef,
) -> Result<(Vec<u8>, serde_json::Value), AppError> {
    let verifier = serde_json::json!({"runtime":loaded.registration.metadata.request.runtime,"qualification":loaded.registration.metadata.qualification,"profile":qualification,"kinds":["suppressed","host_stopped"]});
    let context = serde_json::json!({"schema_version":2,"origin":q.origin,"initial_operation_id":loaded.history.request.original.operation_id,"initial_request_digest":loaded.history.receipt.request_digest,"initial_versions":loaded.history.receipt.versions,"execution_deadline_ms":loaded.history.receipt.execution_deadline_ms,"registration_id":loaded.registration.id,"prepared":loaded.prepared,"verifier":verifier});
    let bytes = canonical(&context)?;
    if bytes.len() > 65536 {
        return Err(conflict());
    }
    Ok((bytes, verifier))
}
