//! Provider capacity for a native offer, within the task's authorized route and level.
use crate::admission::{self, Cell};
use aq_profiles::roles::Launch;
use aq_selection::{
    ConcreteChoice, SelectionError, SelectionInputs, SelectionOutcome,
    select_for_offer_with_capacity, select_for_workspace_environment_with_capacity,
};
use sqlx::PgPool;
use std::collections::BTreeMap;

#[derive(Debug, thiserror::Error)]
pub enum PlacementError {
    #[error(transparent)]
    Selection(#[from] SelectionError),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

pub fn cell(input: &SelectionInputs, choice: &ConcreteChoice) -> Cell {
    Cell {
        provider: choice.provider.clone(),
        intelligence_class: input.project.intelligence_profile.clone(),
    }
}

/// The ordinary ranker owns route preferences and candidate eligibility. On a
/// capacity refusal it ranks the same launch against one snapshot of all limits.
/// The offer transaction independently fences the winner before claiming the task.
pub async fn select(
    pool: &PgPool,
    launch: &Launch<'_>,
    input: &SelectionInputs,
) -> Result<SelectionOutcome, PlacementError> {
    select_with_capacity(pool, launch, input, false).await
}

/// Provider capacity can rerank a workspace launch within its one already
/// admitted environment, retaining all other restrictions.
pub async fn select_for_workspace(
    pool: &PgPool,
    launch: &Launch<'_>,
    input: &SelectionInputs,
) -> Result<SelectionOutcome, PlacementError> {
    select_with_capacity(pool, launch, input, true).await
}

async fn select_with_capacity(
    pool: &PgPool,
    launch: &Launch<'_>,
    input: &SelectionInputs,
    workspace_alternative: bool,
) -> Result<SelectionOutcome, PlacementError> {
    let rank = if workspace_alternative {
        select_for_workspace_environment_with_capacity
    } else {
        select_for_offer_with_capacity
    };
    let mut outcome = rank(launch, input, &BTreeMap::new())?;
    let Some(preferred) = &outcome.resolved else {
        return Ok(outcome);
    };
    let first = cell(input, &preferred.choice);
    let mut tx = pool.begin().await?;
    let Some(capacity) = admission::Capacity::load(&mut tx).await? else {
        tx.commit().await?;
        return Ok(outcome);
    };
    let mut wait = capacity.reason(Some(&first));
    if let Some(first_reason) = wait {
        let mut unavailable = BTreeMap::new();
        for candidate in &launch.candidates {
            let cell = Cell {
                provider: candidate.provider.clone(),
                intelligence_class: first.intelligence_class.clone(),
            };
            if let Some(reason) = capacity.reason(Some(&cell)) {
                unavailable.insert(cell.provider, reason);
            }
        }
        outcome = rank(launch, input, &unavailable)?;
        if outcome.resolved.is_some() {
            wait = None;
        } else {
            // Keep the preferred cell's actionable wait and all per-candidate traces.
            outcome.reason_codes = vec![first_reason.into()];
        }
    }
    admission::record_wait(
        &mut tx,
        input.project.lineage.task_id,
        Some(&first),
        input.now_ms as i64,
        wait,
    )
    .await?;
    tx.commit().await?;
    Ok(outcome)
}
