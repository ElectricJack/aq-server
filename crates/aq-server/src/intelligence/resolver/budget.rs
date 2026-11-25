//! Read lineage counts and spend under their respective storage owners.
use super::{Result, *};
use aq_project_policy::decision::Accounting;
pub(super) async fn attempts(
    state: &AppState,
    lineage: &TaskLineage,
) -> Result<crate::profiled_execution::no_grant::AttemptCounts> {
    use crate::profiled_execution::no_grant;
    no_grant::check_retry_bound(no_grant::attempt_counts(&state.store.pool, lineage.task_id).await?)
}

pub(super) async fn accounting(state: &AppState, request: &ResolveRequest) -> Result<Accounting> {
    let ceiling = &request.project.project.binding.ceilings;
    let soft = ceiling
        .soft_cost_limit()
        .map(
            |(limit_microunits, currency)| crate::observations::budget::SoftCeiling {
                limit_microunits,
                currency: currency.to_owned(),
            },
        );
    let (pool, task) = (&state.store.pool, request.lineage.task_id);
    let cost_baseline = aq_machine_control::holds::cost_baseline(pool, task).await;
    let cost_baseline = cost_baseline.map_err(|_| AppError::Internal)?;
    let ledger = match aq_admission::transport::task_lineage_id(
        request.lineage.project_id,
        request.lineage.workspace_id,
        request.lineage.task_id,
    ) {
        Ok(lineage) => crate::observations::budget::lineage(state, lineage, soft.as_ref())
            .await
            .ok()
            .flatten()
            .map(|budget| budget.snapshot()),
        Err(_) => None,
    };
    Ok(Accounting {
        ledger,
        cost_baseline,
    })
}
