//! One terminal fence transaction; no filesystem/RPC or task/source release.
use super::{
    canonical, conflict,
    context::Context,
    database, history,
    policy::{EvidenceKind, InstalledPolicy},
};
use crate::{
    AppState,
    error::AppError,
    execution::{
        authority::{self, Kind, Policy, hot::HotState},
        profiled::{Status, Stop},
    },
    profiled_execution::initial_grant,
    worker_api::{self, AuthenticatedNode},
};
use aq_admission::{
    Digest, SessionIdentity,
    granted_closure::{Disposition, JournalCheckpoint, Receipt, Request},
};
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

pub async fn record(
    state: &AppState,
    policy: &InstalledPolicy,
    node: &AuthenticatedNode,
    request: &Request,
) -> Result<Receipt, AppError> {
    request
        .validate()
        .map_err(|_| AppError::BadRequest("invalid launch closure request".into()))?;
    let SessionIdentity::Remote { node_id, .. } = request.binding.session else {
        return Err(conflict());
    };
    if node_id != node.id() {
        return Err(AppError::Forbidden("closure node mismatch".into()));
    }
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        record_inner(state, policy, node, request),
    )
    .await
    .map_err(|_| AppError::IdentityUnavailable("closure deadline exceeded".into()))?
}
async fn record_inner(
    state: &AppState,
    policy: &InstalledPolicy,
    node: &AuthenticatedNode,
    request: &Request,
) -> Result<Receipt, AppError> {
    let mut tx = state.store.pool.begin().await.map_err(database)?;
    for statement in [
        "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE",
        "SET LOCAL lock_timeout='500ms'",
        "SET LOCAL statement_timeout='2s'",
    ] {
        sqlx::query(statement)
            .execute(&mut *tx)
            .await
            .map_err(database)?;
    }
    // Installing a replacement policy waits for all old transactions to finish.
    policy.fence(&mut tx).await?;
    worker_api::lock_current_node(&mut tx, node).await?;
    let view = authority::load_locked(
        state,
        &mut tx,
        &request.binding.session,
        Policy::IdentityOnly,
    )
    .await?;
    if view.kind != Kind::ProfiledV1
        || view.offer_id != request.binding.offer_id
        || view.execution_spec_digest != request.binding.execution_spec_digest.as_str()
        || view.launch_manifest_digest.as_ref() != Some(&request.binding.launch_manifest_digest)
    {
        return Err(conflict());
    }
    let row =
        sqlx::query("SELECT state FROM profiled_authority_hot WHERE attempt_id=$1 FOR UPDATE")
            .bind(request.binding.attempt_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(database)?
            .ok_or_else(conflict)?;
    let mut hot: HotState = serde_json::from_value(row.get("state")).map_err(|_| conflict())?;
    hot.validate_against(&request.binding)
        .map_err(|_| conflict())?;
    if let Some(receipt) = replay(&mut tx, node.id(), request).await? {
        tx.commit().await.map_err(database)?;
        return Ok(receipt);
    }
    if !view.holds_task
        || !matches!(hot.stop, Stop::Unknown)
        || matches!(
            hot.status,
            Status::NeverGranted | Status::Finished | Status::Declined
        )
    {
        return Err(conflict());
    }
    if hot
        .grant
        .as_ref()
        .is_none_or(|g| g.grant_id != request.grant_id || g.binding != request.binding)
    {
        return Err(conflict());
    }
    let started: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM profiled_started_receipts WHERE attempt_id=$1)",
    )
    .bind(request.binding.attempt_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(database)?;
    if started {
        return Err(conflict());
    }
    let original = initial_grant::load_original_grant_locked(
        &mut tx,
        node.id(),
        &request.binding,
        request.grant_id,
    )
    .await?;
    if hot.execution_deadline_ms != Some(original.execution_deadline_ms)
        || hot.owner.as_ref().is_none_or(|o| {
            o.guardian_id != original.body.guardian_id
                || o.launch_id != original.body.launch_id
                || o.reservation_id != original.body.reservation_id
        })
    {
        return Err(conflict());
    }
    let registration = history::for_attempt_locked(&mut tx, &request.binding).await?;
    let verifier = policy.verifier(
        &registration.metadata.request.runtime,
        &registration.metadata.qualification,
        &EvidenceKind::of(&request.evidence),
    )?;
    let head = sqlx::query(
        "SELECT phase,revision,digest FROM profiled_owner_journals WHERE attempt_id=$1 FOR UPDATE",
    )
    .bind(request.binding.attempt_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(database)?
    .ok_or_else(conflict)?;
    if head.get::<String, _>("phase") != "prepared" {
        return Err(conflict());
    }
    let context = Context {
        schema_version: 1,
        initial_operation_id: original.operation_id,
        initial_request_digest: original.request_digest,
        initial_versions: original.versions,
        execution_deadline_ms: original.execution_deadline_ms,
        grant: original.body,
        registration_id: registration.id,
        prepared: JournalCheckpoint {
            revision: u64::try_from(head.get::<i64, _>("revision")).map_err(|_| conflict())?,
            digest: serde_json::from_value(serde_json::json!(head.get::<String, _>("digest")))
                .map_err(|_| conflict())?,
        },
        verifier,
        latest_renewal_revision: super::super::renewal::latest_renewal_revision_locked(
            &mut tx,
            node.id(),
            &request.binding,
        )
        .await?,
    };
    context.check_original(&mut tx, node.id(), request).await?;
    // Acquire quotas before mutating authority. All variable retained bytes use
    // their actual stored representation; physical row/index overhead is separate.
    let request_bytes = canonical(request)?;
    let context_bytes = canonical(&context)?;
    if context_bytes.len() > 65536 {
        return Err(conflict());
    }
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended('aq-profiled-launch-closure-quota-v1',0))",
    )
    .execute(&mut *tx)
    .await
    .map_err(database)?;
    // Revoke private permission only. Preserve grant, owner, physical phase,
    // retention/effect facts and the common held task claim.
    let leases=sqlx::query("SELECT lease_id FROM admission_material_leases WHERE prospective_attempt_id=$1 ORDER BY lease_id LIMIT 65 FOR UPDATE").bind(request.binding.attempt_id).fetch_all(&mut *tx).await.map_err(database)?;
    if leases.len() > 64 {
        return Err(conflict());
    }
    sqlx::query("UPDATE admission_material_leases SET state='revoked' WHERE prospective_attempt_id=$1 AND state IN ('pending','admitted')").bind(request.binding.attempt_id).execute(&mut *tx).await.map_err(database)?;
    sqlx::query("UPDATE agent_launch_bindings SET revoked=true,credential_verifier=NULL,credential_expires_at_ms=NULL,credential_version=credential_version+1,authority_revision=authority_revision+1 WHERE attempt_id=$1").bind(request.binding.attempt_id).execute(&mut *tx).await.map_err(database)?;
    hot.status = Status::Cancelling;
    hot.revoked = true;
    hot.validate_against(&request.binding)
        .map_err(|_| conflict())?;
    let authority_revision:i64=sqlx::query_scalar("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1 RETURNING authority_revision").bind(request.binding.attempt_id).bind(serde_json::to_value(&hot).map_err(|_|AppError::Internal)?).fetch_one(&mut *tx).await.map_err(database)?;
    let recorded_at_ms =
        u64::try_from((state.clock)().timestamp_millis()).map_err(|_| conflict())?;
    let receipt = Receipt {
        schema_version: 1,
        operation_id: request.operation_id,
        binding: request.binding.clone(),
        grant_id: request.grant_id,
        request_digest: request.digest().map_err(|_| conflict())?,
        recorded_at_ms,
        authority_revision_at_record: u64::try_from(authority_revision).map_err(|_| conflict())?,
        disposition: Disposition::LaunchClosedAwaitingSettlement,
    };
    receipt.validate_for(request).map_err(|_| conflict())?;
    let receipt_bytes = canonical(&receipt)?;
    let binding = serde_json::to_value(&request.binding).map_err(|_| AppError::Internal)?;
    let verifier = serde_json::to_value(&context.verifier).map_err(|_| AppError::Internal)?;
    let incoming: i64 = sqlx::query_scalar(
        "SELECT (octet_length($1::JSONB::TEXT)+octet_length($2::JSONB::TEXT))::BIGINT",
    )
    .bind(&binding)
    .bind(&verifier)
    .fetch_one(&mut *tx)
    .await
    .map_err(database)?;
    let incoming = incoming
        .checked_add((request_bytes.len() + context_bytes.len() + receipt_bytes.len()) as i64)
        .ok_or_else(conflict)?;
    let (n,b,pn,pb):(i64,i64,i64,i64)=sqlx::query_as("SELECT count(*),COALESCE(sum(octet_length(request_bytes)+octet_length(context_bytes)+octet_length(receipt_bytes)+octet_length(binding::text)+octet_length(verifier::text)),0),count(*) FILTER(WHERE node_id=$1),COALESCE(sum(octet_length(request_bytes)+octet_length(context_bytes)+octet_length(receipt_bytes)+octet_length(binding::text)+octet_length(verifier::text)) FILTER(WHERE node_id=$1),0) FROM profiled_launch_closures WHERE NOT EXISTS(SELECT 1 FROM profiled_v2_accepts a WHERE a.attempt_id=profiled_launch_closures.attempt_id)").bind(node.id()).fetch_one(&mut *tx).await.map_err(database)?;
    if n >= 4096 || pn >= 256 || b + incoming > 67108864 || pb + incoming > 4194304 {
        return Err(AppError::Conflict(
            "closure retained capacity exhausted".into(),
        ));
    }
    sqlx::query("INSERT INTO profiled_launch_closures(attempt_id,node_id,operation_id,binding,grant_id,initial_prepare_operation_id,registration_id,evidence_kind,verifier,terminal_revision,request_digest,request_bytes,context_bytes,receipt_bytes,recorded_at_ms,authority_revision) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16)").bind(request.binding.attempt_id).bind(node.id()).bind(request.operation_id).bind(binding).bind(request.grant_id).bind(context.initial_operation_id).bind(context.registration_id).bind(EvidenceKind::of(&request.evidence).as_str()).bind(verifier).bind(request.journal.revision as i64).bind(receipt.request_digest.as_str()).bind(request_bytes).bind(context_bytes).bind(receipt_bytes).bind(receipt.recorded_at_ms as i64).bind(authority_revision).execute(&mut *tx).await.map_err(database)?;
    tx.commit().await.map_err(database)?;
    Ok(receipt)
}
async fn replay(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    request: &Request,
) -> Result<Option<Receipt>, AppError> {
    let rows=sqlx::query("SELECT * FROM profiled_launch_closures WHERE attempt_id=$1 OR(node_id=$2 AND operation_id=$3) LIMIT 2 FOR SHARE").bind(request.binding.attempt_id).bind(node).bind(request.operation_id).fetch_all(&mut **tx).await.map_err(database)?;
    if rows.is_empty() {
        return Ok(None);
    }
    if rows.len() != 1 {
        return Err(conflict());
    }
    let row = &rows[0];
    if row.get::<Uuid, _>("node_id") != node
        || row.get::<Uuid, _>("attempt_id") != request.binding.attempt_id
        || row.get::<Uuid, _>("operation_id") != request.operation_id
        || row.get::<String, _>("request_digest")
            != request.digest().map_err(|_| conflict())?.as_str()
    {
        return Err(conflict());
    }
    let request_bytes: Vec<u8> = row.get("request_bytes");
    let context_bytes: Vec<u8> = row.get("context_bytes");
    let receipt_bytes: Vec<u8> = row.get("receipt_bytes");
    for (bytes, key, max) in [
        (&request_bytes, "request_bytes_digest", 32768),
        (&context_bytes, "context_digest", 65536),
        (&receipt_bytes, "receipt_digest", 16384),
    ] {
        if bytes.is_empty()
            || bytes.len() > max
            || Digest::of(bytes).as_str() != row.get::<String, _>(key)
        {
            return Err(conflict());
        }
    }
    if request_bytes != canonical(request)? {
        return Err(conflict());
    }
    let context: Context = serde_json::from_slice(&context_bytes).map_err(|_| conflict())?;
    if context_bytes != canonical(&context)? {
        return Err(conflict());
    }
    context.check_original(tx, node, request).await?;
    let receipt = aq_admission::granted_closure::decode_receipt(&receipt_bytes, request)
        .map_err(|_| conflict())?;
    if receipt_bytes != canonical(&receipt)?
        || row.get::<Uuid, _>("initial_prepare_operation_id") != context.initial_operation_id
        || row.get::<Uuid, _>("registration_id") != context.registration_id
        || row.get::<Uuid, _>("grant_id") != request.grant_id
        || row.get::<i64, _>("terminal_revision") != request.journal.revision as i64
        || row.get::<serde_json::Value, _>("binding")
            != serde_json::to_value(&request.binding).map_err(|_| AppError::Internal)?
        || row.get::<serde_json::Value, _>("verifier")
            != serde_json::to_value(&context.verifier).map_err(|_| AppError::Internal)?
        || row.get::<String, _>("evidence_kind") != EvidenceKind::of(&request.evidence).as_str()
        || row.get::<i64, _>("recorded_at_ms") != receipt.recorded_at_ms as i64
        || row.get::<i64, _>("authority_revision") != receipt.authority_revision_at_record as i64
    {
        return Err(conflict());
    }
    Ok(Some(receipt))
}
