//! Recheck each source of a frozen selection snapshot under the admission locks.
use super::{Result, *};
type Tx<'a> = sqlx::Transaction<'a, sqlx::Postgres>;
async fn fence_credentials(
    state: &AppState,
    tx: &mut Tx<'_>,
    decision: &Decision,
    owned_attempt: bool,
) -> Result<Vec<AuthorizedCredential>> {
    let mut credentials =
        aq_intelligence_config::node_credentials(loaded(state)?, decision.project.node_id);
    let frozen = owned_attempt.then_some(decision.inputs.credentials.as_slice());
    aq_swarm_settings::providers::mask(tx, &mut credentials, frozen)
        .await
        .map_err(db)?;
    let decided = decision.inputs.now_ms as i64;
    aq_login_store::mask_stale(
        tx,
        loaded(state)?,
        &mut credentials,
        aq_login_store::LoginSelection::at(decided, frozen),
    )
    .await?;
    if digest(&credentials).map_err(|_| AppError::Internal)?
        != digest(&decision.inputs.credentials).map_err(|_| AppError::Internal)?
    {
        return Err(conflict("credential authorization changed"));
    }
    Ok(credentials)
}
pub(super) async fn fence_snapshot(
    state: &AppState,
    tx: &mut Tx<'_>,
    decision: &Decision,
    owned_attempt: bool,
) -> Result<()> {
    crate::project_bindings::fence_for_offer(state, tx, &decision.project).await?;
    registry::check_current(state, tx).await?;
    fence_node(tx, decision).await?;
    fence_availability(state, tx, decision, owned_attempt).await?;
    fence_retry_history(tx, decision, owned_attempt).await?;
    check_time(state, decision)
}
async fn fence_node(tx: &mut Tx<'_>, decision: &Decision) -> Result<()> {
    let current: Value =
        sqlx::query_scalar("SELECT state FROM execution_nodes WHERE node_id=$1 FOR SHARE")
            .bind(decision.project.node_id)
            .fetch_one(&mut **tx)
            .await
            .map_err(db)?;
    let ExecutionTarget::Worker { node_id, boot_id } = decision.inputs.node.target else {
        return Err(conflict(
            "central execution needs a distinct central authority adapter",
        ));
    };
    if node_id != decision.project.node_id || current["identity"]["boot_id"] != boot_id.to_string()
    {
        return Err(conflict("selection boot changed"));
    }
    let snapshot: Value = sqlx::query_scalar(
        "SELECT snapshot FROM intelligence_node_snapshots WHERE node_id=$1 FOR SHARE",
    )
    .bind(node_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(db)?;
    // A sibling slot's republish of the same installation is not a change.
    if !serde_json::from_value(snapshot).is_ok_and(|s| decision.inputs.node.carried_by(&s)) {
        return Err(conflict("installed capability snapshot changed"));
    }
    Ok(())
}
async fn fence_availability(
    state: &AppState,
    tx: &mut Tx<'_>,
    decision: &Decision,
    owned_attempt: bool,
) -> Result<()> {
    let credentials = fence_credentials(state, tx, decision, owned_attempt).await?;
    let node_id = decision.project.node_id;
    let rows:Vec<Value>=sqlx::query_scalar("SELECT observation FROM intelligence_observations ORDER BY scope_source_digest LIMIT 1024 FOR SHARE").fetch_all(&mut **tx).await.map_err(db)?;
    let mut expected = evidence::observed(rows, &credentials, node_id)?;
    let at = aq_machine_control::AttestationTime {
        revision: decision.inputs.registry_revision,
        now_ms: decision.inputs.now_ms,
    };
    let owner = aq_machine_control::owner_attested(tx, &credentials, &expected, at);
    expected.extend(owner.await?);
    if digest(&expected).map_err(|_| AppError::Internal)?
        != digest(&decision.inputs.observations).map_err(|_| AppError::Internal)?
    {
        return Err(conflict("relevant availability changed"));
    }
    Ok(())
}
async fn fence_retry_history(
    tx: &mut Tx<'_>,
    decision: &Decision,
    owned_attempt: bool,
) -> Result<()> {
    use crate::profiled_execution::no_grant;
    let task = decision.inputs.project.lineage.task_id;
    let mut counts = no_grant::check_retry_bound(no_grant::attempt_counts_locked(tx, task).await?)?;
    if owned_attempt {
        counts.started = counts
            .started
            .checked_sub(1)
            .ok_or_else(|| conflict("reserved attempt count mismatch"))?;
        counts.history = counts
            .history
            .checked_sub(1)
            .ok_or_else(|| conflict("reserved history count mismatch"))?;
    }
    if counts.started != decision.inputs.budget.attempts_started
        || counts.budget_revision() != decision.inputs.budget.revision
    {
        return Err(conflict("task retry budget/history changed"));
    }
    Ok(())
}
