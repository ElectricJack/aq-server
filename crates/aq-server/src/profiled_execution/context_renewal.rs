//! Renewed context facts. Until the renewal route lands this answers with no pin.
use crate::{AppState, error::AppError};

pub async fn current_pin(
    _: &AppState,
    _: u64,
) -> Result<Option<aq_protocol::QualificationPin>, AppError> {
    Ok(None)
}
