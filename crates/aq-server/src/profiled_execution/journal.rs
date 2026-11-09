//! Internal launch-transition owner. Call only while holding node→attempt→hot
//! locks, after the corresponding HotState transition and before commit.
//! Request digests and renew counters are not a substitute for this journal.
use crate::{error::AppError, execution::authority::hot::LaunchOwner};
use aq_admission::{Digest, LaunchBinding};
use aq_session_protocol::Validate;
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;
pub(super) fn conflict() -> AppError {
    AppError::Conflict("launch owner journal conflict or unavailable".into())
}
use crate::error::db;
pub async fn record_reserved(
    tx: &mut Transaction<'_, Postgres>,
    binding: &LaunchBinding,
    owner: &LaunchOwner,
    revision: u64,
    digest: &Digest,
) -> Result<(), AppError> {
    record(tx, binding, owner, None, "reserved", revision, digest).await
}
pub async fn record_prepared(
    tx: &mut Transaction<'_, Postgres>,
    binding: &LaunchBinding,
    owner: &LaunchOwner,
    grant: Uuid,
    revision: u64,
    digest: &Digest,
) -> Result<(), AppError> {
    let g = Some(grant);
    record(tx, binding, owner, g, "prepared", revision, digest).await
}
pub(super) async fn record_started(
    tx: &mut Transaction<'_, Postgres>,
    request: &aq_admission::started::Request,
) -> Result<(), AppError> {
    record(
        tx,
        &request.binding,
        &LaunchOwner {
            guardian_id: request.guardian_id,
            launch_id: request.launch_id,
            reservation_id: request.reservation_id,
        },
        Some(request.grant_id),
        "started",
        request.journal_revision,
        &request.journal_digest,
    )
    .await
}
pub(super) async fn record_stopped(
    tx: &mut Transaction<'_, Postgres>,
    request: &aq_admission::stopped::Request,
) -> Result<(), AppError> {
    let (revision, digest) = request.journal().map_err(|_| conflict())?;
    record(
        tx,
        &request.evidence.binding,
        &LaunchOwner {
            guardian_id: request.evidence.guardian_id,
            launch_id: request.evidence.launch_id,
            reservation_id: request.reservation_id,
        },
        Some(request.grant_id().map_err(|_| conflict())?),
        "stopped",
        revision,
        digest,
    )
    .await?;
    revoke(tx, request.evidence.binding.attempt_id).await
}
/// Revoke the attempt's agent credential (0006 §5.4): stop, closure, cancellation, disposition; legacy fixture stop and cancellation via `revoke_if`.
pub(super) async fn revoke(tx: &mut Transaction<'_, Postgres>, a: Uuid) -> Result<(), AppError> {
    revoke_if(tx, a, true).await.map_err(db)
}
pub async fn revoke_if(tx: &mut Transaction<'_, Postgres>, a: Uuid, on: bool) -> sqlx::Result<()> {
    sqlx::query("UPDATE agent_launch_bindings SET revoked=true,credential_verifier=NULL,credential_expires_at_ms=NULL,credential_version=credential_version+1,authority_revision=authority_revision+1 WHERE attempt_id=$1 AND $2").bind(a).bind(on).execute(&mut **tx).await.map(drop)
}
async fn record(
    tx: &mut Transaction<'_, Postgres>,
    binding: &LaunchBinding,
    owner: &LaunchOwner,
    grant: Option<Uuid>,
    phase: &str,
    revision: u64,
    digest: &Digest,
) -> Result<(), AppError> {
    binding.validate().map_err(|_| conflict())?;
    if revision == 0
        || revision > i64::MAX as u64
        || [owner.guardian_id, owner.launch_id, owner.reservation_id]
            .iter()
            .any(Uuid::is_nil)
        || grant.is_some_and(|id| id.is_nil())
    {
        return Err(conflict());
    }
    let body = serde_json::to_value(binding).map_err(|_| AppError::Internal)?;
    let old=sqlx::query("SELECT binding,guardian_id,launch_id,reservation_id,phase,grant_id,revision,digest FROM profiled_owner_journals WHERE attempt_id=$1 FOR UPDATE").bind(binding.attempt_id).fetch_optional(&mut **tx).await.map_err(db)?;
    if let Some(old) = old {
        if old.get::<serde_json::Value, _>("binding") != body
            || old.get::<Uuid, _>("guardian_id") != owner.guardian_id
            || old.get::<Uuid, _>("launch_id") != owner.launch_id
            || old.get::<Uuid, _>("reservation_id") != owner.reservation_id
        {
            return Err(conflict());
        }
        let previous = old.get::<String, _>("phase");
        if previous == phase {
            return if old.get::<Option<Uuid>, _>("grant_id") == grant
                && old.get::<i64, _>("revision") == revision as i64
                && old.get::<String, _>("digest") == digest.as_str()
            {
                Ok(())
            } else {
                Err(conflict())
            };
        }
        if !matches!(
            (previous.as_str(), phase),
            ("reserved", "prepared") | ("prepared", "started") | ("started", "stopped")
        ) || revision as i64 <= old.get::<i64, _>("revision")
        {
            return Err(conflict());
        }
        sqlx::query("UPDATE profiled_owner_journals SET phase=$2,grant_id=$3,revision=$4,digest=$5 WHERE attempt_id=$1").bind(binding.attempt_id).bind(phase).bind(grant).bind(revision as i64).bind(digest.as_str()).execute(&mut **tx).await.map_err(db)?;
    } else {
        if phase != "reserved" {
            return Err(conflict());
        }
        sqlx::query("INSERT INTO profiled_owner_journals(attempt_id,binding,guardian_id,launch_id,reservation_id,phase,grant_id,revision,digest) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)").bind(binding.attempt_id).bind(body).bind(owner.guardian_id).bind(owner.launch_id).bind(owner.reservation_id).bind(phase).bind(grant).bind(revision as i64).bind(digest.as_str()).execute(&mut **tx).await.map_err(db)?;
    }
    Ok(())
}
