//! Managed supplemental-media owner and explicitly mounted human adapter.
//! No default mount, AppState owner cycle, worker grant or execution authority.
mod admission;
pub mod api;
mod body;
mod cleanup;
pub use aq_result_media::config;
mod db;
mod files;
mod generation;
mod processing;
mod reads;
pub mod runtime;

use crate::{AppState, store::User};
use aq_result_media as media;
use media::Validate;
use std::{path::Path, sync::Arc};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid media operation")]
    Invalid,
    #[error("media operation identity conflicts")]
    Conflict,
    #[error("media requires the same current Owner principal")]
    Access,
    #[error("media operation not found")]
    NotFound,
    #[error("media owner busy; inspect the original operation")]
    Busy,
    #[error("media capacity exhausted")]
    Capacity,
    #[error("media storage unavailable or changed")]
    Storage,
    #[error("retained media integrity failed")]
    Integrity,
    #[error("retained media file missing")]
    Missing,
    #[error("retained media is withheld")]
    Withheld,
    #[error("media operation or retention expired")]
    Expired,
    #[error("media owner data unavailable or inconsistent")]
    Owner,
}
type Result<T> = std::result::Result<T, Error>;
fn sql(_: sqlx::Error) -> Error {
    Error::Owner
}
fn json<T: serde::Serialize>(value: &T) -> Result<serde_json::Value> {
    serde_json::to_value(value).map_err(|_| Error::Owner)
}
fn now(state: &AppState) -> Result<u64> {
    u64::try_from((state.clock)().timestamp_millis())
        .ok()
        .filter(|v| (1..=media::MAX_SAFE_MILLIS).contains(v))
        .ok_or(Error::Owner)
}
#[derive(Clone)]
pub struct Owner {
    state: AppState,
    root: Arc<files::Root>,
    limits: media::Limits,
}
pub struct Admission {
    pub created: bool,
    pub status: media::UploadStatus,
}

enum OperationLock {
    Read(Uuid),
    Change(Uuid),
}

#[cfg(any(test, feature = "test-support"))]
pub type TestHook = Arc<dyn Fn(&str) -> Result<()> + Send + Sync>;
impl Owner {
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_test_hook(&self, hook: Option<TestHook>) -> Result<()> {
        *self.root.hook.lock().map_err(|_| Error::Storage)? = hook;
        Ok(())
    }
    pub async fn open(state: AppState, path: &Path, storage_id: Uuid) -> Result<Self> {
        let path = path.to_owned();
        let root = tokio::task::spawn_blocking(move || files::Root::open(&path, storage_id))
            .await
            .map_err(|_| Error::Storage)??;
        let mut tx = db::transaction(&state).await?;
        sqlx::query("INSERT INTO result_media_storage(storage_id,root_fingerprint,created_at_ms) VALUES($1,$2,$3) ON CONFLICT DO NOTHING")
            .bind(root.id).bind(&root.fingerprint).bind(now(&state)? as i64).execute(&mut *tx).await.map_err(sql)?;
        let binding:(Uuid,String)=sqlx::query_as("SELECT storage_id,root_fingerprint FROM result_media_storage WHERE singleton FOR SHARE").fetch_one(&mut *tx).await.map_err(sql)?;
        if binding != (root.id, root.fingerprint.clone()) {
            return Err(Error::Storage);
        }
        tx.commit().await.map_err(sql)?;
        root.check()?;
        Ok(Self {
            state,
            root,
            limits: media::Limits::default(),
        })
    }
    pub async fn status(&self, actor: &User, id: Uuid) -> Result<media::UploadStatus> {
        let mut tx = db::transaction(&self.state).await?;
        let row = self
            .lock_operation(&mut tx, actor, OperationLock::Read(id))
            .await?;
        let mut status = row.status(now(&self.state)?)?;
        let withheld = row.withheld;
        tx.commit().await.map_err(sql)?;
        if let Some(artifact) = status.artifact.as_mut() {
            let owner = self.clone();
            let receipt = artifact.receipt.clone();
            artifact.availability =
                tokio::task::spawn_blocking(move || owner.observation(&receipt, withheld))
                    .await
                    .map_err(|_| Error::Storage)?;
        }
        let mut tx = db::transaction(&self.state).await?;
        let current = self
            .lock_operation(&mut tx, actor, OperationLock::Read(id))
            .await?;
        let observed = (&row.receipt, row.state, withheld);
        if (&current.receipt, current.state, current.withheld) != observed {
            return Err(Error::Conflict);
        }
        status.validate().map_err(|_| Error::Owner)?;
        tx.commit().await.map_err(sql)?;
        self.root.check()?;
        Ok(status)
    }
    async fn lock_operation(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        actor: &User,
        operation: OperationLock,
    ) -> Result<db::Intent> {
        let (id, write_budget) = match operation {
            OperationLock::Read(id) => (id, false),
            OperationLock::Change(id) => (id, true),
        };
        if write_budget {
            db::budgets(tx, None).await?;
        }
        db::principal(tx, actor).await?;
        let peek = db::peek(tx, id).await?;
        peek.original(actor)?;
        if write_budget {
            db::budgets(tx, Some(peek.binding.project_id.as_uuid())).await?;
        }
        // All human paths take the actual result/node authority before an
        // intent lock. The peek supplies only an immutable lookup key.
        let proof = db::binding(
            &self.state,
            tx,
            peek.binding.report_id.as_uuid(),
            &peek.binding.report_revision,
        )
        .await?;
        let row = db::load(tx, id).await?;
        if !peek.matches(&row) || proof != row.binding {
            return Err(Error::Owner);
        }
        db::principal(tx, actor).await?;
        row.original(actor)?;
        if row.storage != self.root.id {
            return Err(Error::Storage);
        }
        Ok(row)
    }
    fn observation(&self, receipt: &media::Receipt, withheld: bool) -> media::Availability {
        if withheld {
            return media::Availability {
                state: media::AvailabilityState::Withheld,
                observed_at_ms: now(&self.state).unwrap_or(media::MAX_SAFE_MILLIS),
                reason: Some(media::AvailabilityReason::OwnerWithheld),
            };
        }
        self.availability(receipt)
    }
    fn availability(&self, receipt: &media::Receipt) -> media::Availability {
        let at = now(&self.state).unwrap_or(media::MAX_SAFE_MILLIS);
        use media::{AvailabilityReason as R, AvailabilityState as S};
        let (state, reason) = if at >= receipt.retain_until_ms {
            (S::Expired, Some(R::RetentionExpired))
        } else if self.root.check().is_err() {
            (S::Failed, Some(R::StorageUnavailable))
        } else {
            let result = self
                .root
                .active(receipt.artifact_id.as_uuid(), false)
                .and_then(|_guard| {
                    self.root
                        .probe(receipt.artifact_id.as_uuid(), &receipt.variants)
                });
            match result {
                Ok(()) => (S::Available, None),
                Err(Error::Integrity) => (S::Failed, Some(R::IntegrityFailed)),
                Err(Error::Missing) => (S::Missing, Some(R::FilesMissing)),
                Err(_) => (S::Failed, Some(R::StorageUnavailable)),
            }
        };
        media::Availability {
            state,
            reason,
            observed_at_ms: at,
        }
    }
}

/// Bytes retain their physical reader guard until the eventual body/consumer is
/// dropped. Cleanup cannot release a charge while this object remains live.
pub struct Content {
    bytes: Vec<u8>,
    _guard: files::ActivityGuard,
    pub descriptor: media::Variant,
    pub receipt_digest: media::Digest,
}
impl Content {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}
