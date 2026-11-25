//! Arbitrary-artifact retention for the CON-03 lifecycle protocol (H5
//! follow-up, crisp-ember-25). A worker uploads one artifact (checkpoint,
//! transcript, result or source) in exact-offset chunks and commits it. The
//! commit returns a lifecycle `RetentionReceipt` whose `binding_digest` is the
//! canonical digest of the uploading attempt's lifecycle `Binding`, so
//! `aq_lifecycle_protocol::validate_retention` accepts it unchanged.
//!
//! Upload completion is never retention: only the committed row with a receipt
//! is retained. A successor attempt on the same (project, task, lineage) may
//! fetch retained bytes; nothing else can. Every mutating request is an
//! idempotent operation in `execution_operations` and replays byte-for-byte.
use crate::{
    AppState,
    admin_api::ApiJson,
    agent_access::waits,
    error::AppError,
    execution::authority::{AuthorityError, AuthorityView, Policy, load_attempt_locked},
    source_retention::Runtime,
    worker_api::{AuthenticatedNode, lock_current_node},
};
use aq_admission_sandbox::sandbox::RunnerOperation;
use aq_lifecycle_api::artifact_upload::Upload;
pub use aq_lifecycle_api::artifact_upload::{
    ArtifactTransferStatus, BeginArtifact, CommitArtifact,
};
use aq_lifecycle_api::worker_lifecycle as lc;
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Path as RoutePath, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::Response,
    routing::{get, post, put},
};
use serde::Deserialize;
mod operation;
use aq_history_caps::retained_artifacts as inventory;
pub(crate) use operation::Operation;
use sha2::{Digest as _, Sha256};
use sqlx::{Postgres, Row, Transaction};
use std::{path::PathBuf, sync::Arc};
use uuid::Uuid;
type Result<T> = std::result::Result<T, AppError>;
pub(crate) fn bad(s: &str) -> AppError {
    AppError::BadRequest(s.into())
}
pub(crate) fn conflict(s: &str) -> AppError {
    AppError::Conflict(s.into())
}
pub(crate) fn internal<E: std::fmt::Display>(e: E) -> AppError {
    tracing::error!(error=%e,"lifecycle artifact failure");
    AppError::Internal
}
pub(crate) fn sha(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}
fn chunk_size() -> usize {
    256 * 1024
}
fn admit_bulk(rt: &Runtime) -> Result<tokio::sync::SemaphorePermit<'_>> {
    rt.bulk()
        .try_acquire()
        .map_err(|_| AppError::Maintenance("artifact bulk capacity exhausted".into()))
}
pub(crate) fn runtime(s: &AppState) -> Result<Arc<Runtime>> {
    s.source_retention
        .clone()
        .ok_or_else(|| AppError::UpdatesUnavailable("artifact retention is not configured".into()))
}
fn artifacts_dir(rt: &Runtime) -> PathBuf {
    rt.config.staging_root.join("artifacts")
}
fn part_path(rt: &Runtime, transfer: Uuid) -> PathBuf {
    artifacts_dir(rt).join(format!("{transfer}.part"))
}
fn retained_path(rt: &Runtime, receipt: Uuid) -> PathBuf {
    artifacts_dir(rt).join(receipt.to_string())
}
async fn remove_quietly(path: &std::path::Path) -> Result<()> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(internal(e)),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChunkQuery {
    operation_id: Uuid,
    offset: u64,
    digest: String,
}
impl ChunkQuery {
    fn validate(&self, bytes: &[u8]) -> Result<()> {
        if !(1..=chunk_size()).contains(&bytes.len()) || self.digest != sha(bytes) {
            return Err(bad("invalid chunk size or digest"));
        }
        Ok(())
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FetchQuery {
    attempt_id: Uuid,
    #[serde(default)]
    offset: u64,
    #[serde(default = "chunk_size")]
    limit: usize,
}
pub fn routes(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/v1/execution/artifacts/begin", post(begin))
        .route("/v1/execution/artifacts/{transfer}/data", put(chunk))
        .route("/v1/execution/artifacts/{transfer}/commit", post(commit))
        .route("/v1/execution/artifacts/{transfer}/status", get(status))
        .route("/v1/execution/artifacts/retained/{receipt}", get(fetch))
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .with_state(state)
}

pub(crate) async fn authenticate(
    s: &AppState,
    headers: &HeaderMap,
    operation: RunnerOperation,
) -> Result<AuthenticatedNode> {
    crate::delegated_permits::runtime::authenticate_runner(s, headers, operation, None).await
}
// HTTP-only credential fence; lock order is Node -> Attempt -> Transfer. The
// IdentityOnly policy admits late (stopped, expired) attempts: a parked attempt
// still retains and a successor still fetches.
pub(crate) async fn view(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    n: &AuthenticatedNode,
    attempt: Uuid,
) -> Result<AuthorityView> {
    if n.delegated()
        .is_some_and(|d| d.current.record.binding.attempt_id != attempt)
    {
        return Err(AppError::Forbidden("artifact attempt mismatch".into()));
    }
    lock_current_node(tx, n).await?;
    load_attempt_locked(state, tx, n.id(), attempt, Policy::IdentityOnly)
        .await
        .map_err(|e| match e {
            AuthorityError::NotFound | AuthorityError::Identity => {
                AppError::Forbidden("attempt is not owned by this node".into())
            }
            other => other.into(),
        })
}
pub(crate) fn binding(view: &AuthorityView) -> Result<lc::Binding> {
    waits::binding_for(view)
        .map_err(|_| AppError::Forbidden("attempt has no lifecycle binding".into()))
}
async fn transfer(tx: &mut Transaction<'_, Postgres>, node: Uuid, id: Uuid) -> Result<Upload> {
    let r=sqlx::query("SELECT receipt_id,artifact::text AS artifact,role,binding::text AS binding,declared_bytes,upload_offset,state,receipt::text AS receipt FROM retained_artifacts WHERE transfer_id=$1 AND node_id=$2 FOR UPDATE").bind(id).bind(node).fetch_optional(&mut **tx).await.map_err(internal)?.ok_or_else(||AppError::NotFound("artifact transfer not found".into()))?;
    Ok(Upload {
        receipt_id: r.get("receipt_id"),
        artifact: serde_json::from_str(r.get("artifact")).map_err(internal)?,
        role: serde_json::from_value(serde_json::Value::String(r.get::<String, _>("role")))
            .map_err(internal)?,
        binding: serde_json::from_str(r.get("binding")).map_err(internal)?,
        bytes: r.get::<i64, _>("declared_bytes") as u64,
        offset: r.get::<i64, _>("upload_offset") as u64,
        state: r.get("state"),
        receipt: r
            .get::<Option<&str>, _>("receipt")
            .map(serde_json::from_str)
            .transpose()
            .map_err(internal)?,
    })
}
async fn owner_attempt(tx: &mut Transaction<'_, Postgres>, node: Uuid, id: Uuid) -> Result<Uuid> {
    sqlx::query_scalar(
        "SELECT attempt_id FROM retained_artifacts WHERE transfer_id=$1 AND node_id=$2",
    )
    .bind(id)
    .bind(node)
    .fetch_optional(&mut **tx)
    .await
    .map_err(internal)?
    .ok_or_else(|| AppError::NotFound("artifact transfer not found".into()))
}
fn role_name(role: &lc::ArtifactRole) -> Result<String> {
    match serde_json::to_value(role).map_err(internal)? {
        serde_json::Value::String(s) => Ok(s),
        _ => Err(AppError::Internal),
    }
}

async fn reserve(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    r: &BeginArtifact,
    b: &lc::Binding,
) -> Result<ArtifactTransferStatus> {
    let transfer_id = Uuid::new_v4();
    let receipt_id = Uuid::new_v4();
    let binding_digest = lc::canonical_digest(b).map_err(internal)?;
    sqlx::query("INSERT INTO retained_artifacts(transfer_id,receipt_id,node_id,attempt_id,project_id,task_id,lineage_id,binding,binding_digest,role,artifact,declared_bytes) VALUES($1,$2,$3,$4,$5,$6,$7,$8::jsonb,$9,$10,$11::jsonb,$12)")
        .bind(transfer_id).bind(receipt_id).bind(node).bind(r.attempt_id)
        .bind(b.project_id).bind(b.task_id).bind(b.lineage_id)
        .bind(serde_json::to_string(b).map_err(internal)?).bind(binding_digest.as_str())
        .bind(role_name(&r.role)?).bind(serde_json::to_string(&r.artifact).map_err(internal)?)
        .bind(r.bytes as i64)
        .execute(&mut **tx).await.map_err(internal)?;
    Ok(ArtifactTransferStatus::uploading(transfer_id, r.bytes))
}
async fn begin(
    State(s): State<AppState>,
    headers: HeaderMap,
    ApiJson(r): ApiJson<BeginArtifact>,
) -> Result<Json<ArtifactTransferStatus>> {
    lc::artifact(&r.artifact).map_err(|_| bad("invalid artifact reference"))?;
    if r.bytes == 0 {
        return Err(bad("artifact bytes required"));
    }
    let n = authenticate(&s, &headers, RunnerOperation::UploadArtifact).await?;
    let rt = runtime(&s)?;
    let _bulk = admit_bulk(&rt)?;
    let mut tx = s.store.pool.begin().await.map_err(internal)?;
    let a = view(&s, &mut tx, &n, r.attempt_id).await?;
    let b = binding(&a)?;
    let operation = Operation::new(
        n.id(),
        r.operation_id,
        sha(&serde_json::to_vec(&r).map_err(internal)?),
    );
    if let Some(old) = operation.replay(&mut tx).await? {
        return Ok(Json(old));
    }
    if a.retention_released {
        return Err(conflict("attempt retention has already been released"));
    }
    if r.bytes > rt.config.max_bundle_bytes {
        return Err(AppError::PayloadTooLarge("artifact limit exceeded".into()));
    }
    inventory::lock(&mut tx).await.map_err(internal)?;
    let now = crate::execution::api::now(&s);
    cleanup(&rt, &mut tx, now).await?;
    if let Some(reason) = inventory::capacity(&mut tx, r.bytes, rt.config.max_staging_bytes)
        .await
        .map_err(internal)?
    {
        return Err(AppError::PayloadTooLarge(reason.into()));
    }
    let out = reserve(&mut tx, n.id(), &r, &b).await?;
    operation.record(&mut tx, &out).await?;
    tx.commit().await.map_err(internal)?;
    Ok(Json(out))
}
async fn chunk(
    State(s): State<AppState>,
    headers: HeaderMap,
    (RoutePath(id), Query(q)): (RoutePath<Uuid>, Query<ChunkQuery>),
    bytes: Bytes,
) -> Result<Json<ArtifactTransferStatus>> {
    let n = authenticate(&s, &headers, RunnerOperation::UploadArtifact).await?;
    let rt = runtime(&s)?;
    let _bulk = admit_bulk(&rt)?;
    q.validate(&bytes)?;
    let mut tx = s.store.pool.begin().await.map_err(internal)?;
    let attempt = owner_attempt(&mut tx, n.id(), id).await?;
    let _a = view(&s, &mut tx, &n, attempt).await?;
    let mut t = transfer(&mut tx, n.id(), id).await?;
    let operation = Operation::new(
        n.id(),
        q.operation_id,
        sha(format!("{id}/data/{}/{}/{}", q.offset, bytes.len(), q.digest).as_bytes()),
    );
    if let Some(old) = operation.replay(&mut tx).await? {
        return Ok(Json(old));
    }
    t.accept_chunk(q.offset, bytes.len() as u64)
        .map_err(conflict)?;
    aq_delivery::artifact_files::append(&part_path(&rt, id), t.offset, &bytes)
        .await
        .map_err(internal)?;
    t.offset += bytes.len() as u64;
    sqlx::query(
        "UPDATE retained_artifacts SET upload_offset=$2,updated_at=now() WHERE transfer_id=$1",
    )
    .bind(id)
    .bind(t.offset as i64)
    .execute(&mut *tx)
    .await
    .map_err(internal)?;
    let out = t.status(id);
    operation.record(&mut tx, &out).await?;
    tx.commit().await.map_err(internal)?;
    Ok(Json(out))
}
async fn commit(
    State(s): State<AppState>,
    headers: HeaderMap,
    RoutePath(id): RoutePath<Uuid>,
    ApiJson(r): ApiJson<CommitArtifact>,
) -> Result<Json<ArtifactTransferStatus>> {
    let n = authenticate(&s, &headers, RunnerOperation::UploadArtifact).await?;
    let rt = runtime(&s)?;
    let _bulk = admit_bulk(&rt)?;
    let mut tx = s.store.pool.begin().await.map_err(internal)?;
    let attempt = owner_attempt(&mut tx, n.id(), id).await?;
    let a = view(&s, &mut tx, &n, attempt).await?;
    let mut t = transfer(&mut tx, n.id(), id).await?;
    let operation = Operation::new(
        n.id(),
        r.operation_id,
        sha(format!("{id}/commit").as_bytes()),
    );
    if let Some(old) = operation.replay(&mut tx).await? {
        return Ok(Json(old));
    }
    if t.state != "uploading" {
        return Err(conflict("transfer is not uploading"));
    }
    if t.offset != t.bytes {
        return Err(conflict("artifact upload incomplete"));
    }
    // The binding is frozen at begin; the committing caller must still be it.
    if binding(&a)? != t.binding {
        return Err(AppError::Forbidden("artifact binding mismatch".into()));
    }
    let part = part_path(&rt, id);
    let (actual, len) = aq_delivery::artifact_files::digest(&part)
        .await
        .map_err(internal)?;
    if actual != t.artifact.digest.as_str() || len != t.bytes {
        remove_quietly(&part).await?;
        inventory::reject(&mut tx, id).await.map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        return Err(conflict("artifact digest mismatch"));
    }
    let now = crate::execution::api::now(&s);
    let until = now.saturating_add(rt.config.max_age.saturating_mul(1000));
    let receipt = t.retention_receipt(now, until).map_err(internal)?;
    tokio::fs::rename(&part, retained_path(&rt, t.receipt_id))
        .await
        .map_err(internal)?;
    sqlx::query("UPDATE retained_artifacts SET state='retained',receipt=$2::jsonb,retained_at_ms=$3,retained_until_ms=$4,updated_at=now() WHERE transfer_id=$1")
        .bind(id)
        .bind(serde_json::to_string(&receipt).map_err(internal)?)
        .bind(now as i64)
        .bind(until as i64)
        .execute(&mut *tx).await.map_err(internal)?;
    t.state = "retained".into();
    t.receipt = Some(receipt);
    let out = t.status(id);
    operation.record(&mut tx, &out).await?;
    tx.commit().await.map_err(internal)?;
    Ok(Json(out))
}
async fn status(
    State(s): State<AppState>,
    headers: HeaderMap,
    RoutePath(id): RoutePath<Uuid>,
) -> Result<Json<ArtifactTransferStatus>> {
    let n = authenticate(&s, &headers, RunnerOperation::Observe).await?;
    let mut tx = s.store.pool.begin().await.map_err(internal)?;
    let attempt = owner_attempt(&mut tx, n.id(), id).await?;
    let _a = view(&s, &mut tx, &n, attempt).await?;
    let t = transfer(&mut tx, n.id(), id).await?;
    tx.commit().await.map_err(internal)?;
    Ok(Json(t.status(id)))
}
/// Fetch a range of a retained artifact. The caller proves an attempt on the
/// same (project, task, lineage) as the retaining attempt: the predecessor
/// itself or any successor in its lineage. No other node or lineage may read.
async fn fetch(
    State(s): State<AppState>,
    headers: HeaderMap,
    RoutePath(receipt): RoutePath<Uuid>,
    Query(q): Query<FetchQuery>,
) -> Result<Response> {
    let n = authenticate(&s, &headers, RunnerOperation::FetchExecutionMaterial).await?;
    let rt = runtime(&s)?;
    let _bulk = admit_bulk(&rt)?;
    let limit = q.limit.clamp(1, chunk_size());
    let mut tx = s.store.pool.begin().await.map_err(internal)?;
    let a = view(&s, &mut tx, &n, q.attempt_id).await?;
    let b = binding(&a)?;
    let not_found = || AppError::NotFound("retained artifact not found".into());
    let row = inventory::retained(&mut tx, receipt)
        .await
        .map_err(internal)?
        .ok_or_else(not_found)?;
    if !row.active_at(crate::execution::api::now(&s)) {
        return Err(not_found());
    }
    if !row.matches_lineage(b.project_id, b.task_id, b.lineage_id) {
        return Err(AppError::Forbidden("artifact lineage mismatch".into()));
    }
    let total = row.bytes;
    tx.commit().await.map_err(internal)?;
    if q.offset > total {
        return Err(bad("fetch offset beyond artifact"));
    }
    let want = (total - q.offset).min(limit as u64) as usize;
    let buf = aq_delivery::artifact_files::read(&retained_path(&rt, receipt), q.offset, want)
        .await
        .map_err(internal)?;
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CACHE_CONTROL, "no-store")
        .header("x-aq-artifact-bytes", HeaderValue::from(total))
        .header("x-aq-artifact-offset", HeaderValue::from(q.offset))
        .body(Body::from(buf))
        .map_err(internal)
}
/// Release expired retention and reject stale uploads. Runs under the advisory
/// lock on every begin, so no background job is needed for correctness.
async fn cleanup(rt: &Runtime, tx: &mut Transaction<'_, Postgres>, now: u64) -> Result<()> {
    for (transfer, receipt) in inventory::expired(tx, now).await.map_err(internal)? {
        remove_quietly(&retained_path(rt, receipt)).await?;
        inventory::release(tx, transfer).await.map_err(internal)?;
    }
    for transfer in inventory::stale(tx, rt.config.incomplete_ttl_secs)
        .await
        .map_err(internal)?
    {
        remove_quietly(&part_path(rt, transfer)).await?;
        inventory::reject(tx, transfer).await.map_err(internal)?;
    }
    Ok(())
}
