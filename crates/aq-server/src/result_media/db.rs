use super::{AppState, Error, Result, User, json, media, sql};
use media::Validate;
use serde_json::Value;
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;
pub(super) async fn transaction(state: &AppState) -> Result<Transaction<'_, Postgres>> {
    let mut tx = state.store.pool.begin().await.map_err(sql)?;
    sqlx::query("SET LOCAL lock_timeout='1s'")
        .execute(&mut *tx)
        .await
        .map_err(sql)?;
    sqlx::query("SET LOCAL statement_timeout='3s'")
        .execute(&mut *tx)
        .await
        .map_err(sql)?;
    Ok(tx)
}
pub(super) fn key(user: &User) -> Result<Value> {
    if user.role != crate::store::Role::Owner
        || user.issuer.as_ref().is_none_or(|s| s.is_empty())
        || user.subject.as_ref().is_none_or(|s| s.is_empty())
    {
        return Err(Error::Access);
    }
    Ok(serde_json::json!({"issuer":user.issuer,"subject":user.subject}))
}
pub(super) async fn principal(tx: &mut Transaction<'_, Postgres>, user: &User) -> Result<()> {
    let expected = key(user)?;
    let current:Option<(String,Value)>=sqlx::query_as("SELECT role,jsonb_build_object('issuer',issuer,'subject',subject) FROM users WHERE id=$1 FOR SHARE").bind(user.id).fetch_optional(&mut **tx).await.map_err(sql)?;
    if current != Some(("owner".into(), expected)) {
        return Err(Error::Access);
    }
    Ok(())
}
pub(super) async fn budgets(
    tx: &mut Transaction<'_, Postgres>,
    project: Option<Uuid>,
) -> Result<()> {
    sqlx::query(
        "SELECT scope_key FROM result_media_budgets WHERE scope_key='installation' FOR UPDATE",
    )
    .fetch_one(&mut **tx)
    .await
    .map_err(sql)?;
    if let Some(project) = project {
        let scope = format!("project/{project}");
        sqlx::query("INSERT INTO result_media_budgets(scope_key,limit_bytes) VALUES($1,67108864) ON CONFLICT DO NOTHING").bind(&scope).execute(&mut **tx).await.map_err(sql)?;
        sqlx::query("SELECT scope_key FROM result_media_budgets WHERE scope_key=$1 FOR UPDATE")
            .bind(scope)
            .fetch_one(&mut **tx)
            .await
            .map_err(sql)?;
    }
    Ok(())
}
pub(super) async fn capacity(tx: &mut Transaction<'_, Postgres>, project: Uuid) -> Result<()> {
    let count:i64=sqlx::query_scalar("SELECT count(*) FROM result_media_budgets WHERE scope_key IN ('installation',$1) AND reserved_bytes+retained_bytes+10485760<=limit_bytes AND historical_intents<CASE WHEN scope_key='installation' THEN 4096 ELSE 512 END").bind(format!("project/{project}")).fetch_one(&mut **tx).await.map_err(sql)?;
    if count != 2 {
        return Err(Error::Capacity);
    }
    Ok(())
}
pub(super) async fn binding(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    report: Uuid,
    revision: &media::ReportRevision,
) -> Result<media::ReportBinding> {
    let result = crate::profiled_execution::results::load_locked(state, tx, report)
        .await
        .map_err(|error| match error {
            crate::error::AppError::NotFound(_) => Error::NotFound,
            _ => Error::Owner,
        })?;
    if result.receipt.report_digest.as_str() != revision.as_str() {
        return Err(Error::NotFound);
    }
    let id = |value| media::Id::new(value).map_err(|_| Error::Owner);
    let base = serde_json::from_value(json(&result.spec.source.resolved_object)?)
        .map_err(|_| Error::Owner)?;
    let binding = media::ReportBinding {
        coverage: media::ReportCoverage::RecordedLegacyNativeOnly,
        report_id: id(report)?,
        report_revision: revision.clone(),
        project_id: id(result.spec.lifecycle.project_id)?,
        task_id: id(result.spec.task.task_id)?,
        attempt_id: id(result.report.binding.attempt_id)?,
        source: media::SourceBinding {
            repository_id: id(result.spec.source.repository_id)?,
            base,
        },
    };
    binding.validate().map_err(|_| Error::Owner)?;
    Ok(binding)
}
pub(super) struct Intent {
    pub id: Uuid,
    pub actor: Uuid,
    pub identity: Value,
    pub binding: media::ReportBinding,
    pub storage: Uuid,
    pub raw: Vec<u8>,
    pub request: media::UploadCreate,
    pub state: media::UploadState,
    pub created: u64,
    pub upload: u64,
    pub retain: u64,
    pub transition: u64,
    pub lease: Option<Uuid>,
    pub lease_expires: Option<u64>,
    pub attempts: i32,
    pub receipt: Option<media::Receipt>,
    pub rejection: Option<media::Rejection>,
    pub limits: media::Limits,
    pub cleanup: String,
    pub withheld: bool,
}
impl Intent {
    pub fn original(&self, user: &User) -> Result<()> {
        if self.actor != user.id || self.identity != key(user)? {
            return Err(Error::NotFound);
        }
        Ok(())
    }
    pub fn live(&self, now: u64) -> Result<()> {
        if now < self.transition {
            return Err(Error::Conflict);
        }
        if now >= self.upload || self.cleanup != "none" {
            return Err(Error::Expired);
        }
        Ok(())
    }
    pub fn status(&self, at: u64) -> Result<media::UploadStatus> {
        let mut recovery = media::Recovery {
            put_allowed: false,
            finalize_allowed: false,
            reason: None,
        };
        use media::{RecoveryReason as R, UploadState as S};
        recovery.reason = if self.state == S::Retained {
            Some(R::AlreadyRetained)
        } else if self.state == S::Rejected {
            Some(R::Rejected)
        } else if at < self.transition {
            Some(R::ClockRegression)
        } else if at >= self.upload || self.state == S::Expired || self.cleanup != "none" {
            Some(R::Expired)
        } else if self.lease_expires.is_some_and(|v| v > at) {
            Some(R::Busy)
        } else if self.attempts >= 8 {
            Some(R::RetryBudgetExhausted)
        } else {
            recovery.put_allowed = matches!(self.state, S::Reserved | S::Received);
            recovery.finalize_allowed = matches!(self.state, S::Received | S::Finalizing);
            None
        };
        let artifact = self
            .receipt
            .as_ref()
            .map(|receipt| {
                Ok::<media::Artifact, Error>(media::Artifact {
                    receipt: receipt.clone(),
                    receipt_digest: receipt.digest().map_err(|_| Error::Owner)?,
                    availability: media::Availability {
                        state: media::AvailabilityState::Available,
                        observed_at_ms: at,
                        reason: None,
                    },
                })
            })
            .transpose()?;
        Ok(media::UploadStatus {
            contract: media::Contract::HumanResultMedia1,
            operation_id: media::Id::new(self.id).map_err(|_| Error::Owner)?,
            actor_id: media::Id::new(self.actor).map_err(|_| Error::Owner)?,
            request_digest: media::Digest::of(&self.raw),
            binding: self.binding.clone(),
            input: media::Input {
                digest: self.request.input_digest.clone(),
                bytes: self.request.input_bytes,
                filename: self.request.filename.clone(),
            },
            capture: self.request.capture.clone(),
            state: self.state,
            created_at_ms: self.created,
            upload_expires_at_ms: self.upload,
            retain_until_ms: self.retain,
            recovery,
            rejection: self.rejection,
            artifact,
            limits: self.limits.clone(),
        })
    }
}
pub(super) async fn load_optional(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
) -> Result<Option<Intent>> {
    let row = sqlx::query("SELECT * FROM result_media_intents WHERE operation_id=$1 FOR UPDATE")
        .bind(id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(sql)?;
    let Some(r) = row else { return Ok(None) };
    let raw: Vec<u8> = r.get("request_bytes");
    let request = media::UploadCreate::decode(&raw).map_err(|_| Error::Owner)?;
    let binding: media::ReportBinding =
        serde_json::from_value(r.get("report_binding")).map_err(|_| Error::Owner)?;
    binding.validate().map_err(|_| Error::Owner)?;
    let limits: media::Limits =
        serde_json::from_value(r.get("admission_limits")).map_err(|_| Error::Owner)?;
    limits.validate().map_err(|_| Error::Owner)?;
    request
        .validate_with_limits(&limits)
        .map_err(|_| Error::Owner)?;
    let receipt_bytes: Option<Vec<u8>> = r.get("receipt_bytes");
    let receipt: Option<media::Receipt> = receipt_bytes
        .as_ref()
        .map(|bytes| {
            let receipt: media::Receipt =
                serde_json::from_slice(bytes).map_err(|_| Error::Owner)?;
            receipt.validate().map_err(|_| Error::Owner)?;
            if media::canonical_bytes(&receipt).map_err(|_| Error::Owner)? != *bytes
                || receipt.binding != binding
                || receipt.artifact_id.as_uuid() != id
                || receipt.uploader_id.as_uuid() != r.get::<Uuid, _>("actor_id")
                || receipt.input.digest != request.input_digest
                || receipt.input.bytes != request.input_bytes
                || receipt.input.filename != request.filename
                || receipt.capture != request.capture
                || receipt.digest().map_err(|_| Error::Owner)?.as_str()
                    != r.get::<String, _>("receipt_digest")
            {
                return Err(Error::Owner);
            }
            Ok(receipt)
        })
        .transpose()?;
    if media::Digest::of(&raw).as_str() != r.get::<String, _>("request_digest")
        || request.operation_id.as_uuid() != id
    {
        return Err(Error::Owner);
    }
    let state = serde_json::from_value(Value::String(r.get("state"))).map_err(|_| Error::Owner)?;
    let rejection = r
        .get::<Option<String>, _>("rejection")
        .map(|v| serde_json::from_value(Value::String(v)).map_err(|_| Error::Owner))
        .transpose()?;
    Ok(Some(Intent {
        id,
        actor: r.get("actor_id"),
        identity: r.get("actor_identity"),
        binding,
        storage: r.get("storage_id"),
        raw,
        request,
        state,
        created: r.get::<i64, _>("created_at_ms") as u64,
        upload: r.get::<i64, _>("upload_expires_at_ms") as u64,
        retain: r.get::<i64, _>("retain_until_ms") as u64,
        transition: r.get::<i64, _>("transition_at_ms") as u64,
        lease: r.get("lease_id"),
        lease_expires: r
            .get::<Option<i64>, _>("lease_expires_at_ms")
            .map(|v| v as u64),
        attempts: r.get("processing_attempts"),
        receipt,
        rejection,
        limits,
        cleanup: r.get("cleanup_state"),
        withheld: r.get::<Option<i64>, _>("withheld_at_ms").is_some(),
    }))
}
pub(super) async fn load(tx: &mut Transaction<'_, Postgres>, id: Uuid) -> Result<Intent> {
    load_optional(tx, id).await?.ok_or(Error::NotFound)
}
