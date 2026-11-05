//! Completion consumes the actual private source owner's typed fact. Historical
//! completion may be recorded after freshness expires; only fresh inspections
//! can link bytes into a new offer. Neither path grants execution.
use super::{ReservedExport, canonical, database, unavailable};
use crate::{
    AppState,
    error::AppError,
    source_retention::profiled_base::{RetentionObservation, VerifiedExport},
};
use aq_admission::Digest;
use aq_session_protocol::SessionIdentity;
use aq_worker_protocol::{SourceRole, TransferManifest, Validate};
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

pub async fn finalize(
    state: &AppState,
    token: &ReservedExport,
    fact: &VerifiedExport,
) -> Result<(), AppError> {
    fact.validate_for(token)?;
    let bytes = fact.canonical_bytes()?;
    let digest = Digest::of(&bytes);
    let charged = fact
        .bundle_bytes()
        .checked_add(fact.manifest_bytes().len() as u64)
        .ok_or_else(unavailable)?;
    if charged > token.reserved_bytes() {
        return Err(unavailable());
    }
    let mut tx = state.store.pool.begin().await.map_err(database)?;
    sqlx::query("SET LOCAL lock_timeout='2s'")
        .execute(&mut *tx)
        .await
        .map_err(database)?;
    sqlx::query("SET LOCAL statement_timeout='3s'")
        .execute(&mut *tx)
        .await
        .map_err(database)?;
    sqlx::query("SELECT pg_advisory_xact_lock(719247031)")
        .execute(&mut *tx)
        .await
        .map_err(database)?;
    let row=sqlx::query("SELECT request_bytes,request_digest,reserved_bytes,state,verified_bytes,verified_digest FROM profiled_source_exports WHERE operation_id=$1 FOR UPDATE").bind(token.request().operation_id).fetch_optional(&mut *tx).await.map_err(database)?.ok_or_else(unavailable)?;
    if row.get::<Vec<u8>, _>("request_bytes") != canonical(token.request())?
        || row.get::<String, _>("request_digest") != token.request_digest().as_str()
        || row.get::<i64, _>("reserved_bytes") as u64 != token.reserved_bytes()
    {
        return Err(unavailable());
    }
    if row.get::<String, _>("state") == "verified" {
        if row.get::<Option<Vec<u8>>, _>("verified_bytes").as_deref() != Some(bytes.as_slice())
            || row.get::<Option<String>, _>("verified_digest").as_deref() != Some(digest.as_str())
        {
            return Err(unavailable());
        }
    } else {
        sqlx::query("UPDATE profiled_source_exports SET state='verified',verified_bytes=$2,verified_digest=$3,charged_bytes=$4 WHERE operation_id=$1").bind(token.request().operation_id).bind(&bytes).bind(digest.as_str()).bind(charged as i64).execute(&mut *tx).await.map_err(database)?;
    }
    tx.commit().await.map_err(database)?;
    Ok(())
}
/// The producer holds its current native node/configuration and project fences,
/// and calls this inside the same transaction as the initial sealed offer. The
/// common loader reacquires node→attempt→hot and exact sealed identity checks;
/// this does not substitute for the producer's source/task/selection fence.
pub async fn link_locked(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    session: &SessionIdentity,
    token: &ReservedExport,
    fact: &VerifiedExport,
    retention: &RetentionObservation,
) -> Result<TransferManifest, AppError> {
    use crate::execution::{
        authority::{self, Kind, Policy},
        profiled::Status,
    };
    fact.validate_for(token)?;
    retention.validate_for(token)?;
    let view = authority::load_locked(state, tx, session, Policy::IdentityOnly)
        .await
        .map_err(AppError::from)?;
    if view.kind != Kind::ProfiledV1
        || !view.holds_task
        || view.revoked
        || view.status != Status::Offered
        || view.grant_id.is_some()
    {
        return Err(unavailable());
    }
    let source = view.source.as_ref().ok_or_else(unavailable)?;
    let request = token.request();
    if view.project_id != request.project_id
        || source.repository_id != request.repository_id
        || source.binding_generation != request.binding_generation
        || source.logical_identity != request.logical_identity
        || source.base != request.base
    {
        return Err(unavailable());
    }
    let launch = sqlx::query(
        "SELECT execution_spec,offer_expires_at_ms FROM profiled_launches WHERE attempt_id=$1",
    )
    .bind(view.attempt.attempt_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(database)?;
    let offer_expires = launch.get::<i64, _>("offer_expires_at_ms");
    let spec = aq_admission::execution_spec::decode_profiled_spec(&canonical(
        &launch.get::<serde_json::Value, _>("execution_spec"),
    )?)
    .map_err(|_| unavailable())?;
    if spec.source.project_id != request.project_id
        || spec.source.project_revision != request.project_revision
        || spec.source.repository_id != request.repository_id
        || spec.source.repository_binding_generation != request.binding_generation
        || spec.source.resolved_object.format != aq_admission::ObjectFormat::Sha1
        || spec.source.resolved_object.hex != request.base.hex
    {
        return Err(unavailable());
    }
    let row=sqlx::query("SELECT request_digest,verified_bytes,verified_digest,state FROM profiled_source_exports WHERE operation_id=$1 FOR SHARE").bind(request.operation_id).fetch_optional(&mut **tx).await.map_err(database)?.ok_or_else(unavailable)?;
    let bytes = fact.canonical_bytes()?;
    if row.get::<String, _>("state") != "verified"
        || row.get::<String, _>("request_digest") != token.request_digest().as_str()
        || row.get::<Option<Vec<u8>>, _>("verified_bytes").as_deref() != Some(bytes.as_slice())
        || row.get::<Option<String>, _>("verified_digest").as_deref()
            != Some(Digest::of(&bytes).as_str())
    {
        return Err(unavailable());
    }
    let manifest = TransferManifest {
        transfer_id: request.operation_id,
        role: SourceRole::Base,
        checkpoint: source.base.clone(),
        attempt: view.attempt.clone(),
        resolved_spec_digest: view.execution_spec_digest.clone(),
        source: source.clone(),
        artifact_id: request.operation_id,
        manifest_digest: fact.manifest_digest().as_str().to_owned(),
        bundle_digest: Some(fact.bundle_digest().as_str().to_owned()),
        manifest_bytes: fact.manifest_bytes().len() as u64,
        bundle_bytes: fact.bundle_bytes(),
    };
    manifest.validate().map_err(|_| unavailable())?;
    sqlx::query("INSERT INTO profiled_launch_source_exports(attempt_id,operation_id) VALUES($1,$2) ON CONFLICT(attempt_id) DO NOTHING").bind(view.attempt.attempt_id).bind(request.operation_id).execute(&mut **tx).await.map_err(database)?;
    let linked: Uuid = sqlx::query_scalar(
        "SELECT operation_id FROM profiled_launch_source_exports WHERE attempt_id=$1",
    )
    .bind(view.attempt.attempt_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(database)?;
    if linked != request.operation_id {
        return Err(unavailable());
    }
    let existing=sqlx::query("SELECT node_id,operation_id,retention_handle,reserved_bytes,manifest,profiled_export_operation_id FROM source_bases WHERE attempt_id=$1 FOR UPDATE").bind(view.attempt.attempt_id).fetch_optional(&mut **tx).await.map_err(database)?;
    let body = serde_json::to_value(&manifest).map_err(|_| unavailable())?;
    if let Some(old) = existing {
        if old.get::<Uuid, _>("node_id") != view.identity.node_id
            || old.get::<Uuid, _>("operation_id") != request.operation_id
            || old.get::<Uuid, _>("retention_handle") != request.source_retention_handle
            || old.get::<i64, _>("reserved_bytes") != 0
            || old.get::<Option<Uuid>, _>("profiled_export_operation_id")
                != Some(request.operation_id)
            || old.get::<Option<serde_json::Value>, _>("manifest") != Some(body)
        {
            return Err(unavailable());
        }
    } else {
        sqlx::query("INSERT INTO source_bases(attempt_id,node_id,operation_id,retention_handle,reserved_bytes,manifest,profiled_export_operation_id) VALUES($1,$2,$3,$4,0,$5,$3)").bind(view.attempt.attempt_id).bind(view.identity.node_id).bind(request.operation_id).bind(request.source_retention_handle).bind(body).execute(&mut **tx).await.map_err(database)?;
    }
    let now = u64::try_from((state.clock)().timestamp_millis()).map_err(|_| unavailable())?;
    if now == 0
        || now < retention.observed_at_ms()
        || now >= retention.expires_at_ms()
        || offer_expires <= 0
        || now >= offer_expires as u64
        || now < spec.source.observed_at_ms
        || now >= spec.source.expires_at_ms
    {
        return Err(unavailable());
    }
    Ok(manifest)
}

/// Immutable stored facts only, never cached/current execution authorization.
pub struct LinkedExport {
    token: ReservedExport,
    fact: VerifiedExport,
    manifest: TransferManifest,
}
impl LinkedExport {
    pub(crate) fn into_parts(self) -> (ReservedExport, VerifiedExport) {
        (self.token, self.fact)
    }
    pub fn token(&self) -> &ReservedExport {
        &self.token
    }
    pub fn fact(&self) -> &VerifiedExport {
        &self.fact
    }
    pub fn manifest(&self) -> &TransferManifest {
        &self.manifest
    }
}
pub async fn load_linked_locked(
    tx: &mut Transaction<'_, Postgres>,
    attempt: Uuid,
) -> Result<LinkedExport, AppError> {
    let row=sqlx::query("SELECT e.request_bytes,e.request_digest,e.reserved_bytes,e.state,e.verified_bytes,e.verified_digest,b.manifest,b.profiled_export_operation_id FROM profiled_launch_source_exports x JOIN profiled_source_exports e USING(operation_id) JOIN source_bases b ON b.attempt_id=x.attempt_id AND b.profiled_export_operation_id=e.operation_id WHERE x.attempt_id=$1 FOR SHARE OF e,b").bind(attempt).fetch_optional(&mut **tx).await.map_err(database)?.ok_or_else(unavailable)?;
    let request_bytes: Vec<u8> = row.get("request_bytes");
    let request: super::Request =
        serde_json::from_slice(&request_bytes).map_err(|_| unavailable())?;
    request.validate().map_err(|_| unavailable())?;
    let digest = request.digest().map_err(|_| unavailable())?;
    if canonical(&request)? != request_bytes
        || digest.as_str() != row.get::<String, _>("request_digest")
        || row.get::<String, _>("state") != "verified"
    {
        return Err(unavailable());
    }
    let token = ReservedExport {
        request,
        request_digest: digest,
        reserved_bytes: row.get::<i64, _>("reserved_bytes") as u64,
    };
    let bytes: Vec<u8> = row
        .get::<Option<Vec<u8>>, _>("verified_bytes")
        .ok_or_else(unavailable)?;
    if Some(Digest::of(&bytes).as_str())
        != row.get::<Option<String>, _>("verified_digest").as_deref()
    {
        return Err(unavailable());
    }
    let fact = VerifiedExport::decode_stored(&token, &bytes)?;
    let manifest: TransferManifest = serde_json::from_value(
        row.get::<Option<serde_json::Value>, _>("manifest")
            .ok_or_else(unavailable)?,
    )
    .map_err(|_| unavailable())?;
    manifest.validate().map_err(|_| unavailable())?;
    if manifest.attempt.attempt_id != attempt
        || manifest.artifact_id != token.request().operation_id
        || manifest.manifest_digest != fact.manifest_digest().as_str()
        || manifest.bundle_digest.as_deref() != Some(fact.bundle_digest().as_str())
        || manifest.bundle_bytes != fact.bundle_bytes()
        || manifest.manifest_bytes != fact.manifest_bytes().len() as u64
    {
        return Err(unavailable());
    }
    Ok(LinkedExport {
        token,
        fact,
        manifest,
    })
}
