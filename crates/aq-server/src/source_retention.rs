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
    CentralRetentionReceipt, SourceBinding, TransferManifest, TransferStatus,
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
use sha2::{Digest, Sha256};
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
fn digest(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}
pub use aq_row_retention::source_config::SourceRetentionConfig;
pub struct Runtime {
    pub config: SourceRetentionConfig,
    bulk: Semaphore,
    finalize_gate: tokio::sync::Mutex<()>,
}
impl Runtime {
    pub fn new(config: SourceRetentionConfig) -> anyhow::Result<Self> {
        config.validate()?;
        if let Ok(meta) = std::fs::symlink_metadata(&config.git_root) {
            anyhow::ensure!(
                !meta.file_type().is_symlink() && meta.is_dir(),
                "symlink or non-directory git root refused"
            );
        }
        {
            let p = &config.staging_root;
            if let Ok(meta) = std::fs::symlink_metadata(p) {
                anyhow::ensure!(
                    !meta.file_type().is_symlink() && meta.is_dir(),
                    "symlink or non-directory staging root refused"
                );
            }
            std::fs::create_dir_all(p)?;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700))?;
            anyhow::ensure!(
                !std::fs::symlink_metadata(p)?.file_type().is_symlink(),
                "symlink staging root refused"
            );
        }
        Ok(Self {
            bulk: Semaphore::new(config.bulk_concurrency),
            finalize_gate: tokio::sync::Mutex::new(()),
            config,
        })
    }
    fn admit_bulk(&self) -> Result<tokio::sync::SemaphorePermit<'_>> {
        self.bulk
            .try_acquire()
            .map_err(|_| AppError::Maintenance("source bulk capacity exhausted".into()))
    }
    fn path(&self, id: Uuid) -> PathBuf {
        self.config.staging_root.join(id.to_string())
    }
    /// Bulk transport admission shared with lifecycle artifact retention.
    fn git_wait(&self) -> Duration {
        Duration::from_millis(self.config.git_deadline_ms + 5_000)
    }
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
fn filename(name: &str) -> Result<&'static str> {
    match name {
        "manifest" => Ok("manifest.json"),
        "bundle" => Ok("objects.bundle"),
        _ => Err(bad("invalid artifact file")),
    }
}
fn chunk_size() -> usize {
    256 * 1024
}
mod base;
mod confirmation;
mod finalize;
use aq_delivery::source_transport::{GitManifest, RetentionRecord, successful_reply};
mod upload;
use base::{base_download, base_status, provision_base};
pub(crate) use confirmation::first_confirmed_locked;
pub use confirmation::{RecordedSourceConfirmation, load_confirmed_locked};
use finalize::finalize;
use upload::{begin, chunk, status};

// Legacy export and import use the same bounded wire protocol. Callers choose
// the fixed operation and turn uncertain outcomes into their original retry response.
async fn artifact_exchange(rt: &Runtime, request: &Value) -> anyhow::Result<Value> {
    let mut stream = tokio::net::UnixStream::connect(&rt.config.git_socket).await?;
    let mut bytes = serde_json::to_vec(request)?;
    bytes.push(b'\n');
    stream.write_all(&bytes).await?;
    let mut frame = Vec::new();
    BufReader::new(stream)
        .take(1024 * 1024 + 1)
        .read_until(b'\n', &mut frame)
        .await?;
    anyhow::ensure!(
        frame.len() <= 1024 * 1024 && frame.last() == Some(&b'\n'),
        "invalid aq-git artifact frame"
    );
    let reply: Value = serde_json::from_slice(&frame)?;
    anyhow::ensure!(
        successful_reply(&reply, &request["request_id"])
            && reply["operation_id"] == request["operation_id"],
        "unconfirmed aq-git artifact operation"
    );
    reply
        .get("result")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("missing aq-git artifact result"))
}
async fn authenticate_upload(s: &AppState, headers: &HeaderMap) -> Result<AuthenticatedNode> {
    crate::delegated_permits::runtime::authenticate_runner(
        s,
        headers,
        aq_admission_sandbox::sandbox::RunnerOperation::ReportResult,
        None,
    )
    .await
}
async fn transfer_attempt(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    id: Uuid,
) -> Result<Uuid> {
    // Discover the attempt without taking the transfer lock before Node -> Attempt.
    sqlx::query_scalar(
        "SELECT attempt_id FROM source_transfers WHERE transfer_id=$1 AND node_id=$2",
    )
    .bind(id)
    .bind(node)
    .fetch_optional(&mut **tx)
    .await
    .map_err(internal)?
    .ok_or_else(|| AppError::NotFound("source transfer not found".into()))
}
async fn authorized_transfer(
    tx: &mut Transaction<'_, Postgres>,
    node: &AuthenticatedNode,
    attempt: Uuid,
    id: Uuid,
) -> Result<Transfer> {
    let a = machine_authority(tx, node, attempt).await?;
    let t = transfer(tx, node.id(), id).await?;
    check_manifest(&a, &t.manifest)?;
    Ok(t)
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
fn binding(a: &crate::execution::authority::AuthorityView) -> Result<&SourceBinding> {
    a.source
        .as_ref()
        .ok_or_else(|| AppError::Forbidden("attempt has no source binding".into()))
}
fn matches_attempt(m: &TransferManifest, a: &crate::execution::authority::AuthorityView) -> bool {
    m.resolved_spec_digest == a.execution_spec_digest && m.attempt == a.attempt
}
fn check_manifest(
    a: &crate::execution::authority::AuthorityView,
    m: &TransferManifest,
) -> Result<()> {
    m.validate().map_err(|_| bad("invalid transfer manifest"))?;
    if a.kind == crate::execution::authority::Kind::ProfiledV1 && a.grant_id.is_none() {
        return Err(AppError::Forbidden(
            "checkpoint transfer requires an issued execution grant".into(),
        ));
    }

    let bound = m.role == aq_worker_protocol::SourceRole::Checkpoint && &m.source == binding(a)?;
    if !bound || !matches_attempt(m, a) {
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
// Only known never-imported uploads expire. Unknown imports and retained data
// are protected regardless of age or worker disappearance.
async fn cleanup(rt: &Runtime, tx: &mut Transaction<'_, Postgres>) -> Result<()> {
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

pub mod result_retention;

pub mod gc;
