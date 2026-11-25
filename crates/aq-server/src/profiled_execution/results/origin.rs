//! Exact historical result origin. Not an execution/release capability.
use super::{canonical, conflict, db};
use crate::{
    error::AppError,
    profiled_execution::{self as native, initial_authority, phase_profile, registration},
};
use aq_admission::{
    ArtifactRef, Digest, LaunchBinding, SessionIdentity, started, stopped,
    transport::{Feature, ResultReport},
};
use sqlx::{Postgres, Row, Transaction, postgres::PgRow};
use uuid::Uuid;
type Result<T> = std::result::Result<T, AppError>;
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Pin {
    pub kind: &'static str,
    pub execution: Option<String>,
    pub stopped_ack_bytes: Option<String>,
}
impl Pin {
    pub fn legacy() -> Self {
        Self {
            kind: "legacy_prepare_v1",
            execution: None,
            stopped_ack_bytes: None,
        }
    }
    pub async fn writer(&self, tx: &mut Transaction<'_, Postgres>) -> Result<()> {
        if let (Some(execution), Some(stopped)) = (&self.execution, &self.stopped_ack_bytes) {
            sqlx::query("SELECT set_config('aq.result_origin',$1,true)")
                .bind(format!(
                    "aq.result-origin.1:{}:{execution}:{stopped}",
                    self.kind
                ))
                .execute(&mut **tx)
                .await
                .map_err(db)?;
        }
        Ok(())
    }
    pub async fn require_row(&self, tx: &mut Transaction<'_, Postgres>, row: &PgRow) -> Result<()> {
        match row.try_get::<String, _>("result_origin_kind") {
            Ok(kind) => {
                if kind != self.kind
                    || row.get::<Option<String>, _>("origin_execution_receipt_digest")
                        != self.execution
                    || row.get::<Option<String>, _>("origin_stopped_ack_bytes_digest")
                        != self.stopped_ack_bytes
                {
                    return Err(AppError::Internal);
                }
            }
            Err(sqlx::Error::ColumnNotFound(_)) if self.kind == "legacy_prepare_v1" => {
                let version: String =
                    sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
                        .fetch_one(&mut **tx)
                        .await
                        .map_err(db)?;
                if version.parse::<u32>().map_err(|_| AppError::Internal)? >= 38 {
                    return Err(AppError::Internal);
                }
            }
            Err(_) => return Err(AppError::Internal),
        }
        Ok(())
    }
}
pub(super) struct Legacy {
    pub stop: stopped::Request,
    pub ack: stopped::Receipt,
}
pub(super) enum ResultOrigin {
    Legacy(Box<Legacy>),
    Frozen(Box<frozen::Verified>),
    Refresh(Box<refresh::Verified>),
}
impl ResultOrigin {
    pub fn pin(&self) -> Pin {
        match self {
            Self::Legacy(_) => Pin::legacy(),
            Self::Frozen(v) => v.pin.clone(),
            Self::Refresh(v) => v.pin.clone(),
        }
    }
    pub fn stopped_receipt(&self) -> &stopped::Receipt {
        match self {
            Self::Legacy(v) => &v.ack,
            Self::Frozen(v) => &v.stopped_ack.original,
            Self::Refresh(v) => &v.stopped_ack.original,
        }
    }
    pub fn stop(&self) -> &stopped::Request {
        match self {
            Self::Legacy(v) => &v.stop,
            Self::Frozen(v) => &v.stopped.original,
            Self::Refresh(v) => &v.stopped.original,
        }
    }
    pub fn require_report(&self, report: &ResultReport) -> Result<()> {
        if canonical(&report.stop)? != canonical(&self.stop().evidence)? {
            return Err(conflict());
        }
        Ok(())
    }
    pub fn artifact(&self) -> Result<ArtifactRef> {
        self.stopped_receipt()
            .artifact()
            .map_err(|_| AppError::Internal)
    }
}
/// Caller holds actual common node→attempt/hot locks. Stored history is read
/// without requiring current launch allowlist or an unexpired execution grant.
pub(super) async fn load_locked(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    binding: &LaunchBinding,
    grant: Uuid,
    recording: bool,
) -> Result<ResultOrigin> {
    match phase_profile::for_attempt_locked(tx, binding.attempt_id).await? {
        None => {
            initial_authority::require_legacy_origin_locked(tx, binding).await?;
            Ok(ResultOrigin::Legacy(Box::new(
                legacy(tx, node, binding, recording).await?,
            )))
        }
        Some(phase_profile::Profile::Frozen) => {
            let initial_authority::InitialAuthorityHistory::PhaseExecution(history) =
                initial_authority::load_locked(tx, node, binding, grant).await?
            else {
                return Err(conflict());
            };
            Ok(ResultOrigin::Frozen(Box::new(
                frozen::load(tx, node, binding, *history).await?,
            )))
        }
        Some(phase_profile::Profile::Refresh) => {
            let initial_authority::InitialAuthorityHistory::PhaseRefreshExecution(history) =
                initial_authority::load_locked(tx, node, binding, grant).await?
            else {
                return Err(conflict());
            };
            Ok(ResultOrigin::Refresh(Box::new(
                refresh::load(tx, node, binding, *history).await?,
            )))
        }
    }
}
async fn require_legacy_row_locked(
    tx: &mut Transaction<'_, Postgres>,
    row: &PgRow,
    node: Uuid,
) -> Result<()> {
    if row.get::<Uuid, _>("node_id") != node {
        return Err(conflict());
    }
    match row.try_get::<String, _>("initial_authority_kind") {
        Ok(kind) if kind == "legacy_prepare_v1" => (),
        Err(sqlx::Error::ColumnNotFound(_)) => {
            let version: String =
                sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
                    .fetch_one(&mut **tx)
                    .await
                    .map_err(db)?;
            if version.parse::<u32>().map_err(|_| AppError::Internal)? >= 33 {
                return Err(conflict());
            }
        }
        _ => return Err(conflict()),
    }
    Ok(())
}
async fn legacy(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    binding: &LaunchBinding,
    recording: bool,
) -> Result<Legacy> {
    let stop = sqlx::query("SELECT * FROM profiled_stopped_receipts WHERE attempt_id=$1 FOR SHARE")
        .bind(binding.attempt_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(db)?
        .ok_or_else(conflict)?;
    let start =
        sqlx::query("SELECT * FROM profiled_started_receipts WHERE attempt_id=$1 FOR SHARE")
            .bind(binding.attempt_id)
            .fetch_optional(&mut **tx)
            .await
            .map_err(db)?
            .ok_or_else(conflict)?;
    for row in [&start, &stop] {
        require_legacy_row_locked(tx, row, node).await?;
    }
    let history = decode_legacy_history(&start, &stop, binding)?;
    if recording {
        require_recording_runtime_locked(tx, node, binding, &history.stop).await?;
    }
    Ok(history)
}

fn decode_legacy_history(start: &PgRow, stop: &PgRow, binding: &LaunchBinding) -> Result<Legacy> {
    let q: started::Request = started::decode_request(&start.get::<Vec<u8>, _>("request_bytes"))
        .map_err(|_| AppError::Internal)?;
    let a = started::decode_receipt(&start.get::<Vec<u8>, _>("receipt_bytes"), &q)
        .map_err(|_| AppError::Internal)?;
    let stopped = stopped::decode_request(&stop.get::<Vec<u8>, _>("request_bytes"))
        .map_err(|_| AppError::Internal)?;
    let ack = stopped::decode_receipt(&stop.get::<Vec<u8>, _>("receipt_bytes"), &stopped)
        .map_err(|_| AppError::Internal)?;
    stopped.validate_after(&q).map_err(|_| AppError::Internal)?;
    if q.binding != *binding
        || stopped.evidence.binding != *binding
        || canonical(&q)? != start.get::<Vec<u8>, _>("request_bytes")
        || canonical(&a)? != start.get::<Vec<u8>, _>("receipt_bytes")
        || q.digest().map_err(|_| AppError::Internal)?.as_str()
            != start.get::<String, _>("request_digest")
        || canonical(&stopped)? != stop.get::<Vec<u8>, _>("request_bytes")
        || canonical(&ack)? != stop.get::<Vec<u8>, _>("receipt_bytes")
        || stopped.digest().map_err(|_| AppError::Internal)?.as_str()
            != stop.get::<String, _>("request_digest")
        || canonical(&stopped.proof)? != stop.get::<Vec<u8>, _>("proof_bytes")
        || Digest::of(&canonical(&stopped.proof)?).as_str() != stop.get::<String, _>("proof_digest")
    {
        return Err(AppError::Internal);
    }
    Ok(Legacy { stop: stopped, ack })
}

async fn require_recording_runtime_locked(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    binding: &LaunchBinding,
    stopped: &stopped::Request,
) -> Result<()> {
    let SessionIdentity::Remote { boot_id, .. } = binding.session else {
        return Err(conflict());
    };
    let admitted = registration::admitted_runtime_locked(tx, node, boot_id).await?;
    if admitted.request.runtime != stopped.runtime
        || admitted.qualification != stopped.qualification
        || !admitted
            .request
            .features
            .contains(&Feature::HostStopEvidenceV1)
        || !admitted
            .request
            .features
            .contains(&Feature::DurableHostJournalV1)
    {
        return Err(conflict());
    }
    Ok(())
}
mod frozen {
    use super::*;
    use aq_phase_terminal_protocol as wire;
    use native::phase_execution as phase_owner;
    const KIND: &str = "phase_execution_v2";
    include!("phase_origin.rs");
}
mod refresh {
    use super::*;
    use aq_phase_terminal_protocol::refresh_v1 as wire;
    use native::phase_refresh_execution as phase_owner;
    const KIND: &str = "phase_refresh_execution_v1";
    include!("phase_origin.rs");
}
