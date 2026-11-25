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

/// Check only providers the ordinary ranker would actually choose. All checks
/// and the final wait projection share the settings lock. The offer transaction
/// independently fences the winner before claiming the task.
pub async fn select(
    pool: &PgPool,
    launch: &Launch<'_>,
    input: &SelectionInputs,
) -> Result<SelectionOutcome, PlacementError> {
    let mut outcome = select_for_offer(launch, input)?;
    let Some(preferred) = &outcome.resolved else {
        return Ok(outcome);
    };
    let task = input.project.lineage.task_id;
    let first = cell(input, &preferred.choice);
    let mut tx = pool.begin().await?;
    let wait = admission::observe(&mut tx, task, Some(&first), input.now_ms as i64).await?;
    if let Some(first_reason) = wait {
        let mut unavailable = BTreeMap::from([(first.provider, first_reason)]);
        loop {
            outcome = select_for_offer_with_capacity(launch, input, &unavailable)?;
            let Some(next) = &outcome.resolved else {
                // Keep the preferred cell's actionable wait. Per-candidate reasons
                // in the traces retain other refusals and any fallback restrictions.
                outcome.reason_codes = vec![first_reason.into()];
                break;
            };
            let next = cell(input, &next.choice);
            match admission::check(&mut tx, Some(&next)).await? {
                Some(reason) => {
                    unavailable.insert(next.provider, reason);
                }
                None => {
                    admission::clear_waiting(&mut tx, task).await?;
                    break;
                }
            }
        }
    }
    tx.commit().await?;
    Ok(outcome)
}
