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
    Entry, Lineage, Origin, append_locked, codex, head, head_digest, head_version, identity,
    lineage_locked, read, renewable, store, version_of,
};

use crate::{error::AppError, intelligence::config::LoadedRegistry};
use aq_admission::MaterialLease;
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use crate::error::db;

/// Why a stored login of `provider` is not leased now: the owner's Codex login
/// age limit (Settings › Providers), with a reason the node shows. None when it
/// may be leased. An attempt's material fetch and a session's lease both ask.
pub async fn lease_refusal(
    conn: &mut sqlx::PgConnection,
    provider: &str,
    login: &[u8],
    now_ms: i64,
) -> Result<Option<String>, AppError> {
    if provider != codex::PROVIDER {
        return Ok(None);
    }
    let limit = aq_swarm_settings::providers::codex_login_max_age_days(conn)
        .await
        .map_err(db)?;
    Ok(codex::stale(login, limit, now_ms))
}

/// Server-side rule for a node write-back of a Codex login, on the attempt and
/// the session route alike: `new` must be the lineage's newest login refreshed
/// ([`codex::refreshed`]). Other providers' logins are not inspected. The
/// caller holds the lineage lock.
pub(crate) async fn refresh_of_head(
    registry: std::sync::Arc<LoadedRegistry>,
    index: usize,
    lineage: &Lineage,
    dir: &std::path::Path,
    new: std::sync::Arc<zeroize::Zeroizing<Vec<u8>>>,
) -> Result<bool, AppError> {
    if registry.credentials[index].provider != codex::PROVIDER {
        return Ok(true);
    }
    let source = head(lineage).map(|e| HeadSource::of(e, dir));
    tokio::task::spawn_blocking(move || {
        super::material::source_or_configured(&registry, index, source.as_ref())
            .is_ok_and(|stored| codex::refreshed(stored.bytes(), &new))
    })
    .await
    .map_err(|_| AppError::Internal)
}

/// A lineage entry to serve instead of the configured file. Opaque to logs.
#[derive(Clone)]
pub struct HeadSource {
    pub revision_id: Uuid,
    pub digest: String,
    pub bytes: u64,
    pub dir: std::path::PathBuf,
}
impl std::fmt::Debug for HeadSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HeadSource").finish_non_exhaustive()
    }
}
impl HeadSource {
    pub(crate) fn of(entry: &Entry, dir: &std::path::Path) -> Self {
        Self {
            revision_id: entry.revision_id,
            digest: entry.content_digest.clone(),
            bytes: entry.content_bytes as u64,
            dir: dir.to_path_buf(),
        }
    }
}
async fn lineage_for(
    executor: impl sqlx::PgExecutor<'_>,
    loaded: &LoadedRegistry,
    profile: &aq_selection::ArtifactRef,
    revision: u64,
) -> Result<Option<(Lineage, std::path::PathBuf)>, AppError> {
    let Some(index) = loaded
        .credentials
        .iter()
        .position(|c| &c.profile == profile && c.revision == revision)
    else {
        return Ok(None);
    };
    let (Some(configured), Some(Some(base)), Some(dir)) = (
        loaded.config.credentials.get(index),
        loaded.fingerprints.get(index),
        loaded.config.material_store.as_ref(),
    ) else {
        return Ok(None);
    };
    if configured.kind != aq_selection::CredentialKind::SubscriptionLogin {
        return Ok(None);
    }
    let lineage = read(executor, &identity(profile), revision, base).await?;
    Ok(Some((lineage, dir.clone())))
}
/// Newest lineage revision for issuing the next attempt's lease; None means
/// the configured file is newest.
pub async fn newest_source(
    executor: impl sqlx::PgExecutor<'_>,
    loaded: &LoadedRegistry,
    profile: &aq_selection::ArtifactRef,
    revision: u64,
) -> Result<Option<HeadSource>, AppError> {
    Ok(lineage_for(executor, loaded, profile, revision)
        .await?
        .and_then(|(l, dir)| head(&l).map(|e| HeadSource::of(e, &dir))))
}
/// Lineage revision backing an issued lease; None means the lease was issued
/// from the configured file (its digest is checked against the file).
pub async fn lease_source(
    executor: impl sqlx::PgExecutor<'_>,
    loaded: &LoadedRegistry,
    lease: &MaterialLease,
) -> Result<Option<HeadSource>, AppError> {
    Ok(lease_versioned(executor, loaded, lease).await?.0)
}
/// [`lease_source`] plus the login version the lease was issued from (`None`
/// for credentials without a versioned login, or a digest no version names).
/// The node sends this version back as `base_version` on write-back.
pub async fn lease_versioned(
    executor: impl sqlx::PgExecutor<'_>,
    loaded: &LoadedRegistry,
    lease: &MaterialLease,
) -> Result<(Option<HeadSource>, Option<i64>), AppError> {
    let Some((l, dir)) = lineage_for(
        executor,
        loaded,
        &lease.credential_profile,
        lease.credential_revision,
    )
    .await?
    else {
        return Ok((None, None));
    };
    let digest = lease.material_digest.as_str();
    let source = l
        .entries
        .iter()
        .rev()
        .find(|e| e.content_digest == digest)
        .map(|e| HeadSource::of(e, &dir));
    Ok((source, version_of(&l, digest)))
}

/// Configured credential the lease names, with its index, if it is still in
/// the loaded registry at the same revision.
pub(crate) fn configured_index(loaded: &LoadedRegistry, lease: &MaterialLease) -> Option<usize> {
    loaded.credentials.iter().position(|c| {
        c.profile == lease.credential_profile && c.revision == lease.credential_revision
    })
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
    let (Some(credential), Some(configured), Some(Some(base))) = (
        loaded.credentials.get(index),
        loaded.config.credentials.get(index),
        loaded.fingerprints.get(index),
    ) else {
        return Ok(false);
    };
    if configured.kind != aq_selection::CredentialKind::SubscriptionLogin {
        return Ok(base == lease.material_digest.as_str());
    }
    let lineage = lineage_locked(
        tx,
        &identity(&credential.profile),
        credential.revision,
        base,
    )
    .await?;
    Ok(renewable(&lineage, lease.material_digest.as_str()))
}

/// Why an operator upload was refused. Carries no secret bytes.
#[derive(Debug, thiserror::Error)]
pub enum ReplaceError {
    #[error("no enabled subscription-login credential {0} at that revision")]
    UnknownCredential(String),
    #[error("[intelligence] material_store is not configured")]
    NoStore,
    #[error("the login must be 1..={} bytes", store::MAX_BYTES)]
    Size,
    #[error("database error")]
    Internal,
}

/// Operator import/replace: stores `bytes` as the newest version of the
/// credential's login and returns that version. Always wins over node
/// write-backs; live leases issued from older versions stop renewing.
pub async fn operator_replace(
    pool: &sqlx::PgPool,
    loaded: &LoadedRegistry,
    profile: &aq_selection::ArtifactRef,
    revision: u64,
    bytes: std::sync::Arc<zeroize::Zeroizing<Vec<u8>>>,
    now_ms: i64,
) -> Result<i64, ReplaceError> {
    let name = identity(profile);
    if bytes.is_empty() || bytes.len() > store::MAX_BYTES {
        return Err(ReplaceError::Size);
    }
    let index = loaded
        .credentials
        .iter()
        .position(|c| &c.profile == profile && c.revision == revision)
        .ok_or_else(|| ReplaceError::UnknownCredential(name.clone()))?;
    let (Some(configured), Some(Some(base))) = (
        loaded.config.credentials.get(index),
        loaded.fingerprints.get(index),
    ) else {
        return Err(ReplaceError::UnknownCredential(name));
    };
    if configured.kind != aq_selection::CredentialKind::SubscriptionLogin
        || !loaded.credentials[index].enabled
    {
        return Err(ReplaceError::UnknownCredential(name));
    }
    let dir = loaded
        .config
        .material_store
        .as_ref()
        .ok_or(ReplaceError::NoStore)?;
    let internal = |_| ReplaceError::Internal;
    let mut tx = pool.begin().await.map_err(|_| ReplaceError::Internal)?;
    let lineage = lineage_locked(&mut tx, &name, revision, base)
        .await
        .map_err(internal)?;
    let (version, _) = append_locked(
        &mut tx,
        dir,
        &lineage,
        Origin::Operator,
        None,
        bytes,
        now_ms,
    )
    .await
    .map_err(internal)?;
    tx.commit().await.map_err(|_| ReplaceError::Internal)?;
    Ok(version)
}
