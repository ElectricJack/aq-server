//! Same-choice continuing authority; no admission replay or provider RPC.
mod health;
mod history;
mod private_read;
pub(crate) use history::latest_renewal_revision_locked;
mod pinned;

fn now(state: &crate::AppState) -> Result<u64, crate::error::AppError> {
    u64::try_from((state.clock)().timestamp_millis())
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| crate::error::AppError::Conflict("invalid current execution clock".into()))
}
mod current;
mod observations;

mod api;
mod service;
pub use api::routes;
pub use service::renew;

pub(crate) use history::{Issuance, issuance_locked};

pub(crate) use current::choice_supported_by_harness;
pub(crate) use health::{
    Evidence as HealthEvidence, State as HealthState, Subject as HealthSubject,
    evaluate as evaluate_health,
};
pub(crate) use private_read::verify as verify_continuing_material_backing;
pub(crate) use private_read::{Fingerprint, verify_phase as verify_phase_material_backing};
