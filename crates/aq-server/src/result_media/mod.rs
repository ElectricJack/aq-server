//! Managed supplemental-media owner and explicitly mounted human adapter.
//! No default mount, AppState owner cycle, worker grant or execution authority.
pub mod api;
mod body;
mod cleanup;
pub mod config;
mod db;
mod files;
mod processing;
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
    /// Exact original bytes are retained before any physical write. Reusing an
    /// operation with a different actor, target, revision or byte spelling fails.
    pub async fn reserve(
        &self,
        actor: &User,
        report: Uuid,
        revision: &str,
        raw: &[u8],
    ) -> Result<media::UploadStatus> {
        Ok(self.admit(actor, report, revision, raw).await?.status)
    }
    pub async fn admit(
        &self,
        actor: &User,
        report: Uuid,
        revision: &str,
        raw: &[u8],
    ) -> Result<Admission> {
        let request = media::UploadCreate::decode(raw).map_err(|_| Error::Invalid)?;
        request
            .validate_with_limits(&self.limits)
            .map_err(|_| Error::Invalid)?;
        let revision = media::ReportRevision::parse(revision).map_err(|_| Error::Invalid)?;
        self.root.check()?;
        let mut tx = db::transaction(&self.state).await?;
        // Serialize all admissions before result locks; trigger accounting uses
        // exactly this order too, never a result->budget inversion.
        db::budgets(&mut tx, None).await?;
        db::principal(&mut tx, actor).await?;
        let binding = db::binding(&self.state, &mut tx, report, &revision).await?;
        db::budgets(&mut tx, Some(binding.project_id.as_uuid())).await?;
        let id = request.operation_id.as_uuid();
        if let Some(old) = db::load_optional(&mut tx, id).await? {
            old.original(actor)?;
            if old.raw != raw || old.binding != binding {
                return Err(Error::Conflict);
            }
            tx.commit().await.map_err(sql)?;
            return Ok(Admission {
                created: false,
                status: self.status(actor, id).await?,
            });
        }
        let at = now(&self.state)?;
        let upload = at
            .checked_add(self.limits.upload_ttl_ms)
            .filter(|v| *v <= media::MAX_SAFE_MILLIS)
            .ok_or(Error::Invalid)?;
        let retain = at
            .checked_add(self.limits.retention_ms)
            .filter(|v| *v <= media::MAX_SAFE_MILLIS)
            .ok_or(Error::Invalid)?;
        let count:i64=sqlx::query_scalar("SELECT count(*) FROM result_media_intents WHERE report_id=$1 AND report_revision=$2 AND state IN ('reserved','received','finalizing','retained')")
            .bind(report).bind(revision.as_str()).fetch_one(&mut *tx).await.map_err(sql)?;
        if count >= i64::from(self.limits.max_result_artifacts) {
            return Err(Error::Capacity);
        }
        db::capacity(&mut tx, binding.project_id.as_uuid()).await?;
        sqlx::query("INSERT INTO result_media_intents(operation_id,actor_id,actor_identity,report_id,report_revision,project_id,report_binding,storage_id,rendering_policy,admission_limits,request_bytes,request_digest,input_digest,input_bytes,filename,capture,state,created_at_ms,upload_expires_at_ms,retain_until_ms,transition_at_ms,reserved_bytes,file_generation) VALUES($1,$2,$3,$4,$5,$6,$7,$8,'aq.png.rgba8-srgb-v1',$9,$10,$11,$12,$13,$14,$15,'reserved',$16,$17,$18,$16,10485760,$1)")
            .bind(id).bind(actor.id).bind(db::key(actor)?).bind(report).bind(revision.as_str()).bind(binding.project_id.as_uuid()).bind(json(&binding)?).bind(self.root.id).bind(json(&self.limits)?).bind(raw).bind(media::Digest::of(raw).as_str()).bind(request.input_digest.as_str()).bind(request.input_bytes as i64).bind(&request.filename).bind(json(&request.capture)?).bind(at as i64).bind(upload as i64).bind(retain as i64).execute(&mut *tx).await.map_err(sql)?;
        self.root.check()?;
        db::principal(&mut tx, actor).await?;
        tx.commit().await.map_err(sql)?;
        Ok(Admission {
            created: true,
            status: self.status(actor, id).await?,
        })
    }
    pub async fn status(&self, actor: &User, id: Uuid) -> Result<media::UploadStatus> {
        let mut tx = db::transaction(&self.state).await?;
        db::principal(&mut tx, actor).await?;
        let row = db::load(&mut tx, id).await?;
        self.fence(&mut tx, actor, &row).await?;
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
        db::principal(&mut tx, actor).await?;
        let current = db::load(&mut tx, id).await?;
        self.fence(&mut tx, actor, &current).await?;
        if current.receipt != row.receipt
            || current.state != row.state
            || current.withheld != withheld
        {
            return Err(Error::Conflict);
        }
        status.validate().map_err(|_| Error::Owner)?;
        tx.commit().await.map_err(sql)?;
        self.root.check()?;
        Ok(status)
    }
    async fn fence(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        actor: &User,
        row: &db::Intent,
    ) -> Result<()> {
        db::principal(tx, actor).await?;
        row.original(actor)?;
        if row.storage != self.root.id {
            return Err(Error::Storage);
        }
        if db::binding(
            &self.state,
            tx,
            row.binding.report_id.as_uuid(),
            &row.binding.report_revision,
        )
        .await?
            != row.binding
        {
            return Err(Error::Owner);
        }
        Ok(())
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
