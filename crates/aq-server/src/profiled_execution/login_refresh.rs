//! C5b: one stored subscription login per credential, versioned. Secret bytes
//! live only in owner-only files under the configured `material_store`;
//! PostgreSQL holds digests and metadata.
//!
//! A lineage is keyed by (credential identity, registry revision, base), where
//! base is the startup fingerprint of the configured file (version 0). Each
//! stored entry is the next version: either a node `refresh` written back with
//! compare-and-swap against the current version, or an `operator` upload,
//! which always wins. The next attempt is issued from the newest version.
//!
//! Renewal compares credential identity, not bytes: a refreshed version of the
//! same credential never refuses renewal, whichever lease wrote it. Only an
//! operator replace (a newer `operator` entry, or a rotated configured file,
//! which starts a new lineage) or revocation stops renewal.
pub mod api;
pub use aq_login_store::{
    Entry, HeadSource, Lineage, Origin, append_locked, codex, head, head_digest, head_version,
    identity, lease_refusal, lineage_locked, mask_stale, read, refresh_of_head, renewable,
    stale_logins, store, version_of,
};

use crate::{error::AppError, intelligence::config::LoadedRegistry};
use aq_admission::MaterialLease;
use sqlx::{Postgres, Transaction};

pub use aq_login_store::operator::{OperatorUpload, ReplaceError, operator_replace};

/// Newest lineage revision for issuing the next attempt's lease; None means
/// the configured file is newest.
pub async fn newest_source(
    executor: impl sqlx::PgExecutor<'_>,
    loaded: &LoadedRegistry,
    profile: &aq_selection::ArtifactRef,
    revision: u64,
) -> Result<Option<HeadSource>, AppError> {
    Ok(aq_login_store::sources::newest_source(executor, loaded, profile, revision).await?)
}
/// Lineage revision backing an issued lease; None means the lease was issued
/// from the configured file (its digest is checked against the file).
pub async fn lease_source(
    executor: impl sqlx::PgExecutor<'_>,
    loaded: &LoadedRegistry,
    lease: &MaterialLease,
) -> Result<Option<HeadSource>, AppError> {
    Ok(aq_login_store::sources::lease_source(executor, loaded, lease).await?)
}
/// [`lease_source`] plus the login version the lease was issued from (`None`
/// for credentials without a versioned login, or a digest no version names).
/// The node sends this version back as `base_version` on write-back.
pub async fn lease_versioned(
    executor: impl sqlx::PgExecutor<'_>,
    loaded: &LoadedRegistry,
    lease: &MaterialLease,
) -> Result<(Option<HeadSource>, Option<i64>), AppError> {
    Ok(aq_login_store::sources::lease_versioned(executor, loaded, lease).await?)
}

/// Renewal fence. An ApiKey credential keeps the original rule: the startup
/// fingerprint must equal the lease digest. A subscription-login lease stays
/// renewable across refreshed versions of the same credential (see
/// [`renewable`]).
pub async fn continuous_locked(
    tx: &mut Transaction<'_, Postgres>,
    loaded: &LoadedRegistry,
    index: usize,
    lease: &MaterialLease,
) -> Result<bool, AppError> {
    Ok(aq_login_store::sources::continuous_locked(tx, loaded, index, lease).await?)
}
