//! The selection policy of a launch (spec 0014 §5.2). One set of defaults
//! serves every intelligence profile; a profile overrides single keys; the
//! output token limit and the required worker capabilities come from the
//! role's execution profile. [`Snapshot::resolve`] puts the three together.
use super::{Candidate, Confinement, Execution, Intelligence, Role, Snapshot};
use crate::{FallbackReason, IntelligenceBudgetPolicy, SelectionPolicy, UnknownCostPolicy};
use aq_session_protocol::{Capability, Text};
use serde::{Deserialize, Deserializer, Serialize};

/// The selection policy every intelligence profile starts from. A Settings
/// value: the store keeps it and passes it to `compile`, and it is part of
/// the snapshot, so a launch resolves from its snapshot alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionDefaults {
    pub max_attempts: u32,
    pub fallback_on: Vec<FallbackReason>,
    pub max_lineage_elapsed_ms: u64,
    pub observation_max_age_ms: u64,
    pub allow_unknown_availability: bool,
    pub unknown_cost: UnknownCostPolicy,
    pub require_complete_accounting: bool,
    pub require_hard_cost_enforcement: bool,
    /// `None` is no cost limit. Goes together with `currency`.
    pub max_cost_microunits: Option<u64>,
    pub currency: Option<String>,
    /// Sandbox time a task's charged attempts may be admitted for in total.
    #[serde(default = "initial_sandbox_ms")]
    pub max_lineage_sandbox_ms: u64,
    /// Owner- or service-stopped attempts (and lost or never-started ones) a
    /// task may have before a further one is charged as a normal attempt.
    #[serde(default = "initial_uncharged")]
    pub max_uncharged_continuations: u32,
    /// Launches (started attempts and failed wake launches) a task may have
    /// spent for a waiting session to still be woken.
    #[serde(default = "initial_wake_attempts")]
    pub max_wake_attempts: u32,
    /// Time since a task's first offer within which a waiting session is
    /// still woken.
    #[serde(default = "initial_wake_elapsed_ms")]
    pub max_wake_lineage_elapsed_ms: u64,
    /// Offers of a task that may expire without ever being granted before the
    /// task is held.
    #[serde(default = "initial_never_granted")]
    pub max_never_granted_offers: u32,
}
fn initial_never_granted() -> u32 {
    SelectionDefaults::default().max_never_granted_offers
}
// Settings stored before these keys existed take the initial values.
fn initial_sandbox_ms() -> u64 {
    SelectionDefaults::default().max_lineage_sandbox_ms
}
fn initial_uncharged() -> u32 {
    SelectionDefaults::default().max_uncharged_continuations
}
fn initial_wake_attempts() -> u32 {
    SelectionDefaults::default().max_wake_attempts
}
fn initial_wake_elapsed_ms() -> u64 {
    SelectionDefaults::default().max_wake_lineage_elapsed_ms
}
impl Default for SelectionDefaults {
    /// The values a new installation starts with.
    fn default() -> Self {
        Self {
            max_attempts: 2,
            fallback_on: vec![
                FallbackReason::MissingCredentials,
                FallbackReason::Unavailable,
                FallbackReason::Unsupported,
                FallbackReason::StaleUnknown,
                FallbackReason::PolicyDenied,
            ],
            max_lineage_elapsed_ms: 3_600_000,
            observation_max_age_ms: 30_000,
            allow_unknown_availability: false,
            unknown_cost: UnknownCostPolicy::AllowBounded,
            require_complete_accounting: false,
            require_hard_cost_enforcement: false,
            max_cost_microunits: None,
            currency: None,
            max_lineage_sandbox_ms: 240 * 60_000,
            max_uncharged_continuations: 5,
            max_wake_attempts: 3,
            max_wake_lineage_elapsed_ms: 24 * 3_600_000,
            max_never_granted_offers: 8,
        }
    }
}

/// A present key, including a present `null`, is stated.
fn stated<'de, T: Deserialize<'de>, D: Deserializer<'de>>(d: D) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}
/// The keys an intelligence profile states under `## Selection`. An absent
/// key takes the default. For the two cost keys `Some(None)` is the document's
/// `none`: no cost limit, whatever the default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_on: Option<Vec<FallbackReason>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_lineage_elapsed_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_max_age_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_unknown_availability: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unknown_cost: Option<UnknownCostPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_complete_accounting: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_hard_cost_enforcement: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "stated"
    )]
    pub max_cost_microunits: Option<Option<u64>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "stated"
    )]
    pub currency: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_lineage_sandbox_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_uncharged_continuations: Option<u32>,
}
impl SelectionOverrides {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
    /// Without the keys that state the default's own value. A snapshot holds
    /// profiles in this form, so `to_markdown` writes only what differs.
    pub fn differing(self, defaults: &SelectionDefaults) -> Self {
        fn keep<T: PartialEq>(stated: Option<T>, default: &T) -> Option<T> {
            stated.filter(|value| value != default)
        }
        Self {
            max_attempts: keep(self.max_attempts, &defaults.max_attempts),
            fallback_on: keep(self.fallback_on, &defaults.fallback_on),
            max_lineage_elapsed_ms: keep(
                self.max_lineage_elapsed_ms,
                &defaults.max_lineage_elapsed_ms,
            ),
            observation_max_age_ms: keep(
                self.observation_max_age_ms,
                &defaults.observation_max_age_ms,
            ),
            allow_unknown_availability: keep(
                self.allow_unknown_availability,
                &defaults.allow_unknown_availability,
            ),
            unknown_cost: keep(self.unknown_cost, &defaults.unknown_cost),
            require_complete_accounting: keep(
                self.require_complete_accounting,
                &defaults.require_complete_accounting,
            ),
            require_hard_cost_enforcement: keep(
                self.require_hard_cost_enforcement,
                &defaults.require_hard_cost_enforcement,
            ),
            max_cost_microunits: keep(self.max_cost_microunits, &defaults.max_cost_microunits),
            currency: keep(self.currency, &defaults.currency),
            max_lineage_sandbox_ms: keep(
                self.max_lineage_sandbox_ms,
                &defaults.max_lineage_sandbox_ms,
            ),
            max_uncharged_continuations: keep(
                self.max_uncharged_continuations,
                &defaults.max_uncharged_continuations,
            ),
        }
    }
}

impl SelectionDefaults {
    /// These defaults with a profile's stated keys in place.
    pub fn with(&self, stated: &SelectionOverrides) -> Self {
        let d = self.clone();
        let s = stated.clone();
        Self {
            max_attempts: s.max_attempts.unwrap_or(d.max_attempts),
            fallback_on: s.fallback_on.unwrap_or(d.fallback_on),
            max_lineage_elapsed_ms: s.max_lineage_elapsed_ms.unwrap_or(d.max_lineage_elapsed_ms),
            observation_max_age_ms: s.observation_max_age_ms.unwrap_or(d.observation_max_age_ms),
            allow_unknown_availability: s
                .allow_unknown_availability
                .unwrap_or(d.allow_unknown_availability),
            unknown_cost: s.unknown_cost.unwrap_or(d.unknown_cost),
            require_complete_accounting: s
                .require_complete_accounting
                .unwrap_or(d.require_complete_accounting),
            require_hard_cost_enforcement: s
                .require_hard_cost_enforcement
                .unwrap_or(d.require_hard_cost_enforcement),
            max_cost_microunits: s.max_cost_microunits.unwrap_or(d.max_cost_microunits),
            currency: s.currency.unwrap_or(d.currency),
            max_lineage_sandbox_ms: s.max_lineage_sandbox_ms.unwrap_or(d.max_lineage_sandbox_ms),
            max_uncharged_continuations: s
                .max_uncharged_continuations
                .unwrap_or(d.max_uncharged_continuations),
            max_wake_attempts: d.max_wake_attempts,
            max_wake_lineage_elapsed_ms: d.max_wake_lineage_elapsed_ms,
            max_never_granted_offers: d.max_never_granted_offers,
        }
    }
    /// The full policy: these values, and from the execution profile the
    /// output token limit and the capabilities a worker must have.
    pub fn policy(
        &self,
        max_output_tokens: u64,
        required_capabilities: Vec<Capability>,
    ) -> SelectionPolicy {
        SelectionPolicy {
            schema_version: 1,
            fallback_on: self.fallback_on.clone(),
            allow_unknown_availability: self.allow_unknown_availability,
            observation_max_age_ms: self.observation_max_age_ms,
            max_output_tokens,
            max_attempts: self.max_attempts,
            max_lineage_elapsed_ms: self.max_lineage_elapsed_ms,
            max_lineage_sandbox_ms: Some(self.max_lineage_sandbox_ms),
            max_uncharged_continuations: Some(self.max_uncharged_continuations),
            required_capabilities,
            budget: IntelligenceBudgetPolicy {
                max_cost_microunits: self.max_cost_microunits,
                currency: self.currency.clone(),
                require_complete_accounting: self.require_complete_accounting,
                require_hard_cost_enforcement: self.require_hard_cost_enforcement,
                unknown_cost: self.unknown_cost,
            },
        }
    }
    /// The rules of [`SelectionPolicy::validate`] that do not depend on an
    /// execution profile.
    pub fn check(&self) -> Result<(), String> {
        if self.max_wake_attempts == 0
            || self.max_wake_lineage_elapsed_ms == 0
            || self.max_never_granted_offers == 0
        {
            return Err("wake and never-granted limits must be above zero".into());
        }
        (self.policy(1, Vec::new()).validate()).map_err(|crate::Error::Invalid(message)| message)
    }
}

impl Execution {
    /// The capabilities a worker must have to run under this profile: the
    /// four of every profiled launch, and container confinement when the
    /// profile requires or permits a container. In the order an offer lists
    /// them.
    pub fn required_capabilities(&self) -> Vec<Capability> {
        let container = matches!(
            self.confinement,
            Confinement::Container | Confinement::ContainerPermitted
        );
        [
            Some("aq.profiled-launch"),
            Some("aq.terminal"),
            Some("aq.harness.startup-context"),
            container.then_some("aq.harness.container-confinement"),
            Some("aq.harness.stop"),
        ]
        .into_iter()
        .flatten()
        .filter_map(|id| Text::new(id).ok())
        .map(|id| Capability { id, version: 1 })
        .collect()
    }
}

/// What one (role, intelligence profile) pair launches with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch<'a> {
    pub role: &'a Role,
    pub intelligence: &'a Intelligence,
    /// The role's execution profile.
    pub execution: &'a Execution,
    /// The profile's candidates in preference order, each with only the
    /// harnesses the execution profile may use; a candidate left with none
    /// is not listed. May be empty.
    pub candidates: Vec<Candidate>,
    /// Defaults, the profile's overrides, and the execution profile's output
    /// token limit and required capabilities.
    pub selection_policy: SelectionPolicy,
}
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    #[error("no role `{0}` exists")]
    NoRole(String),
    #[error("no intelligence profile `{0}` exists")]
    NoIntelligence(String),
    #[error("no execution profile `{0}` exists")]
    NoExecution(String),
}
impl Snapshot {
    /// The launch description of a role on an intelligence profile. A
    /// disabled role resolves too; the caller decides what `enabled` means
    /// for it.
    pub fn resolve(&self, role: &str, intelligence: &str) -> Result<Launch<'_>, ResolveError> {
        let role = self
            .role(role)
            .ok_or_else(|| ResolveError::NoRole(role.into()))?;
        let intelligence = self
            .intelligence(intelligence)
            .ok_or_else(|| ResolveError::NoIntelligence(intelligence.into()))?;
        let execution = self
            .execution(&role.execution)
            .ok_or_else(|| ResolveError::NoExecution(role.execution.clone()))?;
        let candidates = intelligence
            .candidates
            .iter()
            .map(|candidate| Candidate {
                harnesses: candidate
                    .harnesses
                    .iter()
                    .filter(|harness| execution.harnesses.contains(harness))
                    .cloned()
                    .collect(),
                ..candidate.clone()
            })
            .filter(|candidate| !candidate.harnesses.is_empty())
            .collect();
        let selection_policy = self
            .selection_defaults()
            .with(&intelligence.selection)
            .policy(
                execution.limits.max_output_tokens,
                execution.required_capabilities(),
            );
        Ok(Launch {
            role,
            intelligence,
            execution,
            candidates,
            selection_policy,
        })
    }
}
