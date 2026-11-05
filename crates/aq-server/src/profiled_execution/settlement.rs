//! Actual finite settlement owners and data-only eligibility evaluation.
//! No router, scheduler or public capability is enabled by module registration.
//! Schema39 and positive owner qualification remain explicit rollout gates.
pub(crate) mod actor;
pub(crate) mod applicability;
pub mod effects;
pub mod eligibility;

mod automatic;
mod completion;
mod default_policy;
mod owner;
pub use automatic::{AutoDisposition, AutoSettlement, settle_after_record};
pub use completion::{CompletionReceipt, CompletionStatus};
pub use owner::{EvaluateRequest, EvaluationDisposition, EvaluationReceipt, Service};
