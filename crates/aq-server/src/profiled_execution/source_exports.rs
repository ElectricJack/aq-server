//! Pre-offer source export ownership. A token is minted only after durable quota
//! reservation; the I/O owner cannot turn an arbitrary request into a token.
use aq_admission::{ArtifactRef, Digest};
use aq_session_protocol::Validate;
use aq_worker_protocol::{ObjectAlgorithm, ObjectId};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub schema_version: u32,
    pub operation_id: Uuid,
    pub project_id: Uuid,
    pub project_revision: u64,
    pub repository_id: Uuid,
    pub logical_identity: String,
    pub binding_generation: u64,
    pub base: ObjectId,
    pub source_retention_handle: Uuid,
    pub source_observation: ArtifactRef,
}
impl Request {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.schema_version != 1
            || [
                self.operation_id,
                self.project_id,
                self.repository_id,
                self.source_retention_handle,
            ]
            .iter()
            .any(Uuid::is_nil)
            || self.project_revision == 0
            || self.project_revision > i64::MAX as u64
            || self.binding_generation == 0
            || self.binding_generation > i64::MAX as u64
            || self.logical_identity.trim().is_empty()
            || self.logical_identity.len() > 512
            || self.logical_identity.chars().any(char::is_control)
            || self.base.algorithm != ObjectAlgorithm::Sha1
            || self.base.hex.len() != 40
            || !self.base.hex.bytes().any(|b| b != b'0')
            || !self
                .base
                .hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || self.source_observation.id.as_str().len() > 512
            || self.source_observation.version.as_str().len() > 512
            || self.source_observation.id.validate().is_err()
            || self.source_observation.version.validate().is_err()
        {
            return Err("invalid pre-offer source export request");
        }
        Ok(())
    }
    pub fn digest(&self) -> Result<Digest, &'static str> {
        self.validate()?;
        aq_selection::digest(self).map_err(|_| "source export request encoding")
    }
}
/// No constructor or deserializer is exposed to production callers. The future
/// reserve service owns construction after its quota transaction commits.
pub struct ReservedExport {
    request: Request,
    request_digest: Digest,
    reserved_bytes: u64,
}
impl ReservedExport {
    pub fn request(&self) -> &Request {
        &self.request
    }
    pub fn request_digest(&self) -> &Digest {
        &self.request_digest
    }
    pub fn reserved_bytes(&self) -> u64 {
        self.reserved_bytes
    }
}
/// Explicit component-test capability; not a durable reservation or a route.
#[cfg(any(test, feature = "test-support"))]
pub fn synthetic_reserved_export(
    request: Request,
    reserved_bytes: u64,
) -> Result<ReservedExport, &'static str> {
    let request_digest = request.digest()?;
    if reserved_bytes <= 1_048_576 || reserved_bytes > i64::MAX as u64 {
        return Err("invalid synthetic source reservation bound");
    }
    Ok(ReservedExport {
        request,
        request_digest,
        reserved_bytes,
    })
}

pub enum Reservation {
    Pending(ReservedExport),
    /// Retained original fact only. A fresh owner inspection is required before
    /// an offer; historical readback neither re-exports nor extends freshness.
    Verified {
        reservation: ReservedExport,
        canonical_verified: Vec<u8>,
    },
}
#[track_caller]
fn unavailable() -> crate::error::AppError {
    tracing::debug!(target: "aq_refusal", at = %std::panic::Location::caller(), "profiled refusal");
    crate::error::AppError::Conflict("source export ownership unavailable or inconsistent".into())
}
/// A refusal of a fresh export's policy or observation, logged with its code
/// (live acceptance D10: these refusals were silent).
fn refused(reason: &'static str) -> crate::error::AppError {
    tracing::warn!(reason, "source export refused");
    unavailable()
}
fn database(_: sqlx::Error) -> crate::error::AppError {
    crate::error::AppError::Internal
}
fn canonical(value: &impl Serialize) -> Result<Vec<u8>, crate::error::AppError> {
    let mut value = serde_json::to_value(value).map_err(|_| unavailable())?;
    value.sort_all_objects();
    serde_json::to_vec(&value).map_err(|_| unavailable())
}
/// Trusted central owner only. Commits a finite quota reservation before it can
/// return the opaque capability accepted by the private Git I/O owner.
/// Historical original-ID reconciliation only. The trusted scheduler supplies
/// its owned project; no current admission permission or new scope is minted.
pub(crate) async fn recover_existing(
    state: &crate::AppState,
    operation: Uuid,
    project: Uuid,
) -> Result<Option<Reservation>, crate::error::AppError> {
    if operation.is_nil() || project.is_nil() {
        return Err(unavailable());
    }
    let raw: Option<Vec<u8>> = sqlx::query_scalar(
        "SELECT request_bytes FROM profiled_source_exports WHERE operation_id=$1 AND project_id=$2",
    )
    .bind(operation)
    .bind(project)
    .fetch_optional(&state.store.pool)
    .await
    .map_err(database)?;
    let Some(raw) = raw else { return Ok(None) };
    if raw.len() > 8192 {
        return Err(unavailable());
    }
    let request: Request = serde_json::from_slice(&raw).map_err(|_| unavailable())?;
    if request.operation_id != operation || request.project_id != project {
        return Err(unavailable());
    }
    reserve(state, request).await.map(Some)
}
pub async fn reserve(
    state: &crate::AppState,
    request: Request,
) -> Result<Reservation, crate::error::AppError> {
    use sqlx::Row;
    request.validate().map_err(|_| unavailable())?;
    let request_bytes = canonical(&request)?;
    let request_digest = request.digest().map_err(|_| unavailable())?;
    if request_bytes.len() > 8192 {
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
    // Historical verified facts are read before current admission and quota.
    // They cannot mint a different operation or execute any physical writer.
    if let Some(row)=sqlx::query("SELECT request_bytes,request_digest,reserved_bytes,state,verified_bytes,verified_digest FROM profiled_source_exports WHERE operation_id=$1").bind(request.operation_id).fetch_optional(&mut *tx).await.map_err(database)? {
        if row.get::<Vec<u8>,_>("request_bytes")!=request_bytes || row.get::<String,_>("request_digest")!=request_digest.as_str() {return Err(unavailable());}
        if row.get::<String,_>("state")=="verified" {
            let bytes=row.get::<Option<Vec<u8>>,_>("verified_bytes").ok_or_else(unavailable)?;
            if bytes.len()>4_194_304 || Some(Digest::of(&bytes).as_str())!=row.get::<Option<String>,_>("verified_digest").as_deref() {return Err(unavailable());}
            let token=ReservedExport{request,request_digest,reserved_bytes:row.get::<i64,_>("reserved_bytes") as u64};
            tx.commit().await.map_err(database)?;
            return Ok(Reservation::Verified{reservation:token,canonical_verified:bytes});
        }
        if row.get::<String,_>("state")!="pending" {return Err(unavailable());}
        // The original durable side-effect intent may be reconciled even after
        // admission policy retirement. It remains charged, fixed-scope and is
        // never offer authority; the Git owner still checks original scope.
        let token=ReservedExport{request,request_digest,reserved_bytes:row.get::<i64,_>("reserved_bytes") as u64};
        tx.commit().await.map_err(database)?;
        return Ok(Reservation::Pending(token));
    }
    let runtime = state.source_retention.as_ref().ok_or_else(unavailable)?;
    let reserved_bytes = runtime
        .config
        .max_bundle_bytes
        .checked_add(1_048_576)
        .ok_or_else(unavailable)?;
    let config = state
        .config
        .profiled_execution
        .as_deref()
        .ok_or_else(unavailable)?;
    super::policies::fence_configuration_locked(&mut tx, config).await?;
    let project = sqlx::query(
        "SELECT revision,binding FROM project_execution_bindings WHERE project_id=$1 FOR SHARE",
    )
    .bind(request.project_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(database)?
    .ok_or_else(unavailable)?;
    let binding: crate::project_bindings::domain::ProjectBinding =
        serde_json::from_value(project.get("binding")).map_err(|_| unavailable())?;
    if !binding.enabled
        || project.get::<i64, _>("revision") as u64 != request.project_revision
        || binding.repository_id != request.repository_id
        || binding.repository_binding_generation != request.binding_generation
    {
        return Err(unavailable());
    }
    let configured = config
        .project_policy_record(request.project_id)
        .map_err(|_| unavailable())?;
    let installed: serde_json::Value = sqlx::query_scalar(
        "SELECT policy FROM admission_source_policies WHERE project_id=$1 FOR SHARE",
    )
    .bind(request.project_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(database)?
    .ok_or_else(unavailable)?;
    // D1: a named-ref policy may hydrate from the recorded source head's
    // candidate handle, and then only for exactly that published commit.
    let head_handle = match &configured.source {
        aq_admission::SourceSelection::NamedRef { reference } => {
            crate::profiled_execution::settlement::publication::current_head(
                &mut *tx,
                request.project_id,
                request.repository_id,
                request.binding_generation,
                reference.as_str(),
            )
            .await
            .map_err(|_| unavailable())?
            .filter(|head| head.oid == request.base.hex)
            .map(|head| head.retention_handle)
        }
        aq_admission::SourceSelection::Immutable { .. } => None,
    };
    let policy_handle = configured.retention_handle == request.source_retention_handle
        || head_handle == Some(request.source_retention_handle);
    if !configured.enabled || canonical(&configured)? != canonical(&installed)? {
        return Err(refused("source_export_policy_not_installed"));
    }
    let observation_id:Uuid=sqlx::query_scalar("SELECT artifact_id FROM admission_artifacts WHERE owner_key=$1 AND owner_kind='source_retention' AND descriptor->>'kind'='context' AND descriptor->'artifact'=$2 ORDER BY artifact_id LIMIT 1").bind(request.project_id.to_string()).bind(serde_json::to_value(&request.source_observation).map_err(|_|unavailable())?).fetch_optional(&mut *tx).await.map_err(database)?.ok_or_else(unavailable)?;
    let retained = super::artifacts::load_locked(&mut tx, observation_id).await?;
    if retained.project_id != request.project_id
        || retained.owner != super::artifacts::OwnerKind::SourceRetention
        || retained.descriptor.artifact != request.source_observation
        || retained.descriptor.kind != aq_admission::ArtifactKind::Context
    {
        return Err(unavailable());
    }
    let value: serde_json::Value =
        serde_json::from_slice(&retained.bytes).map_err(|_| unavailable())?;
    // Spec 0007 S7 (D5): a handoff successor exports its predecessor's recorded
    // handoff candidate, held by the candidate handle, as an immutable source.
    let predecessor = value["evidence"]["handoff"]["predecessor"].as_str();
    let scope = (
        request.project_id,
        request.repository_id,
        request.binding_generation,
    );
    let handoff = match predecessor.and_then(|p| Uuid::parse_str(p).ok()) {
        Some(p) => {
            super::handoff::export_is_handoff(&mut tx, p, &request).await?
                && aq_machine_control::export_scope(&mut tx, p).await? == Some(scope)
        }
        None => false,
    };
    if !policy_handle && !handoff {
        return Err(refused("source_export_handle_not_configured"));
    }
    use aq_admission::SourceSelection::Immutable;
    let observation: aq_admission::SourceAdmission =
        serde_json::from_value(value["admission"].clone()).map_err(|_| unavailable())?;
    observation.validate().map_err(|_| unavailable())?;
    if observation.project_id != request.project_id
        || observation.project_revision != request.project_revision
        || observation.repository_id != request.repository_id
        || observation.repository_binding_generation != request.binding_generation
        || observation.policy != configured.artifact().map_err(|_| unavailable())?
        || observation.resolved_object.format != aq_admission::ObjectFormat::Sha1
        || observation.resolved_object.hex != request.base.hex
        || (!handoff
            && serde_json::to_value(&observation.selection).map_err(|_| unavailable())?
                != serde_json::to_value(&configured.source).map_err(|_| unavailable())?)
        || (handoff
            && !matches!(&observation.selection, Immutable { object } if object.hex == request.base.hex))
        || value["retention_handle"] != serde_json::json!(request.source_retention_handle)
    {
        return Err(refused("source_export_observation_mismatch"));
    }
    // Same shared owner lock as legacy transfers/bases; unknown exports remain
    // fully charged. No I/O or file reading occurs inside this transaction.
    sqlx::query("SELECT pg_advisory_xact_lock(719247031)")
        .execute(&mut *tx)
        .await
        .map_err(database)?;
    let prior=sqlx::query("SELECT request_bytes,request_digest,reserved_bytes,state FROM profiled_source_exports WHERE operation_id=$1 FOR UPDATE").bind(request.operation_id).fetch_optional(&mut *tx).await.map_err(database)?;
    let now = u64::try_from((state.clock)().timestamp_millis()).map_err(|_| unavailable())?;
    let reserved = if let Some(row) = prior {
        if row.get::<Vec<u8>, _>("request_bytes") != request_bytes
            || row.get::<String, _>("request_digest") != request_digest.as_str()
            || row.get::<String, _>("state") != "pending"
        {
            return Err(unavailable());
        }
        row.get::<i64, _>("reserved_bytes") as u64
    } else {
        retained
            .descriptor
            .verify(&retained.bytes, now)
            .map_err(|_| unavailable())?;
        if now == 0 || now < observation.observed_at_ms || now >= observation.expires_at_ms {
            return Err(unavailable());
        }
        let used = aq_history_caps::staging_used(&mut tx, now as i64)
            .await
            .map_err(database)?;
        if used < 0
            || (used as u64)
                .checked_add(reserved_bytes)
                .is_none_or(|n| n > runtime.config.max_staging_bytes)
        {
            return Err(crate::error::AppError::PayloadTooLarge(
                "source staging quota exhausted".into(),
            ));
        }
        let kind = aq_history_caps::Kind::SourceExports;
        let capacity = aq_history_caps::usage(&mut tx, kind, Some(request.project_id), now)
            .await
            .map_err(database)?;
        let incoming = request_bytes.len() as i64 + 4_194_304;
        if capacity.total_rows >= 4096
            || capacity.scope_rows >= 256
            || capacity
                .total_bytes
                .checked_add(incoming)
                .is_none_or(|n| n > 268_435_456)
            || capacity
                .scope_bytes
                .checked_add(incoming)
                .is_none_or(|n| n > 67_108_864)
        {
            return Err(crate::error::AppError::PayloadTooLarge(
                "source export metadata quota exhausted".into(),
            ));
        }
        sqlx::query("INSERT INTO profiled_source_exports(operation_id,project_id,request_bytes,request_digest,reserved_bytes,charged_bytes,state) VALUES($1,$2,$3,$4,$5,$5,'pending')").bind(request.operation_id).bind(request.project_id).bind(&request_bytes).bind(request_digest.as_str()).bind(reserved_bytes as i64).execute(&mut *tx).await.map_err(database)?;
        aq_history_caps::stamp_export(&mut tx, request.operation_id, now)
            .await
            .map_err(database)?;
        reserved_bytes
    };
    tx.commit().await.map_err(database)?;
    Ok(Reservation::Pending(ReservedExport {
        request,
        request_digest,
        reserved_bytes: reserved,
    }))
}

mod completion;
pub use completion::{LinkedExport, finalize, link_locked, load_linked_locked};
