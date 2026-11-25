//! Actual finite settlement owners and data-only eligibility evaluation.
//! No router, scheduler or public capability is enabled by module registration.
//! Schema39 and positive owner qualification remain explicit rollout gates.
pub(crate) mod actor;
pub(crate) mod applicability;
pub mod effects;
pub mod eligibility;

mod automatic;
mod completion;
use aq_profiled_state::default_policy;
mod facts;
#[cfg(feature = "delivery_playbook")]
pub(crate) use facts::retained_bundle_head;
mod owner;
pub mod publication;
#[cfg(feature = "delivery_playbook")]
pub(crate) use automatic::recover_held_for_retry_exact;
pub use automatic::settle_after_record;
pub use automatic::{
    AutoDisposition, AutoSettlement, ReplacementFacts, record_replacement_facts,
    reworked_without_facts,
};
#[cfg(feature = "delivery_playbook")]
pub use automatic::{complete_by_playbook, recover_held_for_retry, return_by_playbook};
#[cfg(feature = "delivery_playbook")]
pub use facts::record_facts;

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

#[cfg(test)]
#[test]
fn default_policy_offers_name_the_policy_actor() {
    use aq_admission::outcome::ReviewRequirement::{HumanOwner, PolicyActor};
    let policy =
        r#"{"schema_version":1,"kind":"aq.default-completion.v1","delivery":"not_required"}"#;
    assert_eq!(review_requirement(policy), PolicyActor);
    let other = r#"{"schema_version":1,"kind":"source_change_reviewed_human_owner_no_checks_v1"}"#;
    assert_eq!(review_requirement(other), HumanOwner);
}
