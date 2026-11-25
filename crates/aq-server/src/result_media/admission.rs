//! Reserve one authenticated upload and its budget on the report revision.

use super::{Admission, Error, Owner, Result, User, db, json, media, now, sql};
use media::{Limits, ReportBinding, ReportRevision, UploadCreate};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

struct ReservationRequest<'a> {
    raw: &'a [u8],
    report: Uuid,
    revision: ReportRevision,
    input: UploadCreate,
}

impl<'a> ReservationRequest<'a> {
    fn decode(raw: &'a [u8], report: Uuid, revision: &str, limits: &Limits) -> Result<Self> {
        let input = UploadCreate::decode(raw).map_err(|_| Error::Invalid)?;
        input
            .validate_with_limits(limits)
            .map_err(|_| Error::Invalid)?;
        let revision = ReportRevision::parse(revision).map_err(|_| Error::Invalid)?;
        Ok(Self {
            raw,
            report,
            revision,
            input,
        })
    }
}

struct AdmissionDeadlines {
    upload: u64,
    retain: u64,
}

impl AdmissionDeadlines {
    fn at(at: u64, limits: &Limits) -> Result<Self> {
        let upload = at
            .checked_add(limits.upload_ttl_ms)
            .filter(|v| *v <= media::MAX_SAFE_MILLIS)
            .ok_or(Error::Invalid)?;
        let retain = at
            .checked_add(limits.retention_ms)
            .filter(|v| *v <= media::MAX_SAFE_MILLIS)
            .ok_or(Error::Invalid)?;
        Ok(Self { upload, retain })
    }
}

struct NewReservation<'a> {
    request: &'a ReservationRequest<'a>,
    binding: &'a ReportBinding,
    reserved_at: u64,
    deadlines: AdmissionDeadlines,
}

impl Owner {
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
        let request = ReservationRequest::decode(raw, report, revision, &self.limits)?;
        self.root.check()?;
        let mut tx = db::transaction(&self.state).await?;
        // Serialize all admissions before result locks; trigger accounting uses
        // exactly this order too, never a result->budget inversion.
        db::budgets(&mut tx, None).await?;
        db::principal(&mut tx, actor).await?;
        let binding = db::binding(&self.state, &mut tx, report, &request.revision).await?;
        db::budgets(&mut tx, Some(binding.project_id.as_uuid())).await?;
        let id = request.input.operation_id.as_uuid();
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
        let reserved_at = now(&self.state)?;
        let reservation = NewReservation {
            request: &request,
            binding: &binding,
            reserved_at,
            deadlines: AdmissionDeadlines::at(reserved_at, &self.limits)?,
        };
        self.new_reservation(&mut tx, actor, &reservation).await?;
        self.root.check()?;
        db::principal(&mut tx, actor).await?;
        tx.commit().await.map_err(sql)?;
        Ok(Admission {
            created: true,
            status: self.status(actor, id).await?,
        })
    }
    async fn new_reservation(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        actor: &User,
        reservation: &NewReservation<'_>,
    ) -> Result<()> {
        let NewReservation {
            request,
            binding,
            reserved_at,
            deadlines,
        } = reservation;
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM result_media_intents WHERE report_id=$1 AND report_revision=$2 AND state IN ('reserved','received','finalizing','retained')",
        )
        .bind(request.report)
        .bind(request.revision.as_str())
        .fetch_one(&mut **tx)
        .await
        .map_err(sql)?;
        if count >= i64::from(self.limits.max_result_artifacts) {
            return Err(Error::Capacity);
        }
        db::capacity(tx, binding.project_id.as_uuid()).await?;
        self.verify_poster(tx, reservation).await?;
        sqlx::query(
            "INSERT INTO result_media_intents(operation_id,actor_id,actor_identity,report_id,report_revision,project_id,report_binding,storage_id,rendering_policy,admission_limits,request_bytes,request_digest,input_digest,input_bytes,filename,capture,state,created_at_ms,upload_expires_at_ms,retain_until_ms,transition_at_ms,reserved_bytes,file_generation) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$19,$9,$10,$11,$12,$13,$14,$15,'reserved',$16,$17,$18,$16,10485760,$1)",
        )
        .bind(request.input.operation_id.as_uuid())
        .bind(actor.id)
        .bind(db::key(actor)?)
        .bind(request.report)
        .bind(request.revision.as_str())
        .bind(binding.project_id.as_uuid())
        .bind(json(*binding)?)
        .bind(self.root.id)
        .bind(json(&self.limits)?)
        .bind(request.raw)
        .bind(media::Digest::of(request.raw).as_str())
        .bind(request.input.input_digest.as_str())
        .bind(request.input.input_bytes as i64)
        .bind(&request.input.filename)
        .bind(json(&request.input.capture)?)
        .bind(*reserved_at as i64)
        .bind(deadlines.upload as i64)
        .bind(deadlines.retain as i64)
        .bind(request.input.media.policy().as_str())
        .execute(&mut **tx)
        .await
        .map_err(sql)?;
        Ok(())
    }

    async fn verify_poster(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        reservation: &NewReservation<'_>,
    ) -> Result<()> {
        let NewReservation {
            request,
            binding,
            reserved_at,
            ..
        } = reservation;
        if let Some(poster) = request.input.poster_artifact_id {
            // A clip names a live retained image on this same revision.
            let poster = db::load_optional(tx, poster.as_uuid())
                .await?
                .ok_or(Error::Invalid)?;
            let image = poster.receipt.as_ref().is_some_and(|r| {
                r.rendering_policy == media::RenderingPolicy::Rgba8SrgbV1
                    && *reserved_at < r.retain_until_ms
            });
            let same_owner = poster.binding == **binding && poster.storage == self.root.id;
            let available = !poster.withheld && poster.cleanup == "none";
            if !(image && same_owner && available) {
                return Err(Error::Invalid);
            }
        }
        Ok(())
    }
}
