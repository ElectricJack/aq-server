//! Shared actual phase history and transaction-local compatibility fence.
use super::super::{closure, initial_authority, phase_execution};
use crate::{
    AppState,
    error::AppError,
    execution::authority::{self, AuthorityView, Kind, Policy, hot::HotState},
    worker_api::{self, AuthenticatedNode},
};
use aq_admission::{
    ArtifactRef, LaunchBinding, SessionIdentity, control::Versions,
    granted_closure::JournalCheckpoint,
};
use aq_phase_terminal_protocol as wire;
use aq_session_protocol::Validate;
use sqlx::{Postgres, Row, Transaction};
use tokio::sync::{Semaphore, SemaphorePermit};
use uuid::Uuid;
static WORK: Semaphore = Semaphore::const_new(2);
static CONTROL: Semaphore = Semaphore::const_new(2);
pub(super) fn admit_work() -> Result<SemaphorePermit<'static>, AppError> {
    WORK.try_acquire()
        .map_err(|_| AppError::Maintenance("phase terminal work capacity unavailable".into()))
}
pub(super) fn admit_control() -> Result<SemaphorePermit<'static>, AppError> {
    CONTROL
        .try_acquire()
        .map_err(|_| AppError::Maintenance("phase terminal control capacity unavailable".into()))
}

pub(super) fn conflict() -> AppError {
    AppError::Conflict("phase terminal owner mismatch".into())
}
pub(super) fn database(_: sqlx::Error) -> AppError {
    AppError::Internal
}
pub(super) fn canonical(v: &impl serde::Serialize) -> Result<Vec<u8>, AppError> {
    let mut v = serde_json::to_value(v).map_err(|_| AppError::Internal)?;
    v.sort_all_objects();
    serde_json::to_vec(&v).map_err(|_| AppError::Internal)
}
pub(super) fn now(state: &AppState) -> Result<u64, AppError> {
    u64::try_from((state.clock)().timestamp_millis()).map_err(|_| conflict())
}

pub(super) struct Loaded {
    pub history: Box<phase_execution::history::PhaseExecutionHistory>,
    pub registration: closure::history::RegistrationHistory,
    pub view: AuthorityView,
    pub hot: HotState,
    pub versions: Versions,
    pub prepared: JournalCheckpoint,
    pub qualification: ArtifactRef,
}
impl Loaded {
    pub fn context(&self) -> wire::Context<'_> {
        wire::Context {
            execution: &self.history.receipt,
            prepare: &self.history.request,
            accepted: &self.history.accepted.receipt,
            acceptance: &self.history.accepted.request,
            proposal: &self.history.accepted.proposal,
            runtime: &self.registration.metadata.request.runtime,
            runtime_qualification: &self.registration.metadata.qualification,
            prepared: &self.prepared,
        }
    }
    /// Only an actual validated retained receipt supplies the SQL compatibility marker.
    pub async fn set_writer_marker(
        &self,
        tx: &mut Transaction<'_, Postgres>,
    ) -> Result<(), AppError> {
        sqlx::query("SELECT set_config('aq.phase_terminal_receipt',$1,true)")
            .bind(self.history.receipt_digest.as_str())
            .execute(&mut **tx)
            .await
            .map_err(database)?;
        Ok(())
    }
}
/// Caller begins a bounded transaction; this owns node→attempt/hot→history reads.
/// It proves historical ownership, not current permission to begin execution.
pub(super) async fn load_locked(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    node: &AuthenticatedNode,
    binding: &LaunchBinding,
    grant_id: Uuid,
) -> Result<Loaded, AppError> {
    binding.validate().map_err(|_| conflict())?;
    let SessionIdentity::Remote { node_id, .. } = binding.session else {
        return Err(conflict());
    };
    if node.id() != node_id {
        return Err(AppError::Forbidden("phase terminal node mismatch".into()));
    }
    worker_api::lock_current_node(tx, node).await?;
    let config = state
        .config
        .profiled_execution
        .as_ref()
        .ok_or_else(conflict)?;
    super::super::policies::fence_configuration_locked(tx, config).await?;
    let view = authority::load_locked(state, tx, &binding.session, Policy::IdentityOnly).await?;
    if view.kind != Kind::ProfiledV1
        || view.offer_id != binding.offer_id
        || view.launch_manifest_digest.as_ref() != Some(&binding.launch_manifest_digest)
        || view.execution_spec_digest != binding.execution_spec_digest.as_str()
    {
        return Err(conflict());
    }
    let row=sqlx::query("SELECT state,record_revision,authority_revision,grant_revision FROM profiled_authority_hot WHERE attempt_id=$1 FOR UPDATE").bind(binding.attempt_id).fetch_optional(&mut **tx).await.map_err(database)?.ok_or_else(conflict)?;
    let hot: HotState = serde_json::from_value(row.get("state")).map_err(|_| AppError::Internal)?;
    hot.validate_against(binding).map_err(|_| conflict())?;
    if hot
        .grant
        .as_ref()
        .is_none_or(|g| g.grant_id != grant_id || g.binding != *binding)
    {
        return Err(conflict());
    }
    let history = match initial_authority::load_locked(tx, node.id(), binding, grant_id).await? {
        initial_authority::InitialAuthorityHistory::PhaseExecution(h) => h,
        _ => return Err(conflict()),
    };
    let registration =
        closure::history::load_locked(tx, history.provenance.registration_id).await?;
    let prepared = JournalCheckpoint {
        revision: history.request.original.journal_revision,
        digest: history.request.original.journal_digest.clone(),
    };
    let qualification = super::qualification::for_history(state, &registration)?;
    let versions = Versions {
        record_revision: u64::try_from(row.get::<i64, _>("record_revision"))
            .map_err(|_| conflict())?,
        authority_revision: u64::try_from(row.get::<i64, _>("authority_revision"))
            .map_err(|_| conflict())?,
        grant_revision: u64::try_from(row.get::<i64, _>("grant_revision"))
            .map_err(|_| conflict())?,
    };
    versions.validate().map_err(|_| conflict())?;
    let loaded = Loaded {
        history,
        registration,
        view,
        hot,
        versions,
        prepared,
        qualification,
    };
    loaded.context().validate().map_err(|_| conflict())?;
    Ok(loaded)
}
