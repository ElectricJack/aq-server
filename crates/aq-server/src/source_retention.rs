//! Authenticated bounded source transport. Upload completion is never retention.
//! Only trusted frozen assignment metadata can select a repository. Socket calls
//! are built here from a finite allowlist; caller JSON is never forwarded.
use crate::{
    AppState,
    admin_api::ApiJson,
    error::AppError,
    worker_api::{AuthenticatedNode, authenticate_context, lock_current_node},
};
use aq_worker_protocol::{
    CentralRetentionReceipt, ObjectId, SourceBinding, TransferManifest, TransferStatus,
    TransferStatusRequest, Validate,
};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path as RoutePath, Query, State},
    http::HeaderMap,
    routing::{get, post, put},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{Postgres, Row, Transaction};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufReader},
    sync::Semaphore,
};
use uuid::Uuid;
type Result<T> = std::result::Result<T, AppError>;
fn bad(s: &str) -> AppError {
    AppError::BadRequest(s.into())
}
fn conflict(s: &str) -> AppError {
    AppError::Conflict(s.into())
}
fn internal<E: std::fmt::Display>(e: E) -> AppError {
    tracing::error!(error=%e,"source retention failure");
    AppError::Internal
}
use aq_source_push::{GitManifest, PushError, Quarantine, digest, socket_call};
pub use aq_source_push::{SourceRetentionConfig, default_keep_per_task, default_max_age};
pub struct Runtime {
    pub config: SourceRetentionConfig,
    bulk: Semaphore,
    finalize_gate: tokio::sync::Mutex<()>,
}
impl Runtime {
    pub fn new(config: SourceRetentionConfig) -> anyhow::Result<Self> {
        config.validate()?;
        config.prepare_roots()?;
        Ok(Self {
            bulk: Semaphore::new(config.bulk_concurrency),
            finalize_gate: tokio::sync::Mutex::new(()),
            config,
        })
    }
    fn path(&self, id: Uuid) -> PathBuf {
        self.config.staging_root.join(id.to_string())
    }
    /// Bulk transport admission shared with lifecycle artifact retention.
    pub(crate) fn bulk(&self) -> &Semaphore {
        &self.bulk
    }
}
fn runtime(s: &AppState) -> Result<Arc<Runtime>> {
    s.source_retention
        .clone()
        .ok_or_else(|| AppError::UpdatesUnavailable("source retention is not configured".into()))
}
pub fn machine_routes(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/v1/execution/sources/begin", post(begin))
        .route("/v1/execution/sources/base/{attempt}", get(base_status))
        .route(
            "/v1/execution/sources/base/{attempt}/{file}",
            get(base_download),
        )
        .route("/v1/execution/sources/finalize", post(finalize))
        .route("/v1/execution/sources/pushed", post(pushed))
        .route("/v1/execution/git/{attempt}/info/refs", get(push_refs))
        .route(
            "/v1/execution/git/{attempt}/git-receive-pack",
            post(push_pack),
        )
        .route("/v1/execution/sources/{transfer}/{file}", put(chunk))
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .with_state(state.clone())
        .merge(
            Router::new()
                .route("/v1/execution/sources/status", post(status))
                .layer(DefaultBodyLimit::max(16384))
                .with_state(state.control_view()),
        )
}
pub fn admin_routes() -> Router<AppState> {
    Router::new()
        .route("/api/source-bases/provision", post(provision_base))
        .layer(DefaultBodyLimit::max(16384))
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BeginRequest {
    pub identity: aq_worker_protocol::NodeIdentity,
    pub operation_id: Uuid,
    pub manifest: TransferManifest,
}
/// Admission of the commit an attempt pushed to its quarantine repository.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PushedRequest {
    pub identity: aq_worker_protocol::NodeIdentity,
    pub operation_id: Uuid,
    pub transfer_id: Uuid,
    pub attempt: aq_worker_protocol::AttemptRef,
    pub resolved_spec_digest: String,
    pub source: SourceBinding,
    pub checkpoint: ObjectId,
}
#[derive(Deserialize)]
struct ServiceQuery {
    service: String,
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
struct DownloadQuery {
    offset: u64,
    #[serde(default = "chunk_size")]
    limit: usize,
}
fn chunk_size() -> usize {
    256 * 1024
}
// Caller holds Node -> Attempt locks, including late (expired/cancelled) attempts.
// These paths never renew a grant or change attempt disposition.
async fn authority(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    attempt: Uuid,
) -> Result<crate::execution::authority::AuthorityView> {
    use crate::execution::authority::{AuthorityError, load_source_locked};
    load_source_locked(tx, node, attempt)
        .await
        .map_err(|e| match e {
            AuthorityError::NotFound | AuthorityError::Identity => {
                AppError::Forbidden("attempt is not owned by this node".into())
            }
            other => other.into(),
        })
}
// HTTP-only credential fence. Internal confirmed-owner/admin reads keep their
// actual UUID authority seam; no request credential becomes persisted authority.
async fn machine_authority(
    tx: &mut Transaction<'_, Postgres>,
    authenticated: &AuthenticatedNode,
    attempt: Uuid,
) -> Result<crate::execution::authority::AuthorityView> {
    if authenticated
        .delegated()
        .is_some_and(|d| d.current.record.binding.attempt_id != attempt)
    {
        return Err(AppError::Forbidden(
            "source transfer attempt mismatch".into(),
        ));
    }
    lock_current_node(tx, authenticated).await?;
    authority(tx, authenticated.id(), attempt).await
}
async fn runner(s: &AppState, headers: &HeaderMap) -> Result<AuthenticatedNode> {
    use aq_admission_sandbox::sandbox::RunnerOperation::ReportResult;
    crate::delegated_permits::runtime::authenticate_runner(s, headers, ReportResult, None).await
}
fn binding(a: &crate::execution::authority::AuthorityView) -> Result<&SourceBinding> {
    a.source
        .as_ref()
        .ok_or_else(|| AppError::Forbidden("attempt has no source binding".into()))
}
fn check_manifest(
    a: &crate::execution::authority::AuthorityView,
    m: &TransferManifest,
) -> Result<()> {
    m.validate().map_err(|_| bad("invalid transfer manifest"))?;
    if m.role != aq_worker_protocol::SourceRole::Checkpoint {
        return Err(AppError::Forbidden(
            "frozen source assignment mismatch".into(),
        ));
    }
    frozen(a, &m.source, &m.resolved_spec_digest, &m.attempt)
}
/// A checkpoint of this attempt: an issued grant where the attempt is profiled,
/// and exactly the frozen source binding, spec and attempt identity.
fn frozen(
    a: &crate::execution::authority::AuthorityView,
    source: &SourceBinding,
    spec: &str,
    attempt: &aq_worker_protocol::AttemptRef,
) -> Result<()> {
    if a.kind == crate::execution::authority::Kind::ProfiledV1 && a.grant_id.is_none() {
        return Err(AppError::Forbidden(
            "checkpoint transfer requires an issued execution grant".into(),
        ));
    }
    if source != binding(a)? || spec != a.execution_spec_digest || attempt != &a.attempt {
        return Err(AppError::Forbidden(
            "frozen source assignment mismatch".into(),
        ));
    }
    Ok(())
}

struct Transfer {
    manifest: TransferManifest,
    op: Uuid,
    manifest_offset: u64,
    bundle_offset: u64,
    state: String,
    receipt: Option<CentralRetentionReceipt>,
}
async fn transfer(tx: &mut Transaction<'_, Postgres>, node: Uuid, id: Uuid) -> Result<Transfer> {
    let r=sqlx::query("SELECT manifest::text AS manifest,import_operation_id,manifest_offset,bundle_offset,state,receipt::text AS receipt FROM source_transfers WHERE transfer_id=$1 AND node_id=$2 FOR UPDATE").bind(id).bind(node).fetch_optional(&mut **tx).await.map_err(internal)?.ok_or_else(||AppError::NotFound("source transfer not found".into()))?;
    Ok(Transfer {
        manifest: serde_json::from_str(r.get("manifest")).map_err(internal)?,
        op: r.get("import_operation_id"),
        manifest_offset: r.get::<i64, _>("manifest_offset") as u64,
        bundle_offset: r.get::<i64, _>("bundle_offset") as u64,
        state: r.get("state"),
        receipt: r
            .get::<Option<&str>, _>("receipt")
            .map(serde_json::from_str)
            .transpose()
            .map_err(internal)?,
    })
}
fn response(id: Uuid, t: &Transfer) -> TransferStatus {
    TransferStatus {
        transfer_id: id,
        manifest_offset: t.manifest_offset,
        bundle_offset: t.bundle_offset,
        receipt: t.receipt.clone(),
    }
}
async fn replay(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    op: Uuid,
    payload: &str,
) -> Result<Option<TransferStatus>> {
    if op.is_nil() {
        return Err(bad("operation id required"));
    }
    let row=sqlx::query("SELECT payload_digest,response::text AS response FROM source_transfer_operations WHERE node_id=$1 AND operation_id=$2").bind(node).bind(op).fetch_optional(&mut **tx).await.map_err(internal)?;
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
async fn record(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    op: Uuid,
    payload: &str,
    r: &TransferStatus,
) -> Result<()> {
    sqlx::query("INSERT INTO source_transfer_operations(node_id,operation_id,payload_digest,response) VALUES($1,$2,$3,$4::jsonb)").bind(node).bind(op).bind(payload).bind(serde_json::to_string(r).map_err(internal)?).execute(&mut **tx).await.map_err(internal)?;
    Ok(())
}
/// Reserves durable ownership of a transfer under the shared staging limits.
/// `complete` is a transfer whose bytes the server staged itself from a push,
/// so its offsets start at the end and no chunk is expected.
async fn reserve(
    s: &AppState,
    rt: &Runtime,
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    m: &TransferManifest,
    complete: bool,
) -> Result<()> {
    if m.bundle_bytes > rt.config.max_bundle_bytes {
        return Err(AppError::PayloadTooLarge("bundle limit exceeded".into()));
    }
    // Serialize reservations across nodes; accounting includes uncertain imports.
    sqlx::query("SELECT pg_advisory_xact_lock(719247031)")
        .execute(&mut **tx)
        .await
        .map_err(internal)?;
    cleanup(rt, tx).await?;
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM source_transfers WHERE NOT staging_released")
            .fetch_one(&mut **tx)
            .await
            .map_err(internal)?;
    if count >= 1024 {
        return Err(AppError::PayloadTooLarge(
            "source transfer count limit exceeded".into(),
        ));
    }
    let used = aq_history_caps::staging_used(tx, (s.clock)().timestamp_millis())
        .await
        .map_err(internal)?;
    if (used as u64)
        .saturating_add(m.manifest_bytes)
        .saturating_add(m.bundle_bytes)
        > rt.config.max_staging_bytes
    {
        return Err(AppError::PayloadTooLarge(
            "source staging quota exhausted".into(),
        ));
    }
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM source_transfers WHERE transfer_id=$1 OR artifact_id=$2)",
    )
    .bind(m.transfer_id)
    .bind(m.artifact_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(internal)?;
    if exists {
        return Err(conflict("transfer or artifact identity already exists"));
    }
    // Reserve durable ownership before creating files. Chunk writes lazily create
    // private members, so cancellation cannot leave an unaccounted orphan.

    let (manifest_offset, bundle_offset) = if complete {
        (m.manifest_bytes as i64, m.bundle_bytes as i64)
    } else {
        (0, 0)
    };
    sqlx::query("INSERT INTO source_transfers(transfer_id,attempt_id,node_id,repository_id,import_operation_id,manifest,artifact_id,manifest_offset,bundle_offset) VALUES($1,$2,$3,$4,$5,$6::jsonb,$7,$8,$9)").bind(m.transfer_id).bind(m.attempt.attempt_id).bind(node).bind(m.source.repository_id).bind(Uuid::new_v4()).bind(serde_json::to_string(&m).map_err(internal)?).bind(m.artifact_id).bind(manifest_offset).bind(bundle_offset).execute(&mut **tx).await.map_err(internal)?;
    Ok(())
}
async fn begin(
    State(s): State<AppState>,
    headers: HeaderMap,
    ApiJson(r): ApiJson<BeginRequest>,
) -> Result<Json<TransferStatus>> {
    r.identity
        .validate()
        .map_err(|_| bad("invalid node identity"))?;
    let n = runner(&s, &headers).await?;
    if r.identity.node_id != n.id() {
        return Err(AppError::Forbidden("node identity mismatch".into()));
    }
    let rt = runtime(&s)?;
    let _bulk = rt
        .bulk
        .try_acquire()
        .map_err(|_| AppError::Maintenance("source bulk capacity exhausted".into()))?;
    let mut tx = s.store.pool.begin().await.map_err(internal)?;
    let a = machine_authority(&mut tx, &n, r.manifest.attempt.attempt_id).await?;
    check_manifest(&a, &r.manifest)?;
    let payload = digest(&serde_json::to_vec(&r).map_err(internal)?);
    if let Some(old) = replay(&mut tx, n.id(), r.operation_id, &payload).await? {
        return Ok(Json(old));
    }
    // Historical retries retain their response, but a released attempt cannot
    // acquire a new source-transfer intent or task claim.
    if a.retention_released {
        return Err(conflict("attempt retention has already been released"));
    }
    reserve(&s, &rt, &mut tx, n.id(), &r.manifest, false).await?;
    let out = TransferStatus {
        transfer_id: r.manifest.transfer_id,
        manifest_offset: 0,
        bundle_offset: 0,
        receipt: None,
    };
    record(&mut tx, n.id(), r.operation_id, &payload, &out).await?;
    tx.commit().await.map_err(internal)?;
    Ok(Json(out))
}
async fn status(
    State(s): State<AppState>,
    headers: HeaderMap,
    ApiJson(r): ApiJson<TransferStatusRequest>,
) -> Result<axum::response::Response> {
    r.validate()
        .map_err(|_| bad("invalid source status request"))?;
    let n = runner(&s, &headers).await?;
    if n.id() != r.identity.node_id {
        return Err(AppError::Forbidden("node identity mismatch".into()));
    }
    let mut tx = s.store.pool.begin().await.map_err(internal)?;
    // Discover attempt before taking row lock so lock ordering stays Node->Attempt->Transfer.
    let attempt: Uuid = sqlx::query_scalar(
        "SELECT attempt_id FROM source_transfers WHERE transfer_id=$1 AND node_id=$2",
    )
    .bind(r.transfer_id)
    .bind(n.id())
    .fetch_optional(&mut *tx)
    .await
    .map_err(internal)?
    .ok_or_else(|| AppError::NotFound("source transfer not found".into()))?;
    let a = machine_authority(&mut tx, &n, attempt).await?;
    let t = transfer(&mut tx, n.id(), r.transfer_id).await?;
    check_manifest(&a, &t.manifest)?;
    tx.commit().await.map_err(internal)?;
    crate::delegated_permits::receipts::source(&s, &n, response(r.transfer_id, &t)).await
}
fn filename(name: &str) -> Result<&'static str> {
    match name {
        "manifest" => Ok("manifest.json"),
        "bundle" => Ok("objects.bundle"),
        _ => Err(bad("invalid artifact file")),
    }
}
async fn chunk(
    State(s): State<AppState>,
    headers: HeaderMap,
    RoutePath((id, file)): RoutePath<(Uuid, String)>,
    Query(q): Query<ChunkQuery>,
    bytes: Bytes,
) -> Result<Json<TransferStatus>> {
    let n = runner(&s, &headers).await?;
    let rt = runtime(&s)?;
    let _bulk = rt
        .bulk
        .try_acquire()
        .map_err(|_| AppError::Maintenance("source bulk capacity exhausted".into()))?;
    let name = filename(&file)?;
    if bytes.is_empty() || bytes.len() > chunk_size() || q.digest != digest(&bytes) {
        return Err(bad("invalid chunk size or digest"));
    }
    let mut tx = s.store.pool.begin().await.map_err(internal)?;
    let attempt: Uuid = sqlx::query_scalar(
        "SELECT attempt_id FROM source_transfers WHERE transfer_id=$1 AND node_id=$2",
    )
    .bind(id)
    .bind(n.id())
    .fetch_optional(&mut *tx)
    .await
    .map_err(internal)?
    .ok_or_else(|| AppError::NotFound("source transfer not found".into()))?;
    let a = machine_authority(&mut tx, &n, attempt).await?;
    let mut t = transfer(&mut tx, n.id(), id).await?;
    check_manifest(&a, &t.manifest)?;
    let payload =
        digest(format!("{id}/{file}/{}/{}/{}", q.offset, bytes.len(), q.digest).as_bytes());
    if let Some(old) = replay(&mut tx, n.id(), q.operation_id, &payload).await? {
        return Ok(Json(old));
    }
    if t.state != "uploading" {
        return Err(conflict("transfer no longer accepts chunks"));
    }
    let (offset, limit) = if name == "manifest.json" {
        (t.manifest_offset, t.manifest.manifest_bytes)
    } else {
        (t.bundle_offset, t.manifest.bundle_bytes)
    };
    if q.offset != offset || offset.saturating_add(bytes.len() as u64) > limit {
        return Err(conflict("chunk offset or declared length mismatch"));
    }
    tokio::fs::create_dir_all(rt.path(id))
        .await
        .map_err(internal)?;
    let mut f = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(rt.path(id).join(name))
        .await
        .map_err(internal)?;
    // Discard any unacknowledged bytes from a crash before the SQL commit.
    f.set_len(offset).await.map_err(internal)?;
    f.seek(std::io::SeekFrom::Start(offset))
        .await
        .map_err(internal)?;
    f.write_all(&bytes).await.map_err(internal)?;
    f.sync_all().await.map_err(internal)?;
    let next = offset + bytes.len() as u64;
    if name == "manifest.json" {
        t.manifest_offset = next;
    } else {
        t.bundle_offset = next;
    }
    sqlx::query("UPDATE source_transfers SET manifest_offset=$2,bundle_offset=$3,updated_at=now() WHERE transfer_id=$1").bind(id).bind(t.manifest_offset as i64).bind(t.bundle_offset as i64).execute(&mut *tx).await.map_err(internal)?;
    let out = response(id, &t);
    record(&mut tx, n.id(), q.operation_id, &payload, &out).await?;
    tx.commit().await.map_err(internal)?;
    Ok(Json(out))
}
async fn hash_file(path: &Path) -> Result<String> {
    aq_source_push::hash_file(path).await.map_err(internal)
}
async fn socket_import(rt: &Runtime, t: &Transfer) -> Result<Value> {
    let request_id = format!("source-import-{}", t.op);
    let request = json!({"protocol_version":1,"request_id":request_id,"operation":"artifact.import","repository_id":t.manifest.source.repository_id,"operation_id":t.op,"expected_binding_generation":t.manifest.source.binding_generation,"params":{"artifact_handle":t.manifest.artifact_id},"deadline_ms":30000});
    let exchange = socket_call(&rt.config.git_socket, &request);
    tokio::time::timeout(Duration::from_secs(35), exchange)
        .await
        .map_err(|_| AppError::UpdatesUnavailable("import pending; retry same transfer".into()))?
        .map_err(|e| {
            tracing::warn!(error=%e,operation_id=%t.op,"import outcome remains pending");
            AppError::UpdatesUnavailable("import pending; retry same transfer".into())
        })
}
async fn finalize(
    State(s): State<AppState>,
    headers: HeaderMap,
    ApiJson(r): ApiJson<TransferStatusRequest>,
) -> Result<axum::response::Response> {
    r.validate()
        .map_err(|_| bad("invalid source status request"))?;
    let n = runner(&s, &headers).await?;
    if n.id() != r.identity.node_id {
        return Err(AppError::Forbidden("node identity mismatch".into()));
    }
    let rt = runtime(&s)?;
    let _bulk = rt
        .bulk
        .try_acquire()
        .map_err(|_| AppError::Maintenance("source bulk capacity exhausted".into()))?;
    let _gate = rt.finalize_gate.lock().await;
    admit(&s, &rt, &n, r.transfer_id).await
}
/// Admits one complete staged transfer into central retention and answers with
/// its signed status. Shared by uploaded and pushed transfers; the caller holds
/// the finalize gate.
async fn admit(
    s: &AppState,
    rt: &Runtime,
    n: &AuthenticatedNode,
    id: Uuid,
) -> Result<axum::response::Response> {
    let mut tx = s.store.pool.begin().await.map_err(internal)?;
    let attempt: Uuid = sqlx::query_scalar(
        "SELECT attempt_id FROM source_transfers WHERE transfer_id=$1 AND node_id=$2",
    )
    .bind(id)
    .bind(n.id())
    .fetch_optional(&mut *tx)
    .await
    .map_err(internal)?
    .ok_or_else(|| AppError::NotFound("source transfer not found".into()))?;
    let a = machine_authority(&mut tx, n, attempt).await?;
    let mut t = transfer(&mut tx, n.id(), id).await?;
    check_manifest(&a, &t.manifest)?;
    if t.state == "rejected" {
        return Err(conflict("source upload expired"));
    }
    if t.receipt.is_some() {
        tx.commit().await.map_err(internal)?;
        return crate::delegated_permits::receipts::source(s, n, response(id, &t)).await;
    }
    if t.manifest_offset != t.manifest.manifest_bytes || t.bundle_offset != t.manifest.bundle_bytes
    {
        return Err(conflict("source upload incomplete"));
    }
    // Protect complete bytes from expiry cleanup before releasing locks for
    // validation. The durable operation identity already exists; no Git call
    // occurs until all transport and frozen-manifest checks pass.
    if t.state == "uploading" {
        sqlx::query("UPDATE source_transfers SET state='import_pending',updated_at=now() WHERE transfer_id=$1 AND state='uploading'").bind(id).execute(&mut *tx).await.map_err(internal)?;
        t.state = "import_pending".into();
    }
    tx.commit().await.map_err(internal)?;
    let incoming = rt
        .config
        .git_root
        .join("artifacts/incoming")
        .join(t.manifest.artifact_id.to_string());
    let staging = rt.path(id);
    let validated = async {
        let published = tokio::fs::try_exists(&incoming).await.map_err(internal)?;
        let path = if published { &incoming } else { &staging };
        if hash_file(&path.join("manifest.json")).await? != t.manifest.manifest_digest {
            return Err(conflict("manifest transport digest mismatch"));
        }
        if let Some(expected) = &t.manifest.bundle_digest
            && hash_file(&path.join("objects.bundle")).await? != *expected
        {
            return Err(conflict("bundle transport digest mismatch"));
        }
        let bytes = tokio::fs::read(path.join("manifest.json"))
            .await
            .map_err(internal)?;
        let m: GitManifest =
            serde_json::from_slice(&bytes).map_err(|_| bad("invalid aq-git manifest"))?;
        if !m.binds(&t.manifest) {
            return Err(AppError::Forbidden(
                "artifact does not match frozen source binding".into(),
            ));
        }
        if !m.is_checkpoint_of(&t.manifest) {
            return Err(AppError::Forbidden(
                "artifact role or checkpoint mismatch".into(),
            ));
        }
        m.source.validate().map_err(|_| bad("invalid source oid"))?;
        let canonical_digest = digest(&serde_json::to_vec(&m).map_err(internal)?);
        Ok::<_, AppError>((m, canonical_digest))
    }
    .await;
    // Recheck the same HTTP principal after filesystem waits before either
    // rejecting transport or admitting its physical import. The UUID-only
    // trusted owner loader remains reusable by internal readers.
    let mut import_tx = s.store.pool.begin().await.map_err(internal)?;
    let a = machine_authority(&mut import_tx, n, attempt).await?;
    let latest = transfer(&mut import_tx, n.id(), id).await?;
    check_manifest(&a, &latest.manifest)?;
    if latest.receipt.is_some() {
        import_tx.commit().await.map_err(internal)?;
        return crate::delegated_permits::receipts::source(s, n, response(id, &latest)).await;
    }
    if latest.state == "rejected" {
        return Err(conflict("source transfer rejected"));
    }
    let (m, canonical_digest) = match validated {
        Ok(v) => v,
        Err(e) => {
            if matches!(
                e,
                AppError::BadRequest(_) | AppError::Forbidden(_) | AppError::Conflict(_)
            ) {
                // Never discard bytes for an uncertain already-dispatched import.
                let changed=sqlx::query("UPDATE source_transfers SET state='rejected' WHERE transfer_id=$1 AND state='import_pending' AND NOT dispatch_intent AND receipt IS NULL").bind(id).execute(&mut *import_tx).await.map_err(internal)?.rows_affected();
                import_tx.commit().await.map_err(internal)?;
                if changed == 1 {
                    discard_transport(rt, id, t.manifest.artifact_id).await?;
                    sqlx::query("UPDATE source_transfers SET staging_released=TRUE WHERE transfer_id=$1 AND state='rejected' AND NOT dispatch_intent").bind(id).execute(&s.store.pool).await.map_err(internal)?;
                }
            }
            return Err(e);
        }
    };
    let b = &t.manifest.source;
    sqlx::query("UPDATE source_transfers SET dispatch_intent=TRUE WHERE transfer_id=$1")
        .bind(id)
        .execute(&mut *import_tx)
        .await
        .map_err(internal)?;
    import_tx.commit().await.map_err(internal)?;
    if !tokio::fs::try_exists(&incoming).await.map_err(internal)? {
        // Atomic rename only: deployments must place private staging on git_root's filesystem.

        tokio::fs::create_dir_all(incoming.parent().unwrap())
            .await
            .map_err(internal)?;
        tokio::fs::rename(&staging, &incoming)
            .await
            .map_err(internal)?;
        let dir = tokio::fs::File::open(incoming.parent().unwrap())
            .await
            .map_err(internal)?;
        dir.sync_all().await.map_err(internal)?;
    }
    let imported = socket_import(rt, &t).await;
    // A failed/late physical reply cannot bypass the original principal fence.
    // Already admitted import intent/bytes stay protected for a fresh caller.
    let mut tx = s.store.pool.begin().await.map_err(internal)?;
    machine_authority(&mut tx, n, attempt).await?;
    transfer(&mut tx, n.id(), id).await?;
    let receipt = imported?;
    if receipt["repository_id"] != b.repository_id.to_string()
        || receipt["binding_generation"] != b.binding_generation
        || receipt["transfer_id"] != m.transfer_id
        || receipt["retention_handle"] != m.transfer_id
        || receipt["manifest_digest"] != canonical_digest
        || receipt["source"] != serde_json::to_value(&m.source).map_err(internal)?
        || receipt["retention_state"] != "active"
        || receipt["historical"] != false
        || receipt["receiver_installation_id"]
            .as_str()
            .and_then(|v| Uuid::parse_str(v).ok())
            .is_none()
    {
        return Err(AppError::UpdatesUnavailable(
            "unverified import receipt; transfer remains pending".into(),
        ));
    }
    t.receipt = Some(CentralRetentionReceipt {
        receipt_id: t.op,
        transfer_id: id,
        repository_id: b.repository_id,
        binding_generation: b.binding_generation,
        source: m.source,
        retention_handle: t.manifest.artifact_id,
        manifest_digest: canonical_digest,
    });
    sqlx::query("UPDATE source_transfers SET state='retained',receipt=$2::jsonb,updated_at=now() WHERE transfer_id=$1").bind(id).bind(serde_json::to_string(&t.receipt).map_err(internal)?).execute(&mut *tx).await.map_err(internal)?;
    // Preserve complete raw central receipt as audit evidence, not just its projection.
    sqlx::query("UPDATE source_transfers SET central_receipt=$2::jsonb WHERE transfer_id=$1")
        .bind(id)
        .bind(serde_json::to_string(&receipt).map_err(internal)?)
        .execute(&mut *tx)
        .await
        .map_err(internal)?;
    tx.commit().await.map_err(internal)?;
    // Central retained objects own durability now; incoming transport bytes are expendable.
    match tokio::fs::remove_dir_all(&incoming).await {
        Ok(()) => {
            sqlx::query("UPDATE source_transfers SET staging_released=TRUE WHERE transfer_id=$1 AND receipt IS NOT NULL").bind(id).execute(&s.store.pool).await.map_err(internal)?;
        }
        Err(e) => tracing::warn!(error=%e,"retained incoming staging cleanup deferred"),
    }
    crate::delegated_permits::receipts::source(s, n, response(id, &t)).await
}
fn push_error(e: PushError) -> AppError {
    match e {
        PushError::Refused(m) => AppError::Conflict(m),
        PushError::TooLarge(m) => AppError::PayloadTooLarge(m),
        PushError::Invalid(m) => AppError::BadRequest(m),
        PushError::Internal(e) => internal(e),
    }
}
async fn push_refs(
    State(s): State<AppState>,
    headers: HeaderMap,
    RoutePath(attempt): RoutePath<Uuid>,
    Query(q): Query<ServiceQuery>,
) -> Result<axum::response::Response> {
    if q.service != "git-receive-pack" {
        return Err(bad("only git-receive-pack is served"));
    }
    push(&s, &headers, attempt, None).await
}
async fn push_pack(
    State(s): State<AppState>,
    headers: HeaderMap,
    RoutePath(attempt): RoutePath<Uuid>,
    body: axum::body::Body,
) -> Result<axum::response::Response> {
    push(&s, &headers, attempt, Some(body)).await
}
/// Smart-HTTP `git push` into the attempt's quarantine repository. The fence is
/// the one a transfer has: the runner's credential, the node's authority over
/// the attempt, a source binding, a grant, unreleased retention. What lands
/// here is not retained work; only `pushed` admits it, and it checks again.
async fn push(
    s: &AppState,
    headers: &HeaderMap,
    attempt: Uuid,
    body: Option<axum::body::Body>,
) -> Result<axum::response::Response> {
    let n = runner(s, headers).await?;
    let rt = runtime(s)?;
    // A push waits for its turn; it is never refused because others are running.
    let _bulk = rt.bulk.acquire().await.map_err(internal)?;
    let mut tx = s.store.pool.begin().await.map_err(internal)?;
    let a = machine_authority(&mut tx, &n, attempt).await?;
    let source = binding(&a)?.clone();
    frozen(&a, &source, &a.execution_spec_digest, &a.attempt)?;
    if a.retention_released {
        return Err(conflict("attempt retention has already been released"));
    }
    sqlx::query("SELECT pg_advisory_xact_lock(719247031)")
        .execute(&mut *tx)
        .await
        .map_err(internal)?;
    cleanup(&rt, &mut tx).await?;
    let used = aq_history_caps::staging_used(&mut tx, (s.clock)().timestamp_millis())
        .await
        .map_err(internal)?;
    tx.commit().await.map_err(internal)?;
    let (quarantine, limit) = Quarantine::open(&rt.config, attempt, &source, used as u64)
        .await
        .map_err(push_error)?;
    match body {
        None => quarantine.advertise().await,
        Some(body) => quarantine.receive(&rt.config, headers, body, limit).await,
    }
    .map_err(push_error)
}
/// Admits the commit an attempt pushed: the server builds the aq-git artifact
/// from the quarantine itself and then takes the same path as an upload.
/// Idempotent on the transfer identity.
async fn pushed(
    State(s): State<AppState>,
    headers: HeaderMap,
    ApiJson(r): ApiJson<PushedRequest>,
) -> Result<axum::response::Response> {
    if r.identity.validate().is_err()
        || r.checkpoint.validate().is_err()
        || r.operation_id.is_nil()
        || r.transfer_id.is_nil()
    {
        return Err(bad("invalid pushed transfer request"));
    }
    let n = runner(&s, &headers).await?;
    if n.id() != r.identity.node_id {
        return Err(AppError::Forbidden("node identity mismatch".into()));
    }
    let rt = runtime(&s)?;
    let _bulk = rt.bulk.acquire().await.map_err(internal)?;
    let _gate = rt.finalize_gate.lock().await;
    let attempt = r.attempt.attempt_id;
    let quarantine = Quarantine::at(&rt.config, attempt, &r.source).map_err(push_error)?;
    // The first pass finds no row and builds the artifact without holding
    // locks; the second repeats every check and reserves the row. The finalize
    // gate keeps another admission from reserving it in between.
    let mut built = None;
    loop {
        let mut tx = s.store.pool.begin().await.map_err(internal)?;
        let a = machine_authority(&mut tx, &n, attempt).await?;
        frozen(&a, &r.source, &r.resolved_spec_digest, &r.attempt)?;
        let known = sqlx::query("SELECT node_id,attempt_id,manifest->'checkpoint' AS checkpoint FROM source_transfers WHERE transfer_id=$1").bind(r.transfer_id).fetch_optional(&mut *tx).await.map_err(internal)?;
        if let Some(row) = known {
            if row.get::<Uuid, _>("node_id") != n.id()
                || row.get::<Uuid, _>("attempt_id") != attempt
                || row.get::<Value, _>("checkpoint") != json!(r.checkpoint)
            {
                return Err(conflict("transfer identity names another checkpoint"));
            }
            break;
        }
        if a.retention_released {
            return Err(conflict("attempt retention has already been released"));
        }
        let Some(manifest) = built.take() else {
            drop(tx);
            built = Some(
                quarantine
                    .artifact(
                        &rt.config,
                        r.transfer_id,
                        r.attempt.clone(),
                        r.resolved_spec_digest.clone(),
                        r.source.clone(),
                        r.checkpoint.clone(),
                    )
                    .await
                    .map_err(push_error)?,
            );
            continue;
        };
        reserve(&s, &rt, &mut tx, n.id(), &manifest, true).await?;
        tx.commit().await.map_err(internal)?;
        break;
    }
    quarantine
        .promote(r.transfer_id, &rt.path(r.transfer_id))
        .await
        .map_err(push_error)?;
    let out = admit(&s, &rt, &n, r.transfer_id).await?;
    // Central retention owns the commit now; the pushed copy is expendable.
    quarantine.remove().await;
    Ok(out)
}
// Only known never-imported uploads expire. Unknown imports and retained data
// are protected regardless of age or worker disappearance.
async fn cleanup(rt: &Runtime, tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    aq_source_push::expire(&rt.config)
        .await
        .map_err(push_error)?;
    let retained=sqlx::query("SELECT transfer_id,artifact_id FROM source_transfers WHERE NOT staging_released AND ((state='retained' AND receipt IS NOT NULL) OR (state='rejected' AND NOT dispatch_intent)) FOR UPDATE SKIP LOCKED").fetch_all(&mut **tx).await.map_err(internal)?;
    for r in retained {
        let id: Uuid = r.get("transfer_id");
        let artifact: Uuid = r.get("artifact_id");
        discard_transport(rt, id, artifact).await?;
        sqlx::query("UPDATE source_transfers SET staging_released=TRUE WHERE transfer_id=$1")
            .bind(id)
            .execute(&mut **tx)
            .await
            .map_err(internal)?;
    }

    let ids:Vec<Uuid>=sqlx::query_scalar("SELECT transfer_id FROM source_transfers WHERE state='uploading' AND updated_at < now()-($1::bigint * interval '1 second') FOR UPDATE SKIP LOCKED").bind(rt.config.incomplete_ttl_secs.min(i64::MAX as u64) as i64).fetch_all(&mut **tx).await.map_err(internal)?;
    for id in ids {
        let path = rt.path(id);
        if tokio::fs::try_exists(&path).await.map_err(internal)? {
            tokio::fs::remove_dir_all(path).await.map_err(internal)?;
        }
        sqlx::query(
            "UPDATE source_transfers SET state='rejected',staging_released=TRUE,updated_at=now() WHERE transfer_id=$1",
        )
        .bind(id)
        .execute(&mut **tx)
        .await
        .map_err(internal)?;
    }
    Ok(())
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvisionBase {
    pub attempt_id: Uuid,
    pub operation_id: Uuid,
    pub retention_handle: Uuid,
}
async fn provision_base(
    State(s): State<AppState>,
    _owner: crate::authz::RequireOwner,
    ApiJson(r): ApiJson<ProvisionBase>,
) -> Result<Json<TransferManifest>> {
    if r.operation_id.is_nil() || r.retention_handle.is_nil() {
        return Err(bad("non-nil base operation and handle required"));
    }
    let rt = runtime(&s)?;
    let _bulk = rt
        .bulk
        .try_acquire()
        .map_err(|_| AppError::Maintenance("source bulk capacity exhausted".into()))?;
    let mut tx = s.store.pool.begin().await.map_err(internal)?;
    let node: Uuid =
        sqlx::query_scalar("SELECT node_id FROM execution_attempts WHERE attempt_id=$1")
            .bind(r.attempt_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(internal)?
            .ok_or_else(|| AppError::NotFound("attempt not found".into()))?;
    let a = authority(&mut tx, node, r.attempt_id).await?;
    let b = binding(&a)?.clone();
    let existing=sqlx::query("SELECT operation_id,retention_handle,manifest::text AS manifest FROM source_bases WHERE attempt_id=$1 FOR UPDATE").bind(r.attempt_id).fetch_optional(&mut *tx).await.map_err(internal)?;
    if let Some(old) = existing {
        if old.get::<Uuid, _>("operation_id") != r.operation_id
            || old.get::<Uuid, _>("retention_handle") != r.retention_handle
        {
            return Err(conflict("base provisioning identity conflict"));
        }
        if let Some(m) = old.get::<Option<&str>, _>("manifest") {
            return Ok(Json(serde_json::from_str(m).map_err(internal)?));
        }
    } else {
        sqlx::query("SELECT pg_advisory_xact_lock(719247031)")
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
        let used = aq_history_caps::staging_used(&mut tx, (s.clock)().timestamp_millis())
            .await
            .map_err(internal)?;
        let reserved = rt.config.max_bundle_bytes + 1024 * 1024;
        if (used as u64).saturating_add(reserved) > rt.config.max_staging_bytes {
            return Err(AppError::PayloadTooLarge(
                "base staging quota exhausted".into(),
            ));
        }
        sqlx::query("INSERT INTO source_bases(attempt_id,node_id,operation_id,retention_handle,reserved_bytes) VALUES($1,$2,$3,$4,$5)").bind(r.attempt_id).bind(node).bind(r.operation_id).bind(r.retention_handle).bind(reserved as i64).execute(&mut *tx).await.map_err(internal)?;
    }
    tx.commit().await.map_err(internal)?;
    let request = json!({"protocol_version":1,"request_id":format!("base-export-{}",r.operation_id),"operation":"artifact.export","repository_id":b.repository_id,"operation_id":r.operation_id,"expected_binding_generation":b.binding_generation,"params":{"source":b.base,"base":b.base,"retention_handle":r.retention_handle,"confirmed_prerequisites":[]},"deadline_ms":30000});
    let _result = export_base(&rt, &request).await?;
    let dir = rt
        .config
        .git_root
        .join("artifacts")
        .join(r.operation_id.to_string());
    let bytes = tokio::fs::read(dir.join("manifest.json"))
        .await
        .map_err(internal)?;
    if bytes.len() > 1024 * 1024 {
        return Err(bad("exported manifest too large"));
    }
    let m: GitManifest = serde_json::from_slice(&bytes).map_err(internal)?;
    if m.repository_id != b.repository_id.to_string()
        || m.repository_identity != b.logical_identity
        || m.binding_generation != b.binding_generation
        || m.source != b.base
        || m.base != b.base
        || m.transfer_id != r.operation_id.to_string()
        || m.bundle_size > rt.config.max_bundle_bytes
    {
        return Err(conflict("exported base binding mismatch"));
    }
    let manifest = TransferManifest {
        transfer_id: r.operation_id,
        role: aq_worker_protocol::SourceRole::Base,
        checkpoint: b.base.clone(),
        attempt: a.attempt,
        resolved_spec_digest: a.execution_spec_digest,
        source: b,
        artifact_id: r.operation_id,
        manifest_digest: digest(&bytes),
        bundle_digest: m.bundle_sha256,
        manifest_bytes: bytes.len() as u64,
        bundle_bytes: m.bundle_size,
    };
    manifest
        .validate()
        .map_err(|_| bad("exported base manifest invalid"))?;
    let mut tx = s.store.pool.begin().await.map_err(internal)?;
    let current = authority(&mut tx, node, r.attempt_id).await?;
    if binding(&current)? != &manifest.source {
        return Err(conflict("base binding changed"));
    }
    sqlx::query("UPDATE source_bases SET manifest=$2::jsonb,reserved_bytes=$4 WHERE attempt_id=$1 AND operation_id=$3").bind(r.attempt_id).bind(serde_json::to_string(&manifest).map_err(internal)?).bind(r.operation_id).bind((manifest.manifest_bytes+manifest.bundle_bytes) as i64).execute(&mut *tx).await.map_err(internal)?;
    tx.commit().await.map_err(internal)?;
    Ok(Json(manifest))
}
async fn export_base(rt: &Runtime, request: &Value) -> Result<Value> {
    // Private, finite artifact.export mapping; public routes cannot choose operation.
    let exchange = socket_call(&rt.config.git_socket, request);
    tokio::time::timeout(Duration::from_secs(35), exchange)
        .await
        .map_err(|_| {
            AppError::UpdatesUnavailable("base export pending; retry same operation".into())
        })?
        .map_err(|e| {
            tracing::warn!(error=%e,"base export uncertain");
            AppError::UpdatesUnavailable("base export pending; retry same operation".into())
        })
}
async fn load_base(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    id: Uuid,
) -> Result<TransferManifest> {
    let a = authority(tx, node, id).await?;
    let row=sqlx::query("SELECT manifest::text AS manifest,profiled_export_operation_id FROM source_bases WHERE attempt_id=$1 AND node_id=$2")
        .bind(id).bind(node).fetch_optional(&mut **tx).await.map_err(internal)?
        .ok_or_else(||AppError::NotFound("base is not provisioned".into()))?;
    if row
        .get::<Option<Uuid>, _>("profiled_export_operation_id")
        .is_some()
    {
        return Err(AppError::Forbidden(
            "native source transfer requires current-credential native API".into(),
        ));
    }
    let body: Option<String> = row.get("manifest");
    let m: TransferManifest = serde_json::from_str(
        &body.ok_or_else(|| AppError::NotFound("base is not provisioned".into()))?,
    )
    .map_err(internal)?;
    if binding(&a)? != &m.source
        || m.attempt != a.attempt
        || m.resolved_spec_digest != a.execution_spec_digest
        || m.role != aq_worker_protocol::SourceRole::Base
    {
        return Err(AppError::Forbidden("base assignment mismatch".into()));
    }
    Ok(m)
}
async fn base_status(
    State(s): State<AppState>,
    headers: HeaderMap,
    RoutePath(id): RoutePath<Uuid>,
) -> Result<Json<TransferManifest>> {
    let n = authenticate_context(&s, &headers).await?;
    let mut tx = s.store.pool.begin().await.map_err(internal)?;
    lock_current_node(&mut tx, &n).await?;
    Ok(Json(load_base(&mut tx, n.id(), id).await?))
}
async fn base_download(
    State(s): State<AppState>,
    headers: HeaderMap,
    RoutePath((id, file)): RoutePath<(Uuid, String)>,
    Query(q): Query<DownloadQuery>,
) -> Result<Bytes> {
    let n = authenticate_context(&s, &headers).await?;
    let rt = runtime(&s)?;
    let _bulk = rt
        .bulk
        .try_acquire()
        .map_err(|_| AppError::Maintenance("source bulk capacity exhausted".into()))?;
    if q.limit == 0 || q.limit > chunk_size() {
        return Err(bad("download chunk limit exceeded"));
    }
    let mut tx = s.store.pool.begin().await.map_err(internal)?;
    lock_current_node(&mut tx, &n).await?;
    let m = load_base(&mut tx, n.id(), id).await?;
    let name = filename(&file)?;
    let limit = if name == "manifest.json" {
        m.manifest_bytes
    } else {
        m.bundle_bytes
    };
    if q.offset > limit {
        return Err(bad("download offset outside artifact"));
    }
    let mut f = tokio::fs::File::open(
        rt.config
            .git_root
            .join("artifacts")
            .join(m.artifact_id.to_string())
            .join(name),
    )
    .await
    .map_err(internal)?;
    f.seek(std::io::SeekFrom::Start(q.offset))
        .await
        .map_err(internal)?;
    let mut out = vec![0; q.limit.min((limit - q.offset) as usize)];
    f.read_exact(&mut out).await.map_err(internal)?;
    Ok(Bytes::from(out))
}

async fn discard_transport(rt: &Runtime, id: Uuid, artifact: Uuid) -> Result<()> {
    for p in [
        rt.path(id),
        rt.config
            .git_root
            .join("artifacts/incoming")
            .join(artifact.to_string()),
    ] {
        if tokio::fs::try_exists(&p).await.map_err(internal)? {
            tokio::fs::remove_dir_all(p).await.map_err(internal)?;
        }
    }
    Ok(())
}

pub mod profiled_base;

/// Actual source-owner acknowledgement retained after the public Git import.
/// This records historical retention, not current physical availability or B2
/// eligibility. Caller holds common node/attempt locks before this transfer row.
pub struct RecordedSourceConfirmation {
    pub manifest: TransferManifest,
    pub receipt: CentralRetentionReceipt,
    pub acknowledgement: Value,
}
/// Resolve an actual import operation, never a worker's unverified receipt ID.
/// No current machine credential is needed to read this historical owner record.
pub async fn load_confirmed_locked(
    tx: &mut Transaction<'_, Postgres>,
    operation: Uuid,
    recorded: &crate::profiled_execution::results::RecordedResult,
) -> Result<RecordedSourceConfirmation> {
    if !recorded.report.source_receipt_ids.contains(&operation) {
        return Err(conflict(
            "source receipt is not pinned by the recorded result",
        ));
    }
    let row = sqlx::query("SELECT transfer_id,node_id,attempt_id,repository_id,artifact_id,manifest,state,receipt,central_receipt FROM source_transfers WHERE import_operation_id=$1 FOR SHARE")
        .bind(operation).fetch_optional(&mut **tx).await.map_err(internal)?
        .ok_or_else(|| conflict("source owner acknowledgement is unavailable"))?;
    if row.get::<String, _>("state") != "retained" {
        return Err(conflict("source owner acknowledgement is not retained"));
    }
    let manifest: TransferManifest =
        serde_json::from_value(row.get("manifest")).map_err(internal)?;
    manifest.validate().map_err(internal)?;
    let receipt: CentralRetentionReceipt =
        serde_json::from_value(row.get("receipt")).map_err(internal)?;
    receipt.validate().map_err(internal)?;
    let acknowledgement: Value = row.get("central_receipt");
    let spec = &recorded.spec;
    let binding = &recorded.report.binding;
    // Recheck the same public acknowledgement relationships as finalize(). The
    // projection alone cannot substitute for the retained Git-owner evidence.
    let aq_session_protocol::SessionIdentity::Remote {
        node_id,
        session_id,
        ..
    } = binding.session
    else {
        return Err(AppError::Internal);
    };
    if row.get::<Uuid, _>("node_id") != node_id {
        return Err(AppError::Internal);
    }
    if manifest.source.base.hex.bytes().all(|b| b == b'0')
        || manifest.checkpoint.hex.bytes().all(|b| b == b'0')
        || manifest.role != aq_worker_protocol::SourceRole::Checkpoint
        || row.get::<Uuid, _>("attempt_id") != binding.attempt_id
        || manifest.attempt.attempt_id != binding.attempt_id
        || manifest.attempt.session_id != session_id
        || manifest.attempt.generation != binding.assignment_generation
        || manifest.resolved_spec_digest != binding.execution_spec_digest.as_str()
        || manifest.source.repository_id != spec.source.repository_id
        || manifest.source.binding_generation != spec.source.repository_binding_generation
        || manifest.source.base.hex != spec.source.resolved_object.hex
        || !matches!(
            (
                &manifest.source.base.algorithm,
                &spec.source.resolved_object.format
            ),
            (
                aq_worker_protocol::ObjectAlgorithm::Sha1,
                aq_admission::ObjectFormat::Sha1
            )
        )
        || row.get::<Uuid, _>("repository_id") != manifest.source.repository_id
        || row.get::<Uuid, _>("transfer_id") != manifest.transfer_id
        || row.get::<Uuid, _>("artifact_id") != manifest.artifact_id
        || receipt.receipt_id != operation
        || receipt.transfer_id != manifest.transfer_id
        || receipt.repository_id != manifest.source.repository_id
        || receipt.binding_generation != manifest.source.binding_generation
        || receipt.retention_handle != manifest.artifact_id
        || receipt.source != manifest.checkpoint
        || acknowledgement["repository_id"] != receipt.repository_id.to_string()
        || acknowledgement["binding_generation"] != receipt.binding_generation
        || acknowledgement["transfer_id"] != manifest.artifact_id.to_string()
        || acknowledgement["retention_handle"] != manifest.artifact_id.to_string()
        || acknowledgement["manifest_digest"] != receipt.manifest_digest
        || acknowledgement["source"] != serde_json::to_value(&receipt.source).map_err(internal)?
        || acknowledgement["retention_state"] != "active"
        || acknowledgement["historical"] != false
        || acknowledgement["receiver_installation_id"]
            .as_str()
            .and_then(|v| Uuid::parse_str(v).ok())
            .is_none_or(|v| v.is_nil())
    {
        return Err(AppError::Internal);
    }
    Ok(RecordedSourceConfirmation {
        manifest,
        receipt,
        acknowledgement,
    })
}

pub mod result_retention;

pub mod gc;
