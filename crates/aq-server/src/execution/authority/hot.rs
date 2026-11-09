//! Compact mutable authority. No manifest, source artifact or credential bytes.
//! Receipt references are projections; only owning producers verify release.
use crate::execution::profiled::{Effects, Retention, Status, Stop};
use aq_session_protocol::{LaunchBinding, ProfiledGrant, Validate};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const MAX_HOT_BYTES: usize = 16_384;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchOwner {
    pub guardian_id: Uuid,
    pub launch_id: Uuid,
    pub reservation_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HotState {
    pub schema_version: u32,
    pub status: Status,
    pub grant: Option<ProfiledGrant>,
    pub execution_deadline_ms: Option<u64>,
    pub owner: Option<LaunchOwner>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preparation_operation_id: Option<Uuid>,
    pub cancellation_id: Option<Uuid>,
    pub revoked: bool,
    pub stop: Stop,
    pub retention: Retention,
    pub effects: Effects,
}
impl HotState {
    /// Initial adoption never invents a host owner for an old active grant.
    /// Such rows remain on the qualified compatibility path until a producer
    /// can verify and migrate their actual durable launch identity.
    pub fn from_compat(
        value: &crate::execution::profiled::ProfiledState,
    ) -> Result<Self, &'static str> {
        value.validate()?;
        let result = Self {
            schema_version: 1,
            status: value.status,
            grant: value.grant.clone(),
            execution_deadline_ms: value.execution_deadline_ms,
            owner: None,
            preparation_operation_id: None,
            cancellation_id: value.cancellation_id,
            revoked: value.revoked,
            stop: value.stop.clone(),
            retention: value.retention.clone(),
            effects: value.effects.clone(),
        };
        result.validate_against(&value.offer.binding)?;
        Ok(result)
    }
    pub fn validate_against(&self, binding: &LaunchBinding) -> Result<(), &'static str> {
        binding.validate().map_err(|_| "hot launch binding")?;
        if self.schema_version != 1
            || serde_json::to_vec(self).map_err(|_| "hot encoding")?.len() > MAX_HOT_BYTES
        {
            return Err("hot state version or byte bound");
        }
        if self.preparation_operation_id.is_some_and(|id| id.is_nil())
            || self.cancellation_id.is_some_and(|id| id.is_nil())
            || self.execution_deadline_ms == Some(0)
            || self.owner.as_ref().is_some_and(|owner| {
                owner.guardian_id.is_nil()
                    || owner.launch_id.is_nil()
                    || owner.reservation_id.is_nil()
            })
        {
            return Err("hot identity or deadline");
        }
        if let Some(grant) = &self.grant {
            grant.validate().map_err(|_| "hot grant")?;
            if &grant.binding != binding
                || self.owner.is_none()
                || self
                    .execution_deadline_ms
                    .is_none_or(|deadline| grant.expires_at_ms > deadline)
            {
                return Err("hot grant scope or ceiling");
            }
        }
        if self.status == Status::Granted
            && (self.grant.is_none()
                || self.owner.is_none()
                || self.revoked
                || self.cancellation_id.is_some()
                || self.stop.known_stopped())
        {
            return Err("granted hot state needs actual grant and owner");
        }
        if self.status == Status::NeverGranted
            && (self.grant.is_some() || !matches!(self.stop, Stop::Unknown))
        {
            return Err("never-granted is not physical stop proof");
        }
        if (self.status == Status::Offered
            && (self.owner.is_some()
                || self.grant.is_some()
                || !matches!(self.stop, Stop::Unknown)))
            || (self.status == Status::Preparing
                && (self.owner.is_none()
                    || self.grant.is_some()
                    || !matches!(self.stop, Stop::Unknown)))
            || (matches!(self.status, Status::Finished | Status::Declined)
                && !self.stop.known_stopped())
            || (self.status == Status::Declined && self.grant.is_some())
            || (self.stop.known_stopped()
                && (self.owner.is_none()
                    || matches!(
                        self.status,
                        Status::Offered | Status::Preparing | Status::Granted
                    )))
        {
            return Err("inconsistent hot status and host owner");
        }
        for reference in [
            match &self.stop {
                Stop::NotStarted { receipt } | Stop::Stopped { receipt } => Some(receipt),
                _ => None,
            },
            match &self.retention {
                Retention::Retained { receipt } => Some(receipt),
                _ => None,
            },
            match &self.effects {
                Effects::Quiescent { receipt } => Some(receipt),
                _ => None,
            },
        ]
        .into_iter()
        .flatten()
        {
            reference
                .id
                .validate()
                .map_err(|_| "hot receipt identity")?;
            reference
                .version
                .validate()
                .map_err(|_| "hot receipt version")?;
        }
        if matches!(self.retention, Retention::NotRequired)
            && !matches!(self.stop, Stop::NotStarted { .. })
        {
            return Err("retention not required only with verified nonstart");
        }
        if matches!(self.effects, Effects::Pending {count} if count > 4096) {
            return Err("hot pending effect bound");
        }
        Ok(())
    }
    /// Completeness only, not physical-stop or receipt-owner proof.
    pub fn release_projection(&self) -> bool {
        self.stop.known_stopped()
            && matches!(
                self.retention,
                Retention::NotRequired | Retention::Retained { .. }
            )
            && matches!(self.effects, Effects::Quiescent { .. })
    }
}

/// Build the common projection using freshly locked mutable state plus verified
/// immutable facts. This does not apply caller authorization or active policy;
/// the common loader owns both after node/attempt/hot locks and current clock.
pub fn project(
    state: HotState,
    facts: super::cache::Facts,
    key: &super::cache::Key,
    revisions: (u64, u64, u64),
    node_revoked: bool,
    holds_task: bool,
) -> super::Result<super::AuthorityView> {
    use super::{AuthorityError, AuthorityView, Kind};
    state
        .validate_against(&facts.binding)
        .map_err(|_| AuthorityError::InvalidState)?;
    if revisions.0 == 0
        || revisions.1 == 0
        || key.attempt_id != facts.binding.attempt_id
        || key.execution_spec_digest != facts.binding.execution_spec_digest.as_str()
        || key.launch_manifest_digest != facts.binding.launch_manifest_digest
    {
        return Err(AuthorityError::InvalidState);
    }
    let (identity, attempt) = super::remote(&facts.binding.session)?;
    let release_ready = state.release_projection();
    Ok(AuthorityView {
        kind: Kind::ProfiledV1,
        session: facts.binding.session,
        identity,
        attempt,
        offer_id: facts.binding.offer_id,
        project_id: facts.project_id,
        task_id: facts.task_id,
        graph: Some(facts.graph),
        source: Some(facts.source),
        record_revision: revisions.0,
        authority_revision: revisions.1,
        grant_revision: revisions.2,
        status: state.status,
        holds_task,
        grant_id: state.grant.as_ref().map(|grant| grant.grant_id),
        grant_expires_at_ms: state
            .grant
            .as_ref()
            .map(|grant| grant.expires_at_ms)
            .unwrap_or(0),
        execution_deadline_ms: state.execution_deadline_ms,
        execution_spec_digest: key.execution_spec_digest.clone(),
        launch_manifest_digest: Some(key.launch_manifest_digest.clone()),
        terminal_policy: Some(facts.terminal_policy),
        selection_digest: Some(key.selection_digest.clone()),
        cancellation_id: state.cancellation_id,
        revoked: node_revoked || state.revoked,
        stopped: state.stop.known_stopped(),
        stop: Some(state.stop),
        retention: Some(state.retention),
        effects: Some(state.effects),
        release_ready,
        retention_released: release_ready,
        rows: super::Rows::Lock,
    })
}

/// Migration-013 path. Caller owns
/// scoped human/worker authorization exactly as for the common AuthorityView.
/// None means no adoption: retain the fully verified compatibility loader.
pub(super) async fn load_adopted_locked(
    state: Option<&crate::AppState>,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    node: Uuid,
    attempt_id: Uuid,
    policy: super::Policy,
    rows: super::Rows,
) -> super::Result<Option<super::AuthorityView>> {
    use super::{
        AuthorityError, Policy,
        cache::{Facts, Key},
    };
    use aq_session_protocol::{Digest, LaunchBinding};
    use sqlx::Row;
    if node.is_nil() || attempt_id.is_nil() {
        return Err(AuthorityError::Identity);
    }
    let revoked = super::node_revoked(tx, node, policy, rows).await?;
    let common=sqlx::query(&rows.sql("SELECT kind,task_id,offer_id,session_id,execution_deadline_ms,holds_task FROM execution_attempts WHERE attempt_id=$1 AND node_id=$2", "FOR UPDATE"))
        .bind(attempt_id).bind(node).fetch_optional(&mut **tx).await?.ok_or(AuthorityError::NotFound)?;
    if common.get::<String, _>("kind") != "profiled_v1" {
        return Ok(None);
    }
    // Cache hits fetch only immutable identity/digest columns, never launch,
    // selection, spec or historical-state JSON. All joined rows have SQL guards.
    let Some(row)=sqlx::query(&rows.sql("SELECT h.state,h.record_revision,h.authority_revision,h.grant_revision,l.decision_id,l.source_observation_id,l.execution_spec_digest,l.manifest_digest,s.selection_digest FROM profiled_authority_hot h JOIN profiled_launches l ON l.attempt_id=h.attempt_id JOIN intelligence_attempt_selections s ON s.attempt_id=h.attempt_id JOIN intelligence_decisions d ON d.decision_id=l.decision_id WHERE h.attempt_id=$1", "FOR UPDATE OF h FOR SHARE OF l,s,d"))
        .bind(attempt_id).fetch_optional(&mut **tx).await? else {return Ok(None);};
    let digest = |field: &str| -> super::Result<Digest> {
        serde_json::from_value(serde_json::Value::String(row.get(field)))
            .map_err(|_| AuthorityError::InvalidState)
    };
    let key = Key {
        attempt_id,
        decision_id: row.get("decision_id"),
        source_observation_id: row.get("source_observation_id"),
        execution_spec_digest: row.get("execution_spec_digest"),
        launch_manifest_digest: digest("manifest_digest")?,
        selection_digest: digest("selection_digest")?,
    };
    let cached = if let Some(state) = state {
        state
            .store
            .verified_profiled_facts
            .lock()
            .map_err(|_| AuthorityError::InvalidState)?
            .get(&key)
    } else {
        None
    };
    let facts = if let Some(facts) = cached {
        facts
    } else {
        let body = sqlx::query_scalar("SELECT state FROM execution_attempts WHERE attempt_id=$1")
            .bind(attempt_id)
            .fetch_one(&mut **tx)
            .await?;
        let verified = super::load_profiled_compat(
            tx,
            body,
            attempt_id,
            common.get("execution_deadline_ms"),
            common.get("holds_task"),
            revoked,
            rows,
        )
        .await?;
        if verified.selection_digest.as_ref() != Some(&key.selection_digest)
            || verified.launch_manifest_digest.as_ref() != Some(&key.launch_manifest_digest)
            || verified.execution_spec_digest != key.execution_spec_digest
        {
            return Err(AuthorityError::InvalidState);
        }
        let graph = verified.graph.ok_or(AuthorityError::InvalidState)?;
        let graph_digest =
            serde_json::from_value(serde_json::Value::String(graph.profile_digest.clone()))
                .map_err(|_| AuthorityError::InvalidState)?;
        let spec_digest =
            serde_json::from_value(serde_json::Value::String(key.execution_spec_digest.clone()))
                .map_err(|_| AuthorityError::InvalidState)?;
        let facts = Facts {
            binding: LaunchBinding {
                schema_version: 1,
                session: verified.session,
                offer_id: verified.offer_id,
                attempt_id: verified.attempt.attempt_id,
                assignment_generation: verified.attempt.generation,
                execution_spec_digest: spec_digest,
                launch_manifest_digest: key.launch_manifest_digest.clone(),
                graph_access_profile_digest: graph_digest,
            },
            project_id: verified.project_id,
            task_id: verified.task_id,
            graph,
            terminal_policy: verified
                .terminal_policy
                .ok_or(AuthorityError::InvalidState)?,
            source: verified.source.ok_or(AuthorityError::InvalidState)?,
        };
        if let Some(state) = state {
            state
                .store
                .verified_profiled_facts
                .lock()
                .map_err(|_| AuthorityError::InvalidState)?
                .insert_verified(key.clone(), facts.clone());
        }
        facts
    };
    let hot: HotState =
        serde_json::from_value(row.get("state")).map_err(|_| AuthorityError::InvalidState)?;
    let view = project(
        hot,
        facts,
        &key,
        (
            row.get::<i64, _>("record_revision") as u64,
            row.get::<i64, _>("authority_revision") as u64,
            row.get::<i64, _>("grant_revision") as u64,
        ),
        revoked,
        common.get("holds_task"),
    )?;
    if view.identity.node_id != node
        || view.attempt.attempt_id != attempt_id
        || view.task_id != common.get::<Uuid, _>("task_id")
        || view.offer_id != common.get::<Uuid, _>("offer_id")
        || view.attempt.session_id != common.get::<Uuid, _>("session_id")
    {
        return Err(AuthorityError::InvalidState);
    }
    if matches!(policy, Policy::GraphActive | Policy::TerminalActive) {
        if !common.get::<bool, _>("holds_task") {
            return Err(AuthorityError::Revoked);
        }
        let registered: Option<serde_json::Value> =
            sqlx::query_scalar("SELECT state->'identity' FROM execution_nodes WHERE node_id=$1")
                .bind(node)
                .fetch_optional(&mut **tx)
                .await?;
        if registered
            != Some(serde_json::to_value(&view.identity).map_err(|_| AuthorityError::InvalidState)?)
        {
            return Err(AuthorityError::StaleBoot);
        }
        view.active_at(
            (state.ok_or(AuthorityError::Unsupported)?.clock)()
                .timestamp_millis()
                .max(0) as u64,
        )?;
    }
    if matches!(policy, Policy::TerminalActive)
        && (view.execution_deadline_ms.is_none() || !matches!(view.stop, Some(Stop::Running)))
    {
        return Err(AuthorityError::Unsupported);
    }
    Ok(Some(view))
}
