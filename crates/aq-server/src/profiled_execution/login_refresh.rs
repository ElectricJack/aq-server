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
pub mod store;

use crate::{error::AppError, intelligence::config::LoadedRegistry};
use aq_admission::MaterialLease;
use aq_selection::ArtifactRef;
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

/// Lineage identity of a credential profile.
pub fn identity(profile: &ArtifactRef) -> String {
    format!("{}@{}", profile.id.as_str(), profile.version.as_str())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Refresh,
    Operator,
}
impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::Refresh => "refresh",
            Origin::Operator => "operator",
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The version this entry holds (the configured file is version 0).
    pub sequence: i64,
    pub origin: Origin,
    pub lease_id: Option<Uuid>,
    pub content_digest: String,
    pub content_bytes: i64,
    pub revision_id: Uuid,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lineage {
    pub identity: String,
    pub revision: u64,
    pub base: String,
    pub entries: Vec<Entry>,
}

use crate::error::db;

/// Serializes writers of one lineage for the rest of `tx`, then reads it.
pub async fn lineage_locked(
    tx: &mut Transaction<'_, Postgres>,
    identity: &str,
    revision: u64,
    base: &str,
) -> Result<Lineage, AppError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('aq.lineage:'||$1||'#'||$2::text||'#'||$3,0))")
        .bind(identity)
        .bind(revision as i64)
        .bind(base)
        .execute(&mut **tx)
        .await
        .map_err(db)?;
    read(&mut **tx, identity, revision, base).await
}

/// Unlocked snapshot (issuance looks up the head before its blocking read and
/// re-fences under lock afterward).
pub async fn read(
    executor: impl sqlx::PgExecutor<'_>,
    identity: &str,
    revision: u64,
    base: &str,
) -> Result<Lineage, AppError> {
    let rows = sqlx::query(
        "SELECT revision_id,sequence,origin,lease_id,content_digest,content_bytes FROM credential_material_revisions WHERE credential_identity=$1 AND credential_revision=$2 AND base_digest=$3 ORDER BY sequence",
    )
    .bind(identity)
    .bind(revision as i64)
    .bind(base)
    .fetch_all(executor)
    .await
    .map_err(db)?;
    let entries = rows
        .into_iter()
        .map(|r| {
            Ok(Entry {
                sequence: r.get("sequence"),
                origin: match r.get::<String, _>("origin").as_str() {
                    "refresh" => Origin::Refresh,
                    "operator" => Origin::Operator,
                    _ => return Err(AppError::Internal),
                },
                lease_id: r.get("lease_id"),
                content_digest: r.get("content_digest"),
                content_bytes: r.get("content_bytes"),
                revision_id: r.get("revision_id"),
            })
        })
        .collect::<Result<_, _>>()?;
    Ok(Lineage {
        identity: identity.into(),
        revision,
        base: base.into(),
        entries,
    })
}

pub fn head(lineage: &Lineage) -> Option<&Entry> {
    lineage.entries.last()
}
/// Digest of the newest material in the lineage: the head entry, else base.
pub fn head_digest(lineage: &Lineage) -> &str {
    head(lineage).map_or(lineage.base.as_str(), |e| e.content_digest.as_str())
}

/// Current version: the head entry's, else 0 (the configured file).
pub fn head_version(lineage: &Lineage) -> i64 {
    head(lineage).map_or(0, |e| e.sequence)
}
/// Version holding `digest` (the newest such entry), 0 for base, else None.
pub fn version_of(lineage: &Lineage, digest: &str) -> Option<i64> {
    match lineage
        .entries
        .iter()
        .rev()
        .find(|e| e.content_digest == digest)
    {
        Some(e) => Some(e.sequence),
        None if lineage.base == digest => Some(0),
        None => None,
    }
}
/// Renewal rule (see module docs): the lease's version is in this lineage and
/// no operator upload is newer. Refreshes, from any lease, never refuse.
pub fn renewable(lineage: &Lineage, lease_digest: &str) -> bool {
    version_of(lineage, lease_digest).is_some_and(|v| {
        !lineage
            .entries
            .iter()
            .any(|e| e.origin == Origin::Operator && e.sequence > v)
    })
}

/// Appends one entry after `lineage`'s head (the caller holds the lineage
/// lock). The owner-only file is durable before the row; returns the sequence.
pub(crate) async fn append_locked(
    tx: &mut Transaction<'_, Postgres>,
    dir: &std::path::Path,
    lineage: &Lineage,
    origin: Origin,
    lease_id: Option<Uuid>,
    bytes: std::sync::Arc<zeroize::Zeroizing<Vec<u8>>>,
    now_ms: i64,
) -> Result<(i64, Uuid), AppError> {
    let revision_id = Uuid::new_v4();
    let sequence = head(lineage).map_or(1, |e| e.sequence + 1);
    let digest = aq_admission::Digest::of(&bytes);
    let len = bytes.len() as i64;
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || store::write(&dir, revision_id, &bytes))
        .await
        .map_err(|_| AppError::Internal)?
        .map_err(|_| AppError::Internal)?;
    sqlx::query("INSERT INTO credential_material_revisions(revision_id,credential_identity,credential_revision,base_digest,sequence,origin,lease_id,content_digest,content_bytes,created_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)")
        .bind(revision_id)
        .bind(&lineage.identity)
        .bind(lineage.revision as i64)
        .bind(&lineage.base)
        .bind(sequence)
        .bind(origin.as_str())
        .bind(lease_id)
        .bind(digest.as_str())
        .bind(len)
        .bind(now_ms)
        .execute(&mut **tx)
        .await
        .map_err(db)?;
    Ok((sequence, revision_id))
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
    fn of(entry: &Entry, dir: &std::path::Path) -> Self {
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
    profile: &ArtifactRef,
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

#[cfg(test)]
mod tests {
    use super::*;
    fn entry(sequence: i64, origin: Origin, lease: Option<Uuid>, digest: char) -> Entry {
        Entry {
            sequence,
            origin,
            lease_id: lease,
            content_digest: digest.to_string().repeat(64),
            content_bytes: 1,
            revision_id: Uuid::new_v4(),
        }
    }
    #[test]
    fn only_operator_upload_stops_renewal() {
        let (own, other) = (Uuid::new_v4(), Uuid::new_v4());
        let d = |c: char| c.to_string().repeat(64);
        let mut lineage = Lineage {
            identity: "cred@1".into(),
            revision: 1,
            base: d('a'),
            entries: vec![],
        };
        assert!(renewable(&lineage, &d('a')));
        assert!(!renewable(&lineage, &d('b')), "unknown digest");
        lineage
            .entries
            .push(entry(1, Origin::Refresh, Some(own), 'b'));
        lineage
            .entries
            .push(entry(2, Origin::Refresh, Some(other), 'c'));
        assert_eq!(head_version(&lineage), 2);
        assert_eq!(version_of(&lineage, &d('b')), Some(1));
        assert!(renewable(&lineage, &d('a')), "refreshed versions");
        assert!(renewable(&lineage, &d('b')));
        lineage.entries.push(entry(3, Origin::Operator, None, 'e'));
        assert!(!renewable(&lineage, &d('a')), "operator replace");
        assert!(!renewable(&lineage, &d('c')));
        assert!(renewable(&lineage, &d('e')), "issued from the upload");
    }
}
