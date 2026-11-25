//! Provider capacity for a native offer, within the task's authorized route and level.
use crate::admission::{self, Cell};
use aq_profiles::roles::Launch;
use aq_selection::{
    ConcreteChoice, SelectionError, SelectionInputs, SelectionOutcome, select_for_offer,
    select_for_offer_with_capacity,
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
    let mut outcome = select_for_offer(launch, input)?;
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
        outcome = select_for_offer_with_capacity(launch, input, &unavailable)?;
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
