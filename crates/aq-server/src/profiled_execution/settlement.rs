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
pub mod publication;
pub use automatic::{
    AutoDisposition, AutoSettlement, ReplacementFacts, record_replacement_facts,
    reworked_without_facts, settle_after_record,
};
#[cfg(feature = "delivery_playbook")]
pub use automatic::{complete_by_playbook, return_by_playbook};

/// The review requirement an offer must name for this validation policy body:
/// D3's default completion policy is settled by the policy actor, every other
/// policy by the human owner. Naming the policy actor removes the human review
/// fallback for default-policy tasks: a held or reworked result has no human
/// approval path (MS1 review; a later follow-up, no code change now).
pub(crate) fn review_requirement(policy_body: &str) -> aq_admission::outcome::ReviewRequirement {
    if default_policy::parse(policy_body).is_some() {
        aq_admission::outcome::ReviewRequirement::PolicyActor
    } else {
        aq_admission::outcome::ReviewRequirement::HumanOwner
    }
}
pub use completion::{CompletionReceipt, CompletionStatus};
pub use owner::{EvaluateRequest, EvaluationDisposition, EvaluationReceipt, Service};
