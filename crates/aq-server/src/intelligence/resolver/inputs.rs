//! Assemble selection inputs from independently owned node, placement and budget facts.
use super::{Result, *};
use aq_project_policy::FrozenProjectBinding;
pub(super) async fn collect(
    state: &AppState,
    mut request: ResolveRequest,
    role_digest: &str,
) -> Result<(FrozenProjectBinding, SelectionInputs)> {
    let node_id = request.project.node_id;
    let evidence = evidence::NodeEvidence::load(state, node_id).await?;
    let time = evidence.time;
    let count = budget::attempts(state, &request.lineage).await?;
    let replacement = request.take_replacement(count.started).map_err(bad)?;
    let placement = crate::profiled_execution::placement::facts(
        state,
        node_id,
        request.lineage.project_id,
        &request.project.environment,
        time,
    )
    .await?;
    let accounting = budget::accounting(state, &request).await?;
    let input = SelectionInputs {
        project: request
            .project_snapshot(role_digest, &evidence.node.target)
            .map_err(|error| match error {
                aq_project_policy::decision::SnapshotError::Digest => AppError::Internal,
                aq_project_policy::decision::SnapshotError::Operation => bad("project operation"),
                aq_project_policy::decision::SnapshotError::Scope => bad("project scope"),
            })?,
        node: evidence.node,
        credentials: evidence.credentials,
        observations: evidence.observations,
        registry_revision: evidence.registry_revision,
        // Legacy attempts are counted conservatively. No latest stop/retention/
        // reconciliation or provider ledger authority is invented for them.
        budget: accounting
            .snapshot(
                aq_project_policy::decision::RetryHistory {
                    started: count.started,
                    revision: count.budget_revision(),
                    replacement: replacement.as_ref(),
                },
                request.lineage,
                time,
            )
            .ok_or(AppError::Internal)?,
        replacement: replacement.map(|r| r.evidence),
        estimates: vec![],
        now_ms: time,
        placement,
        route: request.route,
        launches: request.launches,
    };
    Ok((request.project, input))
}
