//! The versioned subscription-login store (C5b), moved out of the aq-server lib:
//! lineages of stored login versions (digests and metadata in PostgreSQL, bytes
//! in owner-only files) and the checks the server makes of a Codex login.
//! Routes, leases and authority stay in aq-server
//! (`profiled_execution::login_refresh`, `manual_agents::session_login`).
pub mod codex;
pub mod import;
pub mod operator;
pub mod refresh;
pub mod sources;
pub mod store;

use aq_intelligence_config::{
    LoadedRegistry,
    material::{MaterialError, ProtectedMaterial, read_configured},
};
use aq_selection::{ArtifactRef, AuthorizedCredential};
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

pub struct NewRevision {
    pub origin: Origin,
    pub lease_id: Option<Uuid>,
    pub bytes: std::sync::Arc<zeroize::Zeroizing<Vec<u8>>>,
    pub now_ms: i64,
}

/// Appends one entry after `lineage`'s head (the caller holds the lineage
/// lock). The owner-only file is durable before the row; returns the sequence.
pub async fn append_locked(
    tx: &mut Transaction<'_, Postgres>,
    dir: &std::path::Path,
    lineage: &Lineage,
    revision: NewRevision,
) -> Result<(i64, Uuid), Error> {
    let NewRevision {
        origin,
        lease_id,
        bytes,
        now_ms,
    } = revision;
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
impl Lineage {
    pub fn head_source(&self, dir: &std::path::Path) -> Option<HeadSource> {
        head(self).map(|entry| HeadSource::of(entry, dir))
    }
}
impl HeadSource {
    pub fn of(entry: &Entry, dir: &std::path::Path) -> Self {
        Self {
            revision_id: entry.revision_id,
            digest: entry.content_digest.clone(),
            bytes: entry.content_bytes as u64,
            dir: dir.to_path_buf(),
        }
    }
}

/// The lineage revision `source`, else the configured file (version 0). The
/// configured file must match its startup fingerprint either way.
pub fn source_or_configured(
    registry: &LoadedRegistry,
    index: usize,
    source: Option<&HeadSource>,
) -> Result<ProtectedMaterial, MaterialError> {
    let configured = read_configured(registry, index)?;
    match source {
        None => Ok(configured),
        Some(s) => {
            drop(configured);
            store::read(&s.dir, s.revision_id, &s.digest, s.bytes)
        }
    }
}

/// Why a stored login of `provider` is not leased now: the owner's Codex login
/// age limit (Settings › Providers), with a reason the node shows. None when it
/// may be leased. An attempt's material fetch and a session's lease both ask.
pub async fn lease_refusal(
    conn: &mut sqlx::PgConnection,
    provider: &str,
    login: &[u8],
    now_ms: i64,
) -> Result<Option<String>, Error> {
    if provider != codex::PROVIDER {
        return Ok(None);
    }
    let limit = aq_swarm_settings::providers::codex_login_max_age_days(conn).await?;
    Ok(codex::stale(login, limit, now_ms))
}

/// Server-side rule for a node write-back of a Codex login, on the attempt and
/// the session route alike: `new` must be the lineage's newest login refreshed
/// ([`codex::refreshed`]). Other providers' logins are not inspected. The
/// caller holds the lineage lock.
pub async fn refresh_of_head(
    registry: std::sync::Arc<LoadedRegistry>,
    index: usize,
    source: Option<HeadSource>,
    new: std::sync::Arc<zeroize::Zeroizing<Vec<u8>>>,
) -> Result<bool, Error> {
    if registry.credentials[index].provider != codex::PROVIDER {
        return Ok(true);
    }
    tokio::task::spawn_blocking(move || {
        source_or_configured(&registry, index, source.as_ref())
            .is_ok_and(|stored| codex::refreshed(stored.bytes(), &new))
    })
    .await
    .map_err(|_| Error::Store)
}

/// The configured Codex subscription logins whose newest stored version is
/// older than the owner's limit: (credential index, the reason a lease of it
/// is refused). Empty without a limit, before any file is read.
pub async fn stale_logins(
    conn: &mut sqlx::PgConnection,
    loaded: &LoadedRegistry,
    now_ms: i64,
) -> Result<Vec<(usize, String)>, Error> {
    let Some(limit_days) =
        aq_swarm_settings::providers::codex_login_max_age_days(&mut *conn).await?
    else {
        return Ok(Vec::new());
    };
    let check = LoginAge {
        loaded,
        limit_days,
        now_ms,
    };
    let mut stale = Vec::new();
    for index in 0..loaded.credentials.len() {
        if let Some(reason) = check.refusal(conn, index).await? {
            stale.push((index, reason));
        }
    }
    Ok(stale)
}

struct LoginAge<'a> {
    loaded: &'a LoadedRegistry,
    limit_days: i64,
    now_ms: i64,
}
impl LoginAge<'_> {
    async fn refusal(
        &self,
        conn: &mut sqlx::PgConnection,
        index: usize,
    ) -> Result<Option<String>, Error> {
        let loaded = self.loaded;
        let credential = &loaded.credentials[index];
        let (Some(configured), Some(Some(base))) = (
            loaded.config.credentials.get(index),
            loaded.fingerprints.get(index),
        ) else {
            return Ok(None);
        };
        if credential.provider != codex::PROVIDER
            || configured.kind != aq_selection::CredentialKind::SubscriptionLogin
        {
            return Ok(None);
        }
        let name = identity(&credential.profile);
        let lineage = read(&mut *conn, &name, credential.revision, base).await?;
        let source = loaded
            .config
            .material_store
            .as_ref()
            .and_then(|dir| lineage.head_source(dir));
        // One small owner-only file, read in place: selection asks rarely.
        let login = source_or_configured(loaded, index, source.as_ref()).ok();
        Ok(login.and_then(|login| codex::stale(login.bytes(), Some(self.limit_days), self.now_ms)))
    }
}

pub enum LoginSelection<'a> {
    At(i64),
    Frozen(&'a [AuthorizedCredential]),
}
impl<'a> LoginSelection<'a> {
    pub fn at(now_ms: i64, frozen: Option<&'a [AuthorizedCredential]>) -> Self {
        frozen.map_or(Self::At(now_ms), Self::Frozen)
    }
}

/// A stale Codex login ([`stale_logins`]) cannot be provisioned, so selection
/// sees its credential as missing and falls back instead of placing an attempt
/// whose lease would be refused. An attempt that already owns its selection
/// keeps what was `frozen` with it, as provider membership does.
pub async fn mask_stale(
    conn: &mut sqlx::PgConnection,
    loaded: &LoadedRegistry,
    credentials: &mut [AuthorizedCredential],
    selection: LoginSelection<'_>,
) -> Result<(), Error> {
    let now_ms = match selection {
        LoginSelection::At(now_ms) => now_ms,
        LoginSelection::Frozen(frozen) => {
            mask_frozen(credentials, frozen);
            return Ok(());
        }
    };
    for (index, _) in stale_logins(conn, loaded, now_ms).await? {
        let stale = &loaded.credentials[index];
        for credential in credentials.iter_mut() {
            if credential.profile == stale.profile && credential.revision == stale.revision {
                credential.provisionable = false;
            }
        }
    }
    Ok(())
}

fn mask_frozen(credentials: &mut [AuthorizedCredential], frozen: &[AuthorizedCredential]) {
    for credential in credentials {
        let old = frozen.iter().find(|old| old.profile == credential.profile);
        credential.provisionable &= old.is_some_and(|old| old.provisionable);
    }
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
