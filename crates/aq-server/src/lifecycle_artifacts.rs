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
use aq_lifecycle_api::worker_lifecycle as lc;
use aq_session_protocol::{ArtifactRef, Text};
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Path as RoutePath, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::Response,
    routing::{get, post, put},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest as _, Sha256};
use sqlx::{Postgres, Row, Transaction};
use std::{path::PathBuf, sync::Arc};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
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
const ADVISORY_LOCK: i64 = 719247032;
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

/// Begin one artifact upload. `bytes` is the exact artifact length and
/// `artifact.digest` the sha256 the committed bytes must hash to.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BeginArtifact {
    pub operation_id: Uuid,
    pub attempt_id: Uuid,
    pub artifact: ArtifactRef,
    pub role: lc::ArtifactRole,
    pub bytes: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactTransferStatus {
    pub transfer_id: Uuid,
    pub offset: u64,
    pub bytes: u64,
    pub state: String,
    pub receipt: Option<lc::RetentionReceipt>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitArtifact {
    pub operation_id: Uuid,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChunkQuery {
    operation_id: Uuid,
    offset: u64,
    digest: String,
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
pub(crate) async fn replay<T: DeserializeOwned>(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    op: Uuid,
    payload: &str,
) -> Result<Option<T>> {
    if op.is_nil() {
        return Err(bad("operation id required"));
    }
    let row = sqlx::query(
        "SELECT payload_digest,response::text AS response FROM execution_operations WHERE node_id=$1 AND operation_id=$2",
    )
    .bind(node)
    .bind(op)
    .fetch_optional(&mut **tx)
    .await
    .map_err(internal)?;
    if let Some(r) = row {
        if r.get::<&str, _>("payload_digest") != payload {
            return Err(conflict("operation id payload conflict"));
        }
        return Ok(Some(
            serde_json::from_str(r.get("response")).map_err(internal)?,
        ));
    }
    Ok(None)
}
pub(crate) async fn record<T: Serialize>(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    op: Uuid,
    payload: &str,
    r: &T,
) -> Result<()> {
    sqlx::query("INSERT INTO execution_operations(node_id,operation_id,payload_digest,response) VALUES($1,$2,$3,$4::jsonb)").bind(node).bind(op).bind(payload).bind(serde_json::to_string(r).map_err(internal)?).execute(&mut **tx).await.map_err(internal)?;
    Ok(())
}

struct Transfer {
    receipt_id: Uuid,
    artifact: ArtifactRef,
    role: lc::ArtifactRole,
    binding: lc::Binding,
    bytes: u64,
    offset: u64,
    state: String,
    receipt: Option<lc::RetentionReceipt>,
}
async fn transfer(tx: &mut Transaction<'_, Postgres>, node: Uuid, id: Uuid) -> Result<Transfer> {
    let r=sqlx::query("SELECT receipt_id,artifact::text AS artifact,role,binding::text AS binding,declared_bytes,upload_offset,state,receipt::text AS receipt FROM retained_artifacts WHERE transfer_id=$1 AND node_id=$2 FOR UPDATE").bind(id).bind(node).fetch_optional(&mut **tx).await.map_err(internal)?.ok_or_else(||AppError::NotFound("artifact transfer not found".into()))?;
    Ok(Transfer {
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
fn response(id: Uuid, t: &Transfer) -> ArtifactTransferStatus {
    ArtifactTransferStatus {
        transfer_id: id,
        offset: t.offset,
        bytes: t.bytes,
        state: t.state.clone(),
        receipt: t.receipt.clone(),
    }
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
    let _bulk = rt
        .bulk()
        .try_acquire()
        .map_err(|_| AppError::Maintenance("artifact bulk capacity exhausted".into()))?;
    let mut tx = s.store.pool.begin().await.map_err(internal)?;
    let a = view(&s, &mut tx, &n, r.attempt_id).await?;
    let b = binding(&a)?;
    let payload = sha(&serde_json::to_vec(&r).map_err(internal)?);
    if let Some(old) = replay(&mut tx, n.id(), r.operation_id, &payload).await? {
        return Ok(Json(old));
    }
    if a.retention_released {
        return Err(conflict("attempt retention has already been released"));
    }
    if r.bytes > rt.config.max_bundle_bytes {
        return Err(AppError::PayloadTooLarge("artifact limit exceeded".into()));
    }
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(ADVISORY_LOCK)
        .execute(&mut *tx)
        .await
        .map_err(internal)?;
    let now = crate::execution::api::now(&s);
    cleanup(&rt, &mut tx, now).await?;
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM retained_artifacts WHERE NOT released")
            .fetch_one(&mut *tx)
            .await
            .map_err(internal)?;
    if count >= 1024 {
        return Err(AppError::PayloadTooLarge(
            "artifact transfer count limit exceeded".into(),
        ));
    }
    let used: i64 = sqlx::query_scalar(
        "SELECT COALESCE(sum(declared_bytes),0)::bigint FROM retained_artifacts WHERE NOT released",
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(internal)?;
    if (used.max(0) as u64).saturating_add(r.bytes) > rt.config.max_staging_bytes {
        return Err(AppError::PayloadTooLarge(
            "artifact retention quota exhausted".into(),
        ));
    }
    let transfer_id = Uuid::new_v4();
    let receipt_id = Uuid::new_v4();
    let binding_digest = lc::canonical_digest(&b).map_err(internal)?;
    sqlx::query("INSERT INTO retained_artifacts(transfer_id,receipt_id,node_id,attempt_id,project_id,task_id,lineage_id,binding,binding_digest,role,artifact,declared_bytes) VALUES($1,$2,$3,$4,$5,$6,$7,$8::jsonb,$9,$10,$11::jsonb,$12)")
        .bind(transfer_id).bind(receipt_id).bind(n.id()).bind(r.attempt_id)
        .bind(b.project_id).bind(b.task_id).bind(b.lineage_id)
        .bind(serde_json::to_string(&b).map_err(internal)?).bind(binding_digest.as_str())
        .bind(role_name(&r.role)?).bind(serde_json::to_string(&r.artifact).map_err(internal)?)
        .bind(r.bytes as i64)
        .execute(&mut *tx).await.map_err(internal)?;
    let out = ArtifactTransferStatus {
        transfer_id,
        offset: 0,
        bytes: r.bytes,
        state: "uploading".into(),
        receipt: None,
    };
    record(&mut tx, n.id(), r.operation_id, &payload, &out).await?;
    tx.commit().await.map_err(internal)?;
    Ok(Json(out))
}
async fn chunk(
    State(s): State<AppState>,
    headers: HeaderMap,
    RoutePath(id): RoutePath<Uuid>,
    Query(q): Query<ChunkQuery>,
    bytes: Bytes,
) -> Result<Json<ArtifactTransferStatus>> {
    let n = authenticate(&s, &headers, RunnerOperation::UploadArtifact).await?;
    let rt = runtime(&s)?;
    let _bulk = rt
        .bulk()
        .try_acquire()
        .map_err(|_| AppError::Maintenance("artifact bulk capacity exhausted".into()))?;
    if bytes.is_empty() || bytes.len() > chunk_size() || q.digest != sha(&bytes) {
        return Err(bad("invalid chunk size or digest"));
    }
    let mut tx = s.store.pool.begin().await.map_err(internal)?;
    let attempt = owner_attempt(&mut tx, n.id(), id).await?;
    let _a = view(&s, &mut tx, &n, attempt).await?;
    let mut t = transfer(&mut tx, n.id(), id).await?;
    let payload = sha(format!("{id}/data/{}/{}/{}", q.offset, bytes.len(), q.digest).as_bytes());
    if let Some(old) = replay(&mut tx, n.id(), q.operation_id, &payload).await? {
        return Ok(Json(old));
    }
    if t.state != "uploading" {
        return Err(conflict("transfer no longer accepts chunks"));
    }
    if q.offset != t.offset || t.offset.saturating_add(bytes.len() as u64) > t.bytes {
        return Err(conflict("chunk offset or declared length mismatch"));
    }
    tokio::fs::create_dir_all(artifacts_dir(&rt))
        .await
        .map_err(internal)?;
    let mut f = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(part_path(&rt, id))
        .await
        .map_err(internal)?;
    // Discard any unacknowledged bytes from a crash before the SQL commit.
    f.set_len(t.offset).await.map_err(internal)?;
    f.seek(std::io::SeekFrom::Start(t.offset))
        .await
        .map_err(internal)?;
    f.write_all(&bytes).await.map_err(internal)?;
    f.sync_all().await.map_err(internal)?;
    t.offset += bytes.len() as u64;
    sqlx::query(
        "UPDATE retained_artifacts SET upload_offset=$2,updated_at=now() WHERE transfer_id=$1",
    )
    .bind(id)
    .bind(t.offset as i64)
    .execute(&mut *tx)
    .await
    .map_err(internal)?;
    let out = response(id, &t);
    record(&mut tx, n.id(), q.operation_id, &payload, &out).await?;
    tx.commit().await.map_err(internal)?;
    Ok(Json(out))
}
async fn file_digest(path: &std::path::Path) -> Result<(String, u64)> {
    let mut f = tokio::fs::File::open(path).await.map_err(internal)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = f.read(&mut buf).await.map_err(internal)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        total += n as u64;
    }
    Ok((hex::encode(h.finalize()), total))
}
async fn commit(
    State(s): State<AppState>,
    headers: HeaderMap,
    RoutePath(id): RoutePath<Uuid>,
    ApiJson(r): ApiJson<CommitArtifact>,
) -> Result<Json<ArtifactTransferStatus>> {
    let n = authenticate(&s, &headers, RunnerOperation::UploadArtifact).await?;
    let rt = runtime(&s)?;
    let _bulk = rt
        .bulk()
        .try_acquire()
        .map_err(|_| AppError::Maintenance("artifact bulk capacity exhausted".into()))?;
    let mut tx = s.store.pool.begin().await.map_err(internal)?;
    let attempt = owner_attempt(&mut tx, n.id(), id).await?;
    let a = view(&s, &mut tx, &n, attempt).await?;
    let mut t = transfer(&mut tx, n.id(), id).await?;
    let payload = sha(format!("{id}/commit").as_bytes());
    if let Some(old) = replay(&mut tx, n.id(), r.operation_id, &payload).await? {
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
    let (actual, len) = file_digest(&part).await?;
    if actual != t.artifact.digest.as_str() || len != t.bytes {
        remove_quietly(&part).await?;
        sqlx::query("UPDATE retained_artifacts SET state='rejected',released=TRUE,updated_at=now() WHERE transfer_id=$1").bind(id).execute(&mut *tx).await.map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        return Err(conflict("artifact digest mismatch"));
    }
    let now = crate::execution::api::now(&s);
    let until = now.saturating_add(rt.config.max_age.saturating_mul(1000));
    let receipt = lc::RetentionReceipt {
        receipt_id: t.receipt_id,
        owner: Text::new(format!("aq-server:{}", t.binding.project_id)).map_err(internal)?,
        binding_digest: lc::canonical_digest(&t.binding).map_err(internal)?,
        artifact: t.artifact.clone(),
        role: t.role.clone(),
        bytes: t.bytes,
        retained_at_ms: now,
        retained_until_ms: until,
    };
    lc::validate_retention(std::slice::from_ref(&receipt), &t.binding).map_err(internal)?;
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
    let out = response(id, &t);
    record(&mut tx, n.id(), r.operation_id, &payload, &out).await?;
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
    Ok(Json(response(id, &t)))
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
    let _bulk = rt
        .bulk()
        .try_acquire()
        .map_err(|_| AppError::Maintenance("artifact bulk capacity exhausted".into()))?;
    let limit = q.limit.clamp(1, chunk_size());
    let mut tx = s.store.pool.begin().await.map_err(internal)?;
    let a = view(&s, &mut tx, &n, q.attempt_id).await?;
    let b = binding(&a)?;
    let row = sqlx::query("SELECT project_id,task_id,lineage_id,declared_bytes,state,released,retained_until_ms FROM retained_artifacts WHERE receipt_id=$1")
        .bind(receipt).fetch_optional(&mut *tx).await.map_err(internal)?;
    let not_found = || AppError::NotFound("retained artifact not found".into());
    let row = row.ok_or_else(not_found)?;
    let now = crate::execution::api::now(&s);
    let until = row.get::<Option<i64>, _>("retained_until_ms").unwrap_or(0);
    if row.get::<&str, _>("state") != "retained"
        || row.get::<bool, _>("released")
        || (until.max(0) as u64) <= now
    {
        return Err(not_found());
    }
    if row.get::<Uuid, _>("project_id") != b.project_id
        || row.get::<Uuid, _>("task_id") != b.task_id
        || row.get::<Uuid, _>("lineage_id") != b.lineage_id
    {
        return Err(AppError::Forbidden("artifact lineage mismatch".into()));
    }
    let total = row.get::<i64, _>("declared_bytes").max(0) as u64;
    tx.commit().await.map_err(internal)?;
    if q.offset > total {
        return Err(bad("fetch offset beyond artifact"));
    }
    let mut f = tokio::fs::File::open(retained_path(&rt, receipt))
        .await
        .map_err(internal)?;
    f.seek(std::io::SeekFrom::Start(q.offset))
        .await
        .map_err(internal)?;
    let want = (total - q.offset).min(limit as u64) as usize;
    let mut buf = vec![0u8; want];
    let mut filled = 0;
    while filled < want {
        let n = f.read(&mut buf[filled..]).await.map_err(internal)?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    buf.truncate(filled);
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
    let expired=sqlx::query("SELECT transfer_id,receipt_id FROM retained_artifacts WHERE NOT released AND state='retained' AND retained_until_ms IS NOT NULL AND retained_until_ms <= $1 FOR UPDATE SKIP LOCKED").bind(now.min(i64::MAX as u64) as i64).fetch_all(&mut **tx).await.map_err(internal)?;
    for r in expired {
        let id: Uuid = r.get("transfer_id");
        remove_quietly(&retained_path(rt, r.get("receipt_id"))).await?;
        sqlx::query(
            "UPDATE retained_artifacts SET released=TRUE,updated_at=now() WHERE transfer_id=$1",
        )
        .bind(id)
        .execute(&mut **tx)
        .await
        .map_err(internal)?;
    }
    let stale:Vec<Uuid>=sqlx::query_scalar("SELECT transfer_id FROM retained_artifacts WHERE state='uploading' AND updated_at < now()-($1::bigint * interval '1 second') FOR UPDATE SKIP LOCKED").bind(rt.config.incomplete_ttl_secs.min(i64::MAX as u64) as i64).fetch_all(&mut **tx).await.map_err(internal)?;
    for id in stale {
        remove_quietly(&part_path(rt, id)).await?;
        sqlx::query("UPDATE retained_artifacts SET state='rejected',released=TRUE,updated_at=now() WHERE transfer_id=$1")
            .bind(id)
            .execute(&mut **tx)
            .await
            .map_err(internal)?;
    }
    Ok(())
}
