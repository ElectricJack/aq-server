use super::{Content, Error, Owner, Result, User, db, files, media, now, sql};
use media::Validate;
use uuid::Uuid;

impl Owner {
    async fn job(
        &self,
        actor: &User,
        id: Uuid,
        processing: bool,
    ) -> Result<(db::Intent, Uuid, files::ActivityGuard)> {
        let guard = self.root.active(id, true)?;
        let mut tx = db::transaction(&self.state).await?;
        db::budgets(&mut tx, None).await?;
        db::principal(&mut tx, actor).await?;
        let row = db::load(&mut tx, id).await?;
        db::budgets(&mut tx, Some(row.binding.project_id.as_uuid())).await?;
        self.fence(&mut tx, actor, &row).await?;
        let at = now(&self.state)?;
        row.live(at)?;
        use media::UploadState as S;
        if row.lease_expires.is_some_and(|end| at < end) {
            return Err(Error::Busy);
        }
        if processing && (!matches!(row.state, S::Received | S::Finalizing) || row.attempts >= 8)
            || !processing && row.state != S::Reserved
        {
            return Err(Error::Conflict);
        }
        let lease = Uuid::new_v4();
        let deadline = row
            .upload
            .min(at.checked_add(30_000).ok_or(Error::Invalid)?);
        sqlx::query(if processing{"UPDATE result_media_intents SET state='finalizing',lease_id=$2,lease_expires_at_ms=$3,processing_attempts=processing_attempts+1,transition_at_ms=$4 WHERE operation_id=$1"}else{"UPDATE result_media_intents SET lease_id=$2,lease_expires_at_ms=$3,transition_at_ms=$4 WHERE operation_id=$1"})
            .bind(id).bind(lease).bind(deadline as i64).bind(at as i64).execute(&mut *tx).await.map_err(sql)?;
        tx.commit().await.map_err(sql)?;
        Ok((row, lease, guard))
    }
    /// Callers pass bounded bytes already associated with the immutable intent;
    /// the owner checks both exact length and digest before disk work.
    pub async fn put(&self, actor: &User, id: Uuid, bytes: Vec<u8>) -> Result<media::UploadStatus> {
        if bytes.len() > media::MAX_INPUT_BYTES {
            return Err(Error::Invalid);
        }
        let status = self.status(actor, id).await?;
        if bytes.len() != status.input.bytes as usize
            || media::Digest::of(&bytes) != status.input.digest
        {
            return Err(Error::Conflict);
        }
        if matches!(
            status.state,
            media::UploadState::Received | media::UploadState::Retained
        ) {
            // Exact lost-response replay is a read. Never renew a lease or
            // rewrite even an expired/cleaned retained generation.
            if status.state == media::UploadState::Received
                && now(&self.state)? >= status.upload_expires_at_ms
            {
                return Err(Error::Expired);
            }
            return Ok(status);
        }
        let (row, lease, guard) = self.job(actor, id, false).await?;
        if bytes.len() != row.request.input_bytes as usize
            || media::Digest::of(&bytes) != row.request.input_digest
        {
            return Err(Error::Conflict);
        }
        let root = self.root.clone();
        let digest = row.request.input_digest.clone();
        let (result, guard) = tokio::task::spawn_blocking(move || {
            // After a failed/canceled receive, replacement first proves removal
            // of the same uncommitted generation under its physical guard.
            let result = root
                .remove(id, false)
                .and_then(|()| root.receive(id, &bytes, &digest))
                .and_then(|()| root.point("received_before_commit"));
            (result, guard)
        })
        .await
        .map_err(|_| Error::Storage)?;
        result?;
        let mut tx = db::transaction(&self.state).await?;
        db::budgets(&mut tx, None).await?;
        db::principal(&mut tx, actor).await?;
        let current = db::load(&mut tx, id).await?;
        db::budgets(&mut tx, Some(current.binding.project_id.as_uuid())).await?;
        self.fence(&mut tx, actor, &current).await?;
        let at = now(&self.state)?;
        current.live(at)?;
        if current.lease != Some(lease) || current.lease_expires.is_none_or(|v| at >= v) {
            return Err(Error::Conflict);
        }
        sqlx::query("UPDATE result_media_intents SET state='received',received_at_ms=$2,lease_id=NULL,lease_expires_at_ms=NULL,transition_at_ms=$2 WHERE operation_id=$1 AND lease_id=$3")
            .bind(id).bind(at as i64).bind(lease).execute(&mut *tx).await.map_err(sql)?;
        self.root.check()?;
        tx.commit().await.map_err(sql)?;
        drop(guard);
        self.status(actor, id).await
    }
    pub async fn finalize(&self, actor: &User, id: Uuid) -> Result<media::UploadStatus> {
        // A response-loss retry of a committed receipt is a read, never a new
        // physical write or a new artifact identity.
        let status = self.status(actor, id).await?;
        if status.state == media::UploadState::Retained {
            return Ok(status);
        }
        let (row, lease, guard) = self.job(actor, id, true).await?;
        let root = self.root.clone();
        let input = row.request.input_digest.clone();
        let size = row.request.input_bytes as usize;
        let limits = row.limits.clone();
        let (prepared, guard) = tokio::task::spawn_blocking(move || {
            let result = (|| {
                let published = root.generation(id, true).is_ok();
                let bytes = root.input(id, published, size, &input)?;
                let image = media::prepare_png(&bytes).map_err(|invalid| match invalid {
                    media::Invalid::UnsupportedPng => {
                        JobError::Reject(media::Rejection::UnsupportedPng)
                    }
                    media::Invalid::MalformedPng => JobError::Reject(media::Rejection::InvalidPng),
                    media::Invalid::Limit(_) => JobError::Reject(media::Rejection::ProcessingLimit),
                    _ => JobError::Reject(media::Rejection::InvalidPng),
                })?;
                let r = image.render().descriptor();
                let p = image.preview().descriptor();
                if r.bytes > limits.max_render_bytes
                    || p.bytes > limits.max_preview_bytes
                    || u64::from(r.width) * u64::from(r.height) > limits.max_pixels
                    || r.width > limits.max_dimension
                    || r.height > limits.max_dimension
                {
                    return Err(JobError::Reject(media::Rejection::ProcessingLimit));
                }
                if published {
                    root.variant(id, "render.png", r)?;
                    root.variant(id, "preview.png", p)?;
                    root.confirm_published(id)?;
                } else {
                    root.remove_variants(id)?;
                    root.variants(id, &image)?;
                    root.publish(id)?;
                }
                let charge = root.charge(id)?;
                if charge > 10_485_760 {
                    return Err(JobError::Reject(media::Rejection::StorageFailure));
                }
                root.point("before_receipt_commit")?;
                Ok((image, charge))
            })();
            (result, guard)
        })
        .await
        .map_err(|_| Error::Storage)?;
        let mut tx = db::transaction(&self.state).await?;
        db::budgets(&mut tx, None).await?;
        db::principal(&mut tx, actor).await?;
        let current = db::load(&mut tx, id).await?;
        db::budgets(&mut tx, Some(current.binding.project_id.as_uuid())).await?;
        self.fence(&mut tx, actor, &current).await?;
        let at = now(&self.state)?;
        current.live(at)?;
        if current.lease != Some(lease) || current.lease_expires.is_none_or(|v| at >= v) {
            return Err(Error::Conflict);
        }
        match prepared {
            Err(JobError::Storage(error)) => return Err(error),
            Err(JobError::Reject(rejection)) => {
                sqlx::query("UPDATE result_media_intents SET state='rejected',rejection=$2,lease_id=NULL,lease_expires_at_ms=NULL,transition_at_ms=$3 WHERE operation_id=$1 AND lease_id=$4")
                    .bind(id).bind(super::json(&rejection)?.as_str().ok_or(Error::Owner)?).bind(at as i64).bind(lease).execute(&mut *tx).await.map_err(sql)?;
            }
            Ok((image, charge)) => {
                let receipt = media::Receipt {
                    schema_version: 1,
                    artifact_id: media::Id::new(id).map_err(|_| Error::Owner)?,
                    binding: current.binding.clone(),
                    uploader_id: media::Id::new(current.actor).map_err(|_| Error::Owner)?,
                    input: media::Input {
                        digest: current.request.input_digest.clone(),
                        bytes: current.request.input_bytes,
                        filename: current.request.filename.clone(),
                    },
                    capture: current.request.capture.clone(),
                    rendering_policy: media::RenderingPolicy::Rgba8SrgbV1,
                    variants: media::Variants {
                        render: image.render().descriptor().clone(),
                        preview: image.preview().descriptor().clone(),
                    },
                    retained_at_ms: at,
                    retain_until_ms: current.retain,
                };
                receipt.validate().map_err(|_| Error::Owner)?;
                let bytes = media::canonical_bytes(&receipt).map_err(|_| Error::Owner)?;
                sqlx::query("UPDATE result_media_intents SET state='retained',receipt_bytes=$2,receipt_digest=$3,reserved_bytes=0,retained_bytes=$4,lease_id=NULL,lease_expires_at_ms=NULL,transition_at_ms=$5 WHERE operation_id=$1 AND lease_id=$6")
                    .bind(id).bind(&bytes).bind(media::Digest::of(&bytes).as_str()).bind(charge as i64).bind(at as i64).bind(lease).execute(&mut *tx).await.map_err(sql)?;
            }
        }
        self.root.check()?;
        db::principal(&mut tx, actor).await?;
        tx.commit().await.map_err(sql)?;
        drop(guard);
        self.status(actor, id).await
    }
    pub async fn list(
        &self,
        actor: &User,
        report: Uuid,
        revision: &str,
    ) -> Result<media::MediaPage> {
        let revision = media::ReportRevision::parse(revision).map_err(|_| Error::Invalid)?;
        let mut tx = db::transaction(&self.state).await?;
        db::principal(&mut tx, actor).await?;
        let binding = db::binding(&self.state, &mut tx, report, &revision).await?;
        let ids:Vec<Uuid>=sqlx::query_scalar("SELECT operation_id FROM result_media_intents WHERE report_id=$1 AND report_revision=$2 AND state='retained' ORDER BY operation_id LIMIT 9").bind(report).bind(revision.as_str()).fetch_all(&mut *tx).await.map_err(sql)?;
        if ids.len() > 8 {
            return Err(Error::Owner);
        }
        let mut snapshots = Vec::new();
        for id in ids {
            let row = db::load(&mut tx, id).await?;
            if row.binding != binding || row.storage != self.root.id {
                return Err(Error::Owner);
            }
            snapshots.push((row.receipt.ok_or(Error::Owner)?, row.withheld));
        }
        tx.commit().await.map_err(sql)?;
        let owner = self.clone();
        let items = tokio::task::spawn_blocking(move || {
            snapshots
                .into_iter()
                .map(|(receipt, withheld)| {
                    Ok(media::Artifact {
                        receipt_digest: receipt.digest().map_err(|_| Error::Owner)?,
                        availability: owner.observation(&receipt, withheld),
                        receipt,
                    })
                })
                .collect::<Result<Vec<_>>>()
        })
        .await
        .map_err(|_| Error::Storage)??;
        let mut tx = db::transaction(&self.state).await?;
        db::principal(&mut tx, actor).await?;
        if db::binding(&self.state, &mut tx, report, &revision).await? != binding {
            return Err(Error::Owner);
        }
        for item in &items {
            let current = db::load(&mut tx, item.receipt.artifact_id.as_uuid()).await?;
            if current.receipt.as_ref() != Some(&item.receipt)
                || current.withheld
                    != (item.availability.state == media::AvailabilityState::Withheld)
            {
                return Err(Error::Conflict);
            }
        }
        let page = media::MediaPage {
            contract: media::Contract::HumanResultMedia1,
            server_time_ms: now(&self.state)?,
            binding,
            limits: self.limits.clone(),
            items,
        };
        page.validate().map_err(|_| Error::Owner)?;
        tx.commit().await.map_err(sql)?;
        Ok(page)
    }
    pub async fn content(
        &self,
        actor: &User,
        report: Uuid,
        revision: &str,
        id: Uuid,
        preview: bool,
    ) -> Result<Content> {
        let revision = media::ReportRevision::parse(revision).map_err(|_| Error::Invalid)?;
        let mut tx = db::transaction(&self.state).await?;
        db::principal(&mut tx, actor).await?;
        let binding = db::binding(&self.state, &mut tx, report, &revision).await?;
        let row = db::load(&mut tx, id).await?;
        if row.binding != binding || row.storage != self.root.id {
            return Err(Error::NotFound);
        }
        if row.withheld {
            return Err(Error::Withheld);
        }
        let receipt = row.receipt.ok_or(Error::NotFound)?;
        if now(&self.state)? >= receipt.retain_until_ms {
            return Err(Error::Expired);
        }
        tx.commit().await.map_err(sql)?;
        let descriptor = if preview {
            receipt.variants.preview.clone()
        } else {
            receipt.variants.render.clone()
        };
        let root = self.root.clone();
        let expected = descriptor.clone();
        let (bytes, guard) = match self.root.active(id, false) {
            Ok(guard) => tokio::task::spawn_blocking(move || {
                (
                    root.variant(
                        id,
                        if preview { "preview.png" } else { "render.png" },
                        &expected,
                    ),
                    Some(guard),
                )
            })
            .await
            .unwrap_or_else(|_| (Err(Error::Storage), None)),
            Err(error) => (Err(error), None),
        };
        let mut tx = db::transaction(&self.state).await?;
        db::principal(&mut tx, actor).await?;
        if db::binding(&self.state, &mut tx, report, &revision).await? != binding {
            return Err(Error::Owner);
        }
        let current = db::load(&mut tx, id).await?;
        if current.receipt.as_ref() != Some(&receipt) || current.cleanup != "none" {
            return Err(Error::NotFound);
        }
        if current.withheld {
            return Err(Error::Withheld);
        }
        if now(&self.state)? >= receipt.retain_until_ms {
            return Err(Error::Expired);
        }
        self.root.check()?;
        tx.commit().await.map_err(sql)?;
        let bytes = bytes?;
        Ok(Content {
            bytes,
            _guard: guard.ok_or(Error::Owner)?,
            descriptor,
            receipt_digest: receipt.digest().map_err(|_| Error::Owner)?,
        })
    }
}
enum JobError {
    Storage(Error),
    Reject(media::Rejection),
}
impl From<Error> for JobError {
    fn from(value: Error) -> Self {
        Self::Storage(value)
    }
}
