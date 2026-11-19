//! The versioned subscription-login store (C5b), moved out of the aq-server lib:
//! lineages of stored login versions (digests and metadata in PostgreSQL, bytes
//! in owner-only files) and the checks the server makes of a Codex login.
//! Routes, leases and authority stay in aq-server
//! (`profiled_execution::login_refresh`, `manual_agents::session_login`).
pub mod codex;
pub mod store;

use aq_selection::ArtifactRef;
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

/// A database error, or the owner-only file store (or a stored row) failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error("login store failure")]
    Store,
}

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

/// Serializes writers of one lineage for the rest of `tx`, then reads it.
pub async fn lineage_locked(
    tx: &mut Transaction<'_, Postgres>,
    identity: &str,
    revision: u64,
    base: &str,
) -> Result<Lineage, Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('aq.lineage:'||$1||'#'||$2::text||'#'||$3,0))")
        .bind(identity)
        .bind(revision as i64)
        .bind(base)
        .execute(&mut **tx)
        .await
        ?;
    read(&mut **tx, identity, revision, base).await
}

/// Unlocked snapshot (issuance looks up the head before its blocking read and
/// re-fences under lock afterward).
pub async fn read(
    executor: impl sqlx::PgExecutor<'_>,
    identity: &str,
    revision: u64,
    base: &str,
) -> Result<Lineage, Error> {
    let rows = sqlx::query(
        "SELECT revision_id,sequence,origin,lease_id,content_digest,content_bytes FROM credential_material_revisions WHERE credential_identity=$1 AND credential_revision=$2 AND base_digest=$3 ORDER BY sequence",
    )
    .bind(identity)
    .bind(revision as i64)
    .bind(base)
    .fetch_all(executor)
    .await
    ?;
    let entries = rows
        .into_iter()
        .map(|r| {
            Ok(Entry {
                sequence: r.get("sequence"),
                origin: match r.get::<String, _>("origin").as_str() {
                    "refresh" => Origin::Refresh,
                    "operator" => Origin::Operator,
                    _ => return Err(Error::Store),
                },
                lease_id: r.get("lease_id"),
                content_digest: r.get("content_digest"),
                content_bytes: r.get("content_bytes"),
                revision_id: r.get("revision_id"),
            })
        })
        .collect::<Result<_, Error>>()?;
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
pub async fn append_locked(
    tx: &mut Transaction<'_, Postgres>,
    dir: &std::path::Path,
    lineage: &Lineage,
    origin: Origin,
    lease_id: Option<Uuid>,
    bytes: std::sync::Arc<zeroize::Zeroizing<Vec<u8>>>,
    now_ms: i64,
) -> Result<(i64, Uuid), Error> {
    let revision_id = Uuid::new_v4();
    let sequence = head(lineage).map_or(1, |e| e.sequence + 1);
    let digest = aq_admission::Digest::of(&bytes);
    let len = bytes.len() as i64;
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || store::write(&dir, revision_id, &bytes))
        .await
        .map_err(|_| Error::Store)?
        .map_err(|_| Error::Store)?;
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
        ?;
    Ok((sequence, revision_id))
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
