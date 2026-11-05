//! SBX25 measured storage. Nodes report disk accounting with their inventory;
//! the central host samples its own root filesystem and database size. The
//! latest report per subject feeds placement (refusal above 85%) and the F4
//! `storage.pressure` episode (opens at 80%, resolves below 75%).
use crate::error::AppError;
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;
use std::path::Path;
use uuid::Uuid;

/// Placement refuses a node whose used share is strictly above this.
pub const REFUSE_ABOVE_BPS: i32 = 8500;
/// A `storage.pressure` episode opens when usage reaches this share.
pub const OPEN_AT_BPS: i32 = 8000;
/// An open `storage.pressure` episode resolves once usage drops below this.
pub const RESOLVE_BELOW_BPS: i32 = 7500;
/// Subject of the central host's own report.
pub const CENTRAL: &str = "central";
/// Central state root sampled with statvfs.
pub const CENTRAL_ROOT: &str = "/var/lib/aq";

/// Measured disk accounting. Older nodes omit the whole report.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageReport {
    pub disk_total_bytes: u64,
    pub disk_used_bytes: u64,
    pub disk_free_bytes: u64,
    pub disk_reserved_bytes: u64,
    #[serde(default)]
    pub preserved_count: u64,
    #[serde(default)]
    pub preserved_bytes: u64,
    /// Boat backends only: bytes held by retained snapshots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retained_snapshot_bytes: Option<u64>,
    /// Central only: `pg_database_size` of the server database.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database_bytes: Option<u64>,
}

impl StorageReport {
    pub fn validate(&self) -> Result<(), AppError> {
        let max = i64::MAX as u64;
        if self.disk_total_bytes == 0
            || self.disk_used_bytes > self.disk_total_bytes
            || self.disk_free_bytes > self.disk_total_bytes
            || self.disk_reserved_bytes > self.disk_total_bytes
            || self.disk_total_bytes > max
            || self.preserved_bytes > max
            || self.retained_snapshot_bytes.is_some_and(|b| b > max)
            || self.database_bytes.is_some_and(|b| b > max)
        {
            return Err(AppError::BadRequest("invalid storage report".into()));
        }
        Ok(())
    }

    /// Used share of the disk in basis points (0..=10000).
    pub fn used_bps(&self) -> i32 {
        if self.disk_total_bytes == 0 {
            return 10_000;
        }
        let bps = u128::from(self.disk_used_bytes) * 10_000 / u128::from(self.disk_total_bytes);
        bps.min(10_000) as i32
    }

    /// statvfs accounting: blocks free to unprivileged users are `free`; the
    /// root-only remainder is `reserved`.
    pub fn from_statvfs(path: &Path) -> Option<Self> {
        let s = nix::sys::statvfs::statvfs(path).ok()?;
        let unit = s.fragment_size() as u64;
        let total = (s.blocks() as u64).checked_mul(unit)?;
        let bfree = (s.blocks_free() as u64).checked_mul(unit)?.min(total);
        let avail = (s.blocks_available() as u64).checked_mul(unit)?.min(bfree);
        Some(Self {
            disk_total_bytes: total,
            disk_used_bytes: total - bfree,
            disk_free_bytes: avail,
            disk_reserved_bytes: bfree - avail,
            ..Self::default()
        })
    }
}

/// Upsert the latest report for `subject` (a node id or [`CENTRAL`]).
pub async fn record(
    conn: &mut PgConnection,
    subject: &str,
    node_id: Option<Uuid>,
    report: &StorageReport,
    observed_at_ms: u64,
) -> Result<(), AppError> {
    report.validate()?;
    let value = serde_json::to_value(report).map_err(|_| AppError::Internal)?;
    sqlx::query(
        "INSERT INTO node_storage_reports(subject,node_id,used_bps,report,observed_at_ms) \
         VALUES($1,$2,$3,$4,$5) ON CONFLICT(subject) DO UPDATE SET node_id=excluded.node_id,\
         used_bps=excluded.used_bps,report=excluded.report,observed_at_ms=excluded.observed_at_ms",
    )
    .bind(subject)
    .bind(node_id)
    .bind(report.used_bps())
    .bind(value)
    .bind(i64::try_from(observed_at_ms).map_err(|_| AppError::Internal)?)
    .execute(conn)
    .await
    .map_err(|_| AppError::Internal)?;
    Ok(())
}

/// Nodes among `nodes` whose latest report is above the refusal line.
pub async fn pressured(conn: &mut PgConnection, nodes: &[Uuid]) -> Result<Vec<Uuid>, AppError> {
    sqlx::query_scalar(
        "SELECT node_id FROM node_storage_reports WHERE node_id = ANY($1) AND used_bps > $2",
    )
    .bind(nodes)
    .bind(REFUSE_ABOVE_BPS)
    .fetch_all(conn)
    .await
    .map_err(|_| AppError::Internal)
}

/// The latest report per node among `nodes`, with its used share and
/// observation time. A stored report that no longer parses is omitted.
pub async fn latest(
    conn: &mut PgConnection,
    nodes: &[Uuid],
) -> Result<Vec<(Uuid, StorageReport, i32, u64)>, AppError> {
    let rows: Vec<(Uuid, serde_json::Value, i32, i64)> = sqlx::query_as(
        "SELECT node_id, report, used_bps, observed_at_ms FROM node_storage_reports \
         WHERE node_id = ANY($1)",
    )
    .bind(nodes)
    .fetch_all(conn)
    .await
    .map_err(|_| AppError::Internal)?;
    Ok(rows
        .into_iter()
        .filter_map(|(node, report, bps, at)| {
            Some((
                node,
                serde_json::from_value(report).ok()?,
                bps,
                u64::try_from(at).ok()?,
            ))
        })
        .collect())
}

/// Sample the central root filesystem and database size into the `central`
/// report. A missing root (development hosts) records nothing.
pub async fn sample_central(
    pool: &sqlx::PgPool,
    root: &Path,
    now_ms: u64,
) -> Result<bool, AppError> {
    let root = root.to_owned();
    let Some(mut report) = tokio::task::spawn_blocking(move || StorageReport::from_statvfs(&root))
        .await
        .map_err(|_| AppError::Internal)?
    else {
        return Ok(false);
    };
    let mut conn = pool.acquire().await.map_err(|_| AppError::Internal)?;
    let size: i64 = sqlx::query_scalar("SELECT pg_database_size(current_database())")
        .fetch_one(&mut *conn)
        .await
        .map_err(|_| AppError::Internal)?;
    report.database_bytes = u64::try_from(size).ok();
    record(&mut conn, CENTRAL, None, &report, now_ms).await?;
    Ok(true)
}

/// SBX22(a) node preserve event: one `sandbox.preserved` notification per
/// event, carrying the node, bytes held and the preservation expiry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreservedEvent {
    pub event_id: Uuid,
    pub attempt_id: Uuid,
    pub preserved_bytes: u64,
    pub expires_at_ms: u64,
}

pub fn routes(state: crate::AppState) -> axum::Router<crate::AppState> {
    axum::Router::new()
        .route(
            "/v1/worker/storage/preserved",
            axum::routing::post(preserved),
        )
        .layer(axum::extract::DefaultBodyLimit::max(4096))
        .with_state(state)
}

async fn preserved(
    axum::extract::State(state): axum::extract::State<crate::AppState>,
    headers: axum::http::HeaderMap,
    axum::Json(event): axum::Json<PreservedEvent>,
) -> Result<axum::Json<serde_json::Value>, AppError> {
    use crate::notifications::Error;
    let auth = crate::worker_api::authenticate_context(&state, &headers).await?;
    let policy =
        crate::notifications::policy::Policy::new(crate::notifications::api::service(&state));
    match policy
        .observe_preserved(
            event.event_id,
            auth.node().id,
            event.attempt_id,
            event.preserved_bytes,
            event.expires_at_ms,
        )
        .await
    {
        Ok(result) => Ok(axum::Json(
            serde_json::json!({ "observed": result.is_some() }),
        )),
        Err(Error::Invalid) => Err(AppError::BadRequest("invalid preserve event".into())),
        Err(Error::NotFound) => Err(AppError::NotFound("attempt not found on node".into())),
        Err(_) => Err(AppError::Internal),
    }
}
