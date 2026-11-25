//! Compact authority loading from immutable launch facts.
use super::{
    AuthorityError, AuthorityView,
    cache::{Facts, Key},
};
pub use aq_profiled_state::hot::*;
use aq_session_protocol::LaunchBinding;
use sqlx::{Postgres, Row, Transaction, postgres::PgRow};
use uuid::Uuid;
/// Project locked state and verified facts; the common loader applies authorization.
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
    use super::Policy;
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
    let key = authority_key(&row, attempt_id)?;
    let facts = load_verified_facts(state, tx, &key, &common, revoked, rows).await?;
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
    view.require_record(node, attempt_id, &common)?;
    // Preserve the compact path's early task-claim refusal before boot checks.
    if matches!(policy, Policy::GraphActive | Policy::TerminalActive) && !view.holds_task {
        return Err(AuthorityError::Revoked);
    }
    view.require_policy(tx, policy, || {
        Ok((state.ok_or(AuthorityError::Unsupported)?.clock)()
            .timestamp_millis()
            .max(0) as u64)
    })
    .await?;
    Ok(Some(view))
}

fn authority_key(row: &PgRow, attempt_id: Uuid) -> super::Result<Key> {
    use aq_session_protocol::Digest;
    let digest = |field: &str| -> super::Result<Digest> {
        serde_json::from_value(serde_json::Value::String(row.get(field)))
            .map_err(|_| AuthorityError::InvalidState)
    };
    Ok(Key {
        attempt_id,
        decision_id: row.get("decision_id"),
        source_observation_id: row.get("source_observation_id"),
        execution_spec_digest: row.get("execution_spec_digest"),
        launch_manifest_digest: digest("manifest_digest")?,
        selection_digest: digest("selection_digest")?,
    })
}

async fn load_verified_facts(
    state: Option<&crate::AppState>,
    tx: &mut Transaction<'_, Postgres>,
    key: &Key,
    common: &PgRow,
    revoked: bool,
    rows: super::Rows,
) -> super::Result<Facts> {
    if let Some(state) = state
        && let Some(facts) = state
            .store
            .verified_profiled_facts
            .lock()
            .map_err(|_| AuthorityError::InvalidState)?
            .get(key)
    {
        return Ok(facts);
    }
    let body = sqlx::query_scalar("SELECT state FROM execution_attempts WHERE attempt_id=$1")
        .bind(key.attempt_id)
        .fetch_one(&mut **tx)
        .await?;
    let verified = super::load_profiled_compat(
        tx,
        body,
        key.attempt_id,
        common.get("execution_deadline_ms"),
        common.get("holds_task"),
        revoked,
        rows,
    )
    .await?;
    let facts = facts_from_verified(verified, key)?;
    if let Some(state) = state {
        state
            .store
            .verified_profiled_facts
            .lock()
            .map_err(|_| AuthorityError::InvalidState)?
            .insert_verified(key.clone(), facts.clone());
    }
    Ok(facts)
}

fn facts_from_verified(verified: AuthorityView, key: &Key) -> super::Result<Facts> {
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
    Ok(Facts {
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
    })
}
