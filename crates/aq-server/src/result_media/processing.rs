use super::generation::{Generation, JobError};
use super::{Error, OperationLock, Owner, Result, User, db, files, media, now, sql};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

#[derive(Clone, Copy)]
enum Job {
    Receive,
    Finalize,
}

struct JobLease {
    operation: Uuid,
    id: Uuid,
}

impl Job {
    fn can_start(self, intent: &db::Intent) -> bool {
        use media::UploadState as State;
        match self {
            Self::Receive => intent.state == State::Reserved,
            Self::Finalize => {
                matches!(intent.state, State::Received | State::Finalizing) && intent.attempts < 8
            }
        }
    }

    fn claim_query(self) -> &'static str {
        match self {
            Self::Receive => {
                "UPDATE result_media_intents SET lease_id=$2,lease_expires_at_ms=$3,transition_at_ms=$4 WHERE operation_id=$1"
            }
            Self::Finalize => {
                "UPDATE result_media_intents SET state='finalizing',lease_id=$2,lease_expires_at_ms=$3,processing_attempts=processing_attempts+1,transition_at_ms=$4 WHERE operation_id=$1"
            }
        }
    }
}

impl Owner {
    async fn job(
        &self,
        actor: &User,
        id: Uuid,
        job: Job,
    ) -> Result<(db::Intent, JobLease, files::ActivityGuard)> {
        let guard = self.root.active(id, true)?;
        let mut tx = db::transaction(&self.state).await?;
        let row = self
            .lock_operation(&mut tx, actor, OperationLock::Change(id))
            .await?;
        let at = now(&self.state)?;
        row.live(at)?;
        if row.lease_expires.is_some_and(|end| at < end) {
            return Err(Error::Busy);
        }
        if !job.can_start(&row) {
            return Err(Error::Conflict);
        }
        let lease = JobLease {
            operation: id,
            id: Uuid::new_v4(),
        };
        let deadline = row
            .upload
            .min(at.checked_add(30_000).ok_or(Error::Invalid)?);
        sqlx::query(job.claim_query())
            .bind(id)
            .bind(lease.id)
            .bind(deadline as i64)
            .bind(at as i64)
            .execute(&mut *tx)
            .await
            .map_err(sql)?;
        tx.commit().await.map_err(sql)?;
        Ok((row, lease, guard))
    }
    async fn lock_current_job(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        actor: &User,
        lease: &JobLease,
    ) -> Result<(db::Intent, u64)> {
        let current = self
            .lock_operation(tx, actor, OperationLock::Change(lease.operation))
            .await?;
        let at = now(&self.state)?;
        current.live(at)?;
        if current.lease != Some(lease.id) || current.lease_expires.is_none_or(|v| at >= v) {
            return Err(Error::Conflict);
        }
        Ok((current, at))
    }

    /// Callers pass bounded bytes already associated with the immutable intent;
    /// the owner checks both exact length and digest before disk work.
    pub async fn put(&self, actor: &User, id: Uuid, bytes: Vec<u8>) -> Result<media::UploadStatus> {
        if bytes.len() > media::MAX_INPUT_BYTES.max(media::MAX_CLIP_BYTES) {
            return Err(Error::Invalid);
        }
        let status = self.status(actor, id).await?;
        if bytes.len() as u64 > status.media.max_input_bytes() {
            return Err(Error::Invalid);
        }
        verify_input(&bytes, status.input.bytes, &status.input.digest)?;
        if self.replayed_receive(&status)? {
            return Ok(status);
        }
        let (row, lease, guard) = self.job(actor, id, Job::Receive).await?;
        verify_input(&bytes, row.request.input_bytes, &row.request.input_digest)?;
        let root = self.root.clone();
        let digest = row.request.input_digest.clone();
        let kind = row.request.media;
        let (result, guard) = tokio::task::spawn_blocking(move || {
            // After a failed/canceled receive, replacement first proves removal
            // of the same uncommitted generation under its physical guard.
            let result = root
                .remove(id, false)
                .and_then(|()| root.receive(id, kind, &bytes, &digest))
                .and_then(|()| root.point("received_before_commit"));
            (result, guard)
        })
        .await
        .map_err(|_| Error::Storage)?;
        result?;
        let mut tx = db::transaction(&self.state).await?;
        let (_, at) = self.lock_current_job(&mut tx, actor, &lease).await?;
        sqlx::query("UPDATE result_media_intents SET state='received',received_at_ms=$2,lease_id=NULL,lease_expires_at_ms=NULL,transition_at_ms=$2 WHERE operation_id=$1 AND lease_id=$3")
            .bind(id).bind(at as i64).bind(lease.id).execute(&mut *tx).await.map_err(sql)?;
        self.root.check()?;
        tx.commit().await.map_err(sql)?;
        drop(guard);
        self.status(actor, id).await
    }
    fn replayed_receive(&self, status: &media::UploadStatus) -> Result<bool> {
        use media::UploadState as State;
        match status.state {
            // Exact lost-response replay is a read. It does not renew a lease
            // or rewrite even an expired or cleaned retained generation.
            State::Retained => Ok(true),
            State::Received if now(&self.state)? >= status.upload_expires_at_ms => {
                Err(Error::Expired)
            }
            State::Received => Ok(true),
            _ => Ok(false),
        }
    }

    pub async fn finalize(&self, actor: &User, id: Uuid) -> Result<media::UploadStatus> {
        // A response-loss retry of a committed receipt is a read, never a new
        // physical write or a new artifact identity.
        let status = self.status(actor, id).await?;
        if status.state == media::UploadState::Retained {
            return Ok(status);
        }
        let (row, lease, guard) = self.job(actor, id, Job::Finalize).await?;
        let kind = row.request.media;
        // A clip's preview is a verified copy of a retained image's preview.
        let poster = match (kind, row.request.poster_artifact_id) {
            (media::MediaKind::Video, Some(poster)) => self.poster(&row, poster.as_uuid()).await?,
            _ => None,
        };
        let poster_guard = match &poster {
            Some((poster_id, _)) => Some(self.root.active(*poster_id, false)?),
            None => None,
        };
        let root = self.root.clone();
        let generation = Generation {
            id,
            kind,
            input: row.request.input_digest.clone(),
            size: row.request.input_bytes as usize,
            limits: row.limits.clone(),
        };
        let (prepared, guard) = tokio::task::spawn_blocking(move || {
            let result = generation.finalize(&root, poster);
            drop(poster_guard);
            (result, guard)
        })
        .await
        .map_err(|_| Error::Storage)?;
        let mut tx = db::transaction(&self.state).await?;
        let (current, at) = self.lock_current_job(&mut tx, actor, &lease).await?;
        match prepared {
            Err(JobError::Storage(error)) => return Err(error),
            Err(JobError::Reject(rejection)) => {
                sqlx::query("UPDATE result_media_intents SET state='rejected',rejection=$2,lease_id=NULL,lease_expires_at_ms=NULL,transition_at_ms=$3 WHERE operation_id=$1 AND lease_id=$4")
                    .bind(id).bind(super::json(&rejection)?.as_str().ok_or(Error::Owner)?).bind(at as i64).bind(lease.id).execute(&mut *tx).await.map_err(sql)?;
            }
            Ok((prepared, charge)) => {
                let bytes = prepared
                    .receipt_bytes(media::ReceiptSource {
                        artifact: id,
                        uploader: current.actor,
                        binding: &current.binding,
                        request: &current.request,
                        kind,
                        retained_at_ms: at,
                        retain_until_ms: current.retain,
                    })
                    .map_err(|_| Error::Owner)?;
                sqlx::query("UPDATE result_media_intents SET state='retained',receipt_bytes=$2,receipt_digest=$3,reserved_bytes=0,retained_bytes=$4,lease_id=NULL,lease_expires_at_ms=NULL,transition_at_ms=$5 WHERE operation_id=$1 AND lease_id=$6")
                    .bind(id).bind(&bytes).bind(media::Digest::of(&bytes).as_str()).bind(charge as i64).bind(at as i64).bind(lease.id).execute(&mut *tx).await.map_err(sql)?;
            }
        }
        self.root.check()?;
        db::principal(&mut tx, actor).await?;
        tx.commit().await.map_err(sql)?;
        drop(guard);
        self.status(actor, id).await
    }
}

fn verify_input(bytes: &[u8], expected_bytes: u64, expected_digest: &media::Digest) -> Result<()> {
    if bytes.len() != expected_bytes as usize || &media::Digest::of(bytes) != expected_digest {
        return Err(Error::Conflict);
    }
    Ok(())
}
impl Owner {
    /// The poster must be a live retained image on the same revision and store.
    async fn poster(
        &self,
        row: &db::Intent,
        poster: Uuid,
    ) -> Result<Option<(Uuid, media::Variant)>> {
        let mut tx = db::transaction(&self.state).await?;
        let Some(found) = db::load_optional(&mut tx, poster).await? else {
            return Ok(None);
        };
        tx.commit().await.map_err(sql)?;
        let at = now(&self.state)?;
        let same_owner = found.binding == row.binding && found.storage == self.root.id;
        let available = !found.withheld && found.cleanup == "none";
        let receipt = found.receipt.filter(|receipt| {
            let image = receipt.clip.is_none()
                && receipt.rendering_policy == media::RenderingPolicy::Rgba8SrgbV1;
            image && at < receipt.retain_until_ms
        });
        Ok(receipt
            .filter(|_| same_owner && available)
            .map(|receipt| (poster, receipt.variants.preview)))
    }
}
