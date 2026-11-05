//! AQ-issued `ExecutionPermit`s for successor launch (aq-session-host 0.1.7
//! `SessionHost::launch`) and the server side of `reconcile_effects` (H5
//! follow-up, crisp-ember-25).
//!
//! A permit binds the attempt's frozen `SessionIdentity` and the launch
//! manifest digest to a bounded window on the machine's boot clock. The server
//! never signs: the permit is trusted because it is delivered over the
//! authenticated worker channel and because `SessionHost::launch` re-validates
//! identity, digest and window locally. Sequences are monotonic per attempt and
//! the manifest digest is pinned by the attempt record, or by the first permit
//! when the record carries none.
use crate::{
    AppState,
    admin_api::ApiJson,
    error::AppError,
    execution::authority::Kind,
    lifecycle_artifacts::{authenticate, bad, conflict, internal, record, replay, sha, view},
};
use aq_admission_sandbox::sandbox::RunnerOperation;
use aq_lifecycle_api::worker_lifecycle as lc;
use aq_session_host::{ExecutionPermit, MAX_PERMIT_MS};
use aq_session_protocol::{ArtifactRef, Digest, SessionIdentity, Text};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::HeaderMap,
    routing::post,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
type Result<T> = std::result::Result<T, AppError>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermitRequest {
    pub operation_id: Uuid,
    pub attempt_id: Uuid,
    pub manifest_digest: Digest,
    /// The machine boot the permit window is measured on. Must equal the
    /// attempt session's boot_id: a permit never outlives a reboot.
    pub machine_boot_id: Uuid,
    pub now_boot_ms: u64,
    pub span_ms: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermitIssued {
    pub permit_id: Uuid,
    pub permit: ExecutionPermit,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectsRequest {
    pub attempt_id: Uuid,
    pub operation_ids: Vec<Uuid>,
}
pub fn routes(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/v1/execution/permits", post(issue))
        .route("/v1/worker/lifecycle/effects", post(effects))
        .layer(DefaultBodyLimit::max(16384))
        .with_state(state)
}
async fn issue(
    State(s): State<AppState>,
    headers: HeaderMap,
    ApiJson(r): ApiJson<PermitRequest>,
) -> Result<Json<PermitIssued>> {
    if r.span_ms == 0 || r.span_ms > MAX_PERMIT_MS {
        return Err(bad("permit span out of range"));
    }
    let expires = r
        .now_boot_ms
        .checked_add(r.span_ms)
        .ok_or_else(|| bad("permit window overflow"))?;
    let n = authenticate(&s, &headers, RunnerOperation::FetchExecutionMaterial).await?;
    let mut tx = s.store.pool.begin().await.map_err(internal)?;
    let a = view(&s, &mut tx, &n, r.attempt_id).await?;
    let payload = sha(&serde_json::to_vec(&r).map_err(internal)?);
    if let Some(old) = replay(&mut tx, n.id(), r.operation_id, &payload).await? {
        return Ok(Json(old));
    }
    match &a.session {
        SessionIdentity::Remote {
            node_id, boot_id, ..
        } if *node_id == n.id() && *boot_id == r.machine_boot_id => {}
        _ => {
            return Err(AppError::Forbidden("permit session boot mismatch".into()));
        }
    }
    if a.revoked || a.stopped || a.cancellation_id.is_some() || a.retention_released {
        return Err(conflict("attempt is not launchable"));
    }
    if a.kind == Kind::ProfiledV1 && a.grant_id.is_none() {
        return Err(AppError::Forbidden(
            "permit requires an issued execution grant".into(),
        ));
    }
    let now = crate::execution::api::now(&s);
    if a.execution_deadline_ms.is_some_and(|d| now >= d) {
        return Err(conflict("execution deadline passed"));
    }
    if a.launch_manifest_digest
        .as_ref()
        .is_some_and(|d| d != &r.manifest_digest)
    {
        return Err(AppError::Forbidden(
            "launch manifest digest mismatch".into(),
        ));
    }
    // The attempt row is locked by `view`, so the sequence is serialized.
    let last: Option<(String, i64)> = sqlx::query_as(
        "SELECT manifest_digest,sequence FROM execution_permits WHERE attempt_id=$1 ORDER BY sequence DESC LIMIT 1",
    )
    .bind(r.attempt_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(internal)?;
    let sequence = match &last {
        Some((digest, _)) if digest != r.manifest_digest.as_str() => {
            return Err(conflict("permit manifest digest is pinned"));
        }
        Some((_, seq)) => (*seq).max(0) as u64 + 1,
        None => 1,
    };
    let permit = ExecutionPermit {
        session: a.session.clone(),
        manifest_digest: r.manifest_digest.clone(),
        sequence,
        issued_boot_ms: r.now_boot_ms,
        expires_boot_ms: expires,
    };
    permit
        .validate_at(&a.session, &r.manifest_digest, r.now_boot_ms)
        .map_err(internal)?;
    let permit_id = Uuid::new_v4();
    sqlx::query("INSERT INTO execution_permits(permit_id,attempt_id,node_id,sequence,manifest_digest,permit,issued_at_ms) VALUES($1,$2,$3,$4,$5,$6::jsonb,$7)")
        .bind(permit_id).bind(r.attempt_id).bind(n.id()).bind(sequence as i64)
        .bind(r.manifest_digest.as_str()).bind(serde_json::to_string(&permit).map_err(internal)?)
        .bind(now as i64)
        .execute(&mut *tx).await.map_err(internal)?;
    let out = PermitIssued { permit_id, permit };
    record(&mut tx, n.id(), r.operation_id, &payload, &out).await?;
    tx.commit().await.map_err(internal)?;
    Ok(Json(out))
}
/// Server side of `reconcile_effects`: which of the caller's operations have a
/// durable response in the server ledgers. All present gives `Reconciled` with
/// a deterministic receipt over the sorted ids; otherwise `Pending` names the
/// missing ones so the worker retries exactly those.
async fn effects(
    State(s): State<AppState>,
    headers: HeaderMap,
    ApiJson(r): ApiJson<EffectsRequest>,
) -> Result<Json<lc::EffectsObservation>> {
    let mut ids = r.operation_ids.clone();
    ids.sort();
    ids.dedup();
    if ids.is_empty() || ids.len() != r.operation_ids.len() || ids.len() > lc::MAX_REFS {
        return Err(bad("operation ids must be unique, non-empty and bounded"));
    }
    let n = authenticate(&s, &headers, RunnerOperation::Observe).await?;
    let mut tx = s.store.pool.begin().await.map_err(internal)?;
    let _a = view(&s, &mut tx, &n, r.attempt_id).await?;
    let present: Vec<Uuid> = sqlx::query_scalar(
        "SELECT operation_id FROM execution_operations WHERE node_id=$1 AND operation_id=ANY($2) UNION SELECT operation_id FROM source_transfer_operations WHERE node_id=$1 AND operation_id=ANY($2)",
    )
    .bind(n.id())
    .bind(&ids)
    .fetch_all(&mut *tx)
    .await
    .map_err(internal)?;
    tx.commit().await.map_err(internal)?;
    let missing: Vec<Uuid> = ids
        .iter()
        .copied()
        .filter(|id| !present.contains(id))
        .collect();
    if !missing.is_empty() {
        return Ok(Json(lc::EffectsObservation::Pending {
            operation_ids: missing,
        }));
    }
    let joined = ids
        .iter()
        .map(Uuid::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    let receipt = ArtifactRef {
        id: Text::new("aq-effects-ledger").map_err(internal)?,
        version: Text::new("1").map_err(internal)?,
        digest: Digest::of(joined.as_bytes()),
    };
    Ok(Json(lc::EffectsObservation::Reconciled { receipt }))
}
