//! Kind-dispatched common authority reads. Current eligibility is sampled AFTER
//! node/attempt locks. Consumers still own independent graph/source/terminal ACLs.
pub mod cache;
pub mod hot;
use super::{
    AttemptState, AuthorityStatus,
    profiled::{ProfiledState, Status},
};
use crate::{AppState, error::AppError};
use aq_session_protocol::{Digest, SessionIdentity};
use aq_worker_protocol::{AttemptRef, GraphBinding, NodeIdentity, SourceBinding};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    LegacyFixture,
    ProfiledV1,
}
#[derive(Debug, Clone, Copy)]
pub enum Policy {
    GraphActive,
    SourceRetention,
    TerminalActive,
    TerminalHistorical,
    IdentityOnly,
}
#[derive(Debug, thiserror::Error)]
pub enum AuthorityError {
    #[error("attempt not found")]
    NotFound,
    #[error("authority revoked")]
    Revoked,
    #[error("stale execution boot")]
    StaleBoot,
    #[error("execution authority expired")]
    Expired,
    #[error("authority identity mismatch")]
    Identity,
    #[error("authority kind unsupported")]
    Unsupported,
    #[error("invalid stored authority")]
    InvalidState,
    #[error("authority database failure")]
    Database(#[from] sqlx::Error),
}
impl From<AuthorityError> for AppError {
    fn from(error: AuthorityError) -> Self {
        match error {
            AuthorityError::NotFound => Self::NotFound("attempt not found".into()),
            AuthorityError::Revoked => Self::CredentialRevoked,
            AuthorityError::Expired
            | AuthorityError::StaleBoot
            | AuthorityError::Identity
            | AuthorityError::Unsupported => Self::Conflict(error.to_string()),
            AuthorityError::InvalidState | AuthorityError::Database(_) => Self::Internal,
        }
    }
}
type Result<T> = std::result::Result<T, AuthorityError>;
#[derive(Debug, Clone)]
pub struct AuthorityView {
    pub kind: Kind,
    pub session: SessionIdentity,
    pub identity: NodeIdentity,
    pub attempt: AttemptRef,
    pub offer_id: Uuid,
    pub project_id: Uuid,
    pub task_id: Uuid,
    pub graph: Option<GraphBinding>,
    pub source: Option<SourceBinding>,
    pub record_revision: u64,
    pub authority_revision: u64,
    pub grant_revision: u64,
    pub status: Status,
    pub holds_task: bool,
    pub grant_id: Option<Uuid>,
    pub grant_expires_at_ms: u64,
    pub execution_deadline_ms: Option<u64>,
    pub execution_spec_digest: String,
    pub launch_manifest_digest: Option<Digest>,
    pub terminal_policy: Option<aq_session_protocol::TerminalPolicy>,
    pub selection_digest: Option<Digest>,
    pub cancellation_id: Option<Uuid>,
    pub revoked: bool,
    pub stopped: bool,
    /// Owning profiled receipt projection; None for legacy, never fabricated.
    pub stop: Option<super::profiled::Stop>,
    pub retention: Option<super::profiled::Retention>,
    pub effects: Option<super::profiled::Effects>,
    /// Projection only. Actual receipt owners must verify physical stop,
    /// retention and effects before a producer clears the common task claim.
    pub release_ready: bool,
    pub retention_released: bool,
}
impl AuthorityView {
    pub fn require_session(&self, expected: &SessionIdentity) -> Result<()> {
        if &self.session != expected {
            Err(AuthorityError::Identity)
        } else {
            Ok(())
        }
    }
    pub fn running_at(&self, now: u64) -> bool {
        self.active_at(now).is_ok() && matches!(self.stop, Some(super::profiled::Stop::Running))
    }
    pub fn active_at(&self, now: u64) -> Result<()> {
        if !self.holds_task
            || self.revoked
            || self.cancellation_id.is_some()
            || self.stopped
            || self.status != Status::Granted
        {
            return Err(AuthorityError::Revoked);
        }
        if now == 0
            || now >= self.grant_expires_at_ms
            || self.execution_deadline_ms.is_some_and(|d| now >= d)
        {
            return Err(AuthorityError::Expired);
        }
        Ok(())
    }
}
fn remote(session: &SessionIdentity) -> Result<(NodeIdentity, AttemptRef)> {
    match session {
        SessionIdentity::Remote {
            node_id,
            boot_id,
            attempt_id,
            assignment_generation,
            session_id,
        } => Ok((
            NodeIdentity {
                node_id: *node_id,
                boot_id: *boot_id,
            },
            AttemptRef {
                attempt_id: *attempt_id,
                session_id: *session_id,
                generation: *assignment_generation,
            },
        )),
        SessionIdentity::Central { .. } => Err(AuthorityError::Unsupported),
    }
}
pub async fn load_locked(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    expected: &SessionIdentity,
    policy: Policy,
) -> Result<AuthorityView> {
    let (identity, attempt) = remote(expected)?;
    let view = load_attempt_locked(state, tx, identity.node_id, attempt.attempt_id, policy).await?;
    view.require_session(expected)?;
    Ok(view)
}
pub async fn load_attempt_locked(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    attempt: Uuid,
    policy: Policy,
) -> Result<AuthorityView> {
    if let Some(view) = hot::load_adopted_locked(Some(state), tx, node, attempt, policy).await? {
        return Ok(view);
    }
    load_record_locked(tx, node, attempt, policy, || {
        (state.clock)().timestamp_millis().max(0) as u64
    })
    .await
}
pub async fn load_source_locked(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    attempt: Uuid,
) -> Result<AuthorityView> {
    if let Some(view) =
        hot::load_adopted_locked(None, tx, node, attempt, Policy::SourceRetention).await?
    {
        return Ok(view);
    }
    load_record_locked(tx, node, attempt, Policy::SourceRetention, || 0).await
}
async fn load_record_locked(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    attempt: Uuid,
    policy: Policy,
    clock: impl Fn() -> u64,
) -> Result<AuthorityView> {
    if node.is_nil() || attempt.is_nil() {
        return Err(AuthorityError::Identity);
    }
    let revoked: Option<bool> =
        sqlx::query_scalar("SELECT revoked_at IS NOT NULL FROM nodes WHERE id=$1 FOR UPDATE")
            .bind(node)
            .fetch_optional(&mut **tx)
            .await?;
    let revoked = revoked.ok_or(AuthorityError::NotFound)?;
    if revoked && !matches!(policy, Policy::TerminalHistorical | Policy::IdentityOnly) {
        return Err(AuthorityError::Revoked);
    }
    let row=sqlx::query("SELECT kind,state,task_id,offer_id,session_id,holds_task,record_revision,authority_revision,grant_revision,execution_deadline_ms FROM execution_attempts WHERE attempt_id=$1 AND node_id=$2 FOR UPDATE").bind(attempt).bind(node).fetch_optional(&mut **tx).await?.ok_or(AuthorityError::NotFound)?;
    let body: Value = row.get("state");
    let kind: String = row.get("kind");
    let mut view = if kind == "legacy_fixture" {
        let value: AttemptState =
            serde_json::from_value(body).map_err(|_| AuthorityError::InvalidState)?;
        let graph = value
            .offer
            .spec
            .bindings
            .as_ref()
            .and_then(|b| b.graph.clone());
        let source = value
            .offer
            .spec
            .bindings
            .as_ref()
            .and_then(|b| b.source.clone());
        let session = SessionIdentity::Remote {
            node_id: node,
            boot_id: value.offer.identity.boot_id,
            attempt_id: value.offer.attempt.attempt_id,
            assignment_generation: value.offer.attempt.generation,
            session_id: value.offer.attempt.session_id,
        };
        AuthorityView {
            kind: Kind::LegacyFixture,
            session,
            identity: value.offer.identity,
            attempt: value.offer.attempt,
            offer_id: value.offer.offer_id,
            project_id: value.offer.spec.project_id,
            task_id: value.task_id,
            graph,
            source,
            record_revision: 0,
            authority_revision: 0,
            grant_revision: 0,
            holds_task: row.get("holds_task"),
            status: match value.status {
                AuthorityStatus::Offered => Status::Offered,
                AuthorityStatus::Granted => Status::Granted,
                AuthorityStatus::Cancelling => Status::Cancelling,
                AuthorityStatus::Finished => Status::Finished,
                AuthorityStatus::Declined => Status::Declined,
            },
            grant_id: None,
            grant_expires_at_ms: value.grant_expires_at_ms,
            execution_deadline_ms: None,
            execution_spec_digest: value.offer.spec.resolved_spec_digest,
            launch_manifest_digest: None,
            terminal_policy: None,
            selection_digest: None,
            cancellation_id: value.cancellation_id,
            revoked,
            stopped: value.stopped,
            stop: None,
            retention: None,
            effects: None,
            release_ready: !row.get::<bool, _>("holds_task"),
            retention_released: value
                .disposition
                .as_ref()
                .is_some_and(|d| d.retention_release),
        }
    } else if kind == "profiled_v1" {
        load_profiled_compat(
            tx,
            body,
            attempt,
            row.get("execution_deadline_ms"),
            row.get("holds_task"),
            revoked,
        )
        .await?
    } else {
        return Err(AuthorityError::Unsupported);
    };
    if view.offer_id != row.get::<Uuid, _>("offer_id")
        || view.identity.node_id != node
        || view.attempt.attempt_id != attempt
        || view.attempt.session_id != row.get::<Uuid, _>("session_id")
        || view.task_id != row.get::<Uuid, _>("task_id")
    {
        return Err(AuthorityError::InvalidState);
    }
    view.record_revision = row.get::<i64, _>("record_revision") as u64;
    view.authority_revision = row.get::<i64, _>("authority_revision") as u64;
    view.grant_revision = row.get::<i64, _>("grant_revision") as u64;
    if matches!(policy, Policy::GraphActive | Policy::TerminalActive) {
        let registered: Option<Value> =
            sqlx::query_scalar("SELECT state->'identity' FROM execution_nodes WHERE node_id=$1")
                .bind(node)
                .fetch_optional(&mut **tx)
                .await?;
        if registered
            != Some(serde_json::to_value(&view.identity).map_err(|_| AuthorityError::InvalidState)?)
        {
            return Err(AuthorityError::StaleBoot);
        }
        view.active_at(clock())?;
    }
    if matches!(policy, Policy::TerminalActive)
        && (view.kind != Kind::ProfiledV1
            || view.execution_deadline_ms.is_none()
            || !matches!(view.stop, Some(super::profiled::Stop::Running)))
    {
        return Err(AuthorityError::Unsupported);
    }
    Ok(view)
}

/// Full immutable verification retained for compatibility and cache cold misses.
async fn load_profiled_compat(
    tx: &mut Transaction<'_, Postgres>,
    body: Value,
    attempt: Uuid,
    execution_deadline_ms: Option<i64>,
    holds_task: bool,
    revoked: bool,
) -> Result<AuthorityView> {
    let value: ProfiledState =
        serde_json::from_value(body).map_err(|_| AuthorityError::InvalidState)?;
    value.validate().map_err(|_| AuthorityError::InvalidState)?;
    let immutable=sqlx::query("SELECT l.launch_binding,l.launch_manifest,l.source_binding,l.manifest_digest,l.execution_spec_digest,l.execution_spec,s.selection_digest,s.selection,d.decision#>'{outcome,resolved}' AS admitted_selection FROM profiled_launches l JOIN intelligence_attempt_selections s ON s.attempt_id=l.attempt_id JOIN intelligence_decisions d ON d.decision_id=l.decision_id WHERE l.attempt_id=$1 FOR SHARE OF l,s,d").bind(attempt).fetch_optional(&mut **tx).await?.ok_or(AuthorityError::InvalidState)?;
    if immutable.get::<Value, _>("launch_binding")
        != serde_json::to_value(&value.offer.binding).map_err(|_| AuthorityError::InvalidState)?
        || immutable.get::<Value, _>("launch_manifest")
            != serde_json::to_value(&value.offer.manifest)
                .map_err(|_| AuthorityError::InvalidState)?
        || immutable.get::<Value, _>("source_binding")
            != serde_json::to_value(&value.source).map_err(|_| AuthorityError::InvalidState)?
        || immutable.get::<String, _>("manifest_digest")
            != value.offer.binding.launch_manifest_digest.as_str()
        || immutable.get::<String, _>("execution_spec_digest")
            != value.offer.binding.execution_spec_digest.as_str()
    {
        return Err(AuthorityError::InvalidState);
    }
    let selected: aq_selection::ResolvedSelection =
        serde_json::from_value(immutable.get("selection"))
            .map_err(|_| AuthorityError::InvalidState)?;
    let selection_digest =
        aq_selection::digest(&selected).map_err(|_| AuthorityError::InvalidState)?;
    let spec: Value = immutable.get("execution_spec");
    let admitted_selection: Value = immutable.get("admitted_selection");
    if spec.get("kind").and_then(Value::as_str) != Some("synthetic_profiled_authority_fixture") {
        let typed: aq_admission::execution_spec::ProfiledExecutionSpec =
            serde_json::from_value(spec.clone()).map_err(|_| AuthorityError::InvalidState)?;
        typed
            .validate_against(&value.offer)
            .map_err(|_| AuthorityError::InvalidState)?;
        let format = match value.source.base.algorithm {
            aq_worker_protocol::ObjectAlgorithm::Sha1 => aq_admission::ObjectFormat::Sha1,
        };
        if typed.selection_digest != selection_digest
            || value.offer.manifest.selection_explanation.digest != selection_digest
            || typed.source.repository_id != value.source.repository_id
            || typed.source.repository_binding_generation != value.source.binding_generation
            || typed.source.resolved_object.format != format
            || typed.source.resolved_object.hex != value.source.base.hex
            || typed.task_observation.digest.as_str() != value.graph.context_digest
        {
            return Err(AuthorityError::InvalidState);
        }
    }
    let selected_node = match &value.offer.binding.session {
        SessionIdentity::Remote {
            node_id, boot_id, ..
        } => aq_selection::ExecutionTarget::Worker {
            node_id: *node_id,
            boot_id: *boot_id,
        },
        _ => return Err(AuthorityError::Unsupported),
    };
    let choice = &selected.choice;
    let manifest = &value.offer.manifest;
    if admitted_selection
        != serde_json::to_value(&selected).map_err(|_| AuthorityError::InvalidState)?
        || selected.catalog_digest != manifest.catalog.digest
        || selected.project_revision != manifest.bindings.project_revision
        || immutable.get::<String, _>("selection_digest") != selection_digest.as_str()
        || aq_selection::digest(&spec).map_err(|_| AuthorityError::InvalidState)?
            != value.offer.binding.execution_spec_digest
        || selected.lineage.task_id != manifest.bindings.task_id
        || selected.lineage.workspace_id != manifest.bindings.graph_id
        || selected.lineage.project_id != manifest.bindings.project_id
        || selected.target != selected_node
        || choice.provider != manifest.model.provider.as_str()
        || choice.model != manifest.model.model.as_str()
        || choice.native_reasoning != manifest.model.reasoning.as_str()
        || choice.max_output_tokens != manifest.model.max_output_tokens
        || choice.harness != manifest.harness.harness.as_str()
        || choice.cli_version != manifest.harness.cli_version.as_str()
        || choice.adapter != manifest.harness.adapter
        || choice.environment != manifest.harness.environment
        || choice.resources.memory_bytes != manifest.resources.memory_bytes
        || choice.resources.cpu_millis != manifest.resources.cpu_millis
        || choice.resources.process_limit != manifest.resources.process_limit
        || choice.resources.wall_time_ms != manifest.resources.wall_time_ms
        || manifest.credentials.is_empty()
        || manifest
            .credentials
            .iter()
            .any(|c| c.credential_profile != choice.credential_profile)
    {
        return Err(AuthorityError::InvalidState);
    }
    let release_ready = value.release_ready();
    let session = value.offer.binding.session.clone();
    let (identity, attempt) = remote(&session)?;
    let deadline = execution_deadline_ms.map(|v| v as u64);
    if deadline != value.execution_deadline_ms {
        return Err(AuthorityError::InvalidState);
    }
    Ok(AuthorityView {
        kind: Kind::ProfiledV1,
        session,
        identity,
        attempt,
        offer_id: value.offer.binding.offer_id,
        project_id: value.offer.manifest.bindings.project_id,
        task_id: value.offer.manifest.bindings.task_id,
        graph: Some(value.graph),
        source: Some(value.source),
        record_revision: 0,
        authority_revision: 0,
        grant_revision: 0,
        status: value.status,
        holds_task,
        grant_id: value.grant.as_ref().map(|g| g.grant_id),
        grant_expires_at_ms: value.grant.as_ref().map(|g| g.expires_at_ms).unwrap_or(0),
        execution_deadline_ms: deadline,
        execution_spec_digest: value.offer.binding.execution_spec_digest.as_str().into(),
        launch_manifest_digest: Some(value.offer.binding.launch_manifest_digest),
        terminal_policy: Some(value.offer.manifest.terminal.clone()),
        selection_digest: Some(selection_digest),
        cancellation_id: value.cancellation_id,
        revoked: revoked || value.revoked,
        stopped: value.stop.known_stopped(),
        stop: Some(value.stop),
        retention: Some(value.retention),
        effects: Some(value.effects),
        release_ready,
        retention_released: release_ready,
    })
}
