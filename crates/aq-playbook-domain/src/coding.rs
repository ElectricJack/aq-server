//! D9 default coding policy, sealed from the pinned Host8 (catalog 2.1.0)
//! command contracts only: `work.ensure`, `check.request`,
//! `candidate.prepare`, `candidate.publish` and the `operation.get`
//! reconciliation command their asynchronous completions name.
//!
//! The policy is four artifacts. Bind names are unique and there is no
//! assignment, so the sync/async completion paths and the failure origins
//! converge through `Call` children whose outcome output is uniform:
//!
//! - `aq.coding.work`: one `work.ensure` (sync `completed`, or `accepted` then
//!   the `aq.operation.updated` completion) → `accepted` / `rejected` /
//!   `escalated`.
//! - `aq.coding.check`: one `check.request` (sync, or `accepted` then the
//!   `aq.check.receipted` receipt) → `passed` / `failed` / `escalated`.
//! - `aq.coding.attempt`: work → `candidate.prepare` → every required check on
//!   the prepared candidate → cross-family review (`work.ensure kind=review`) →
//!   `candidate.publish` with the check and review evidence → `published`, or
//!   `rework{reference}` naming the failing evidence.
//! - `aq.coding.decision` (only when the policy asks for a human decision):
//!   one `decision.request` on the reviewed candidate (sync, or `accepted` then
//!   the `aq.decision.completed` completion) → `approved` / `rejected` /
//!   `escalated`. It runs after, and in addition to, the review.
//! - `aq.coding`: the implementation attempt, then up to `max_repairs` repair
//!   attempts, each handed the previous failing evidence reference as context.
//!
//! Evidence is bound to the prepared candidate subject, so a new candidate
//! (any subject change) runs every check and the review again. Review output
//! is published as review evidence next to, never instead of, the check
//! receipts; the host's publication guard verifies each in its owning store.
use super::review_domain::{CatalogProfile, HOST7_CAPABILITIES};
use aq_playbook_host::model::*;
use aq_playbooks_compiler::draft::retain_registry;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub const ROOT: &str = "aq.coding";
pub const ATTEMPT: &str = "aq.coding.attempt";
pub const WORK: &str = "aq.coding.work";
pub const CHECK: &str = "aq.coding.check";
pub const DECISION: &str = "aq.coding.decision";
/// The grant capability the optional human decision step requires.
pub const DECISION_CAPABILITY: &str = "decision.request";
const COMPILER: &str = "aq-server D9 coding policy builder 1";
/// Bounded rework budget: the root is unrolled, one attempt step per budget.
pub const MAX_REPAIRS: usize = 8;
const WORK_WAIT_MS: u64 = 43_200_000;
const CHECK_WAIT_MS: u64 = 3_600_000;
/// `aq.decision.completed` retention bounds the wait (24h).
const DECISION_WAIT_MS: u64 = 86_400_000;
const DECISION_CHOICES: [&str; 2] = ["approved", "rejected"];
const TEXT: usize = 4096;

/// One required check, pinned to an exact validator.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequiredCheck {
    pub id: String,
    pub validator_id: String,
    pub validator_version: String,
    pub validator_digest: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodingPolicy {
    pub implementer_family: String,
    pub reviewer_family: String,
    pub checks: Vec<RequiredCheck>,
    pub max_repairs: usize,
    /// Optional human decision after review, before publish. It needs the
    /// `decision.request` grant capability; `build` refuses it otherwise and
    /// never substitutes the review for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub human_decision: Option<HumanDecision>,
}

/// The human decision asked of `responders` on each reviewed candidate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HumanDecision {
    pub question: String,
    pub responders: Vec<String>,
}

/// One coding run's request; `input` turns it into the root entry input.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodingRequest {
    /// The work item (task) subject the implementation is ensured for.
    pub subject: Value,
    pub context: String,
    pub base: Value,
    pub destination: Value,
    /// Run input `source` (`{format, hex}`), still admitted for request
    /// compatibility. Checks on the prepared candidate bind
    /// `candidate.source` (catalog 2.1.0), never this value.
    pub source: Value,
    pub visibility: String,
    /// Prefix of the per-attempt idempotent logical keys.
    pub key: String,
    /// Absolute expiry of each human decision; required (and only used) when
    /// the policy has a `human_decision`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_expires_at_ms: Option<u64>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CodingError {
    #[error("implementer and reviewer must be different model families")]
    SameFamily,
    #[error("coding policy invalid: {0}")]
    Invalid(String),
    #[error("coding policy requires ungranted capability {0}")]
    NotGranted(String),
}

/// The sealed artifacts (four, or five with a human decision), in install
/// order (children first).
#[derive(Clone, Debug)]
pub struct CodingArtifacts {
    pub work: Artifact,
    pub check: Artifact,
    pub decision: Option<Artifact>,
    pub attempt: Artifact,
    pub root: Artifact,
}
impl CodingArtifacts {
    pub fn install_order(&self) -> Vec<&Artifact> {
        [&self.work, &self.check]
            .into_iter()
            .chain(self.decision.as_ref())
            .chain([&self.attempt, &self.root])
            .collect()
    }
}

impl CodingPolicy {
    /// The project default: Claude implements, a different family reviews.
    pub fn project_default(checks: Vec<RequiredCheck>) -> Self {
        Self {
            implementer_family: "anthropic".into(),
            reviewer_family: "openai".into(),
            checks,
            max_repairs: 2,
            human_decision: None,
        }
    }

    /// Builds against the Host8 grant ceiling (the Host7 capability set): every
    /// capability a grant can hold (since D11 including `decision.request`).
    pub fn build(&self) -> Result<CodingArtifacts, CodingError> {
        let ceiling: BTreeSet<String> = HOST7_CAPABILITIES.iter().map(|c| c.to_string()).collect();
        self.build_for(&ceiling)
    }

    /// Builds for a grant holding `granted`; refuses a policy whose artifacts
    /// need a capability outside it.
    pub fn build_for(&self, granted: &BTreeSet<String>) -> Result<CodingArtifacts, CodingError> {
        self.validate(granted)?;
        let b = Builder::new();
        let work = b.work()?;
        let check = b.check()?;
        let decision = match &self.human_decision {
            Some(_) => Some(b.decision()?),
            None => None,
        };
        let children: Vec<_> = [&work, &check]
            .into_iter()
            .chain(decision.as_ref())
            .collect();
        let attempt = b.attempt(self, &children)?;
        let root = b.root(self, &attempt)?;
        if let Some(missing) = root
            .content
            .playbook
            .required_capabilities
            .difference(granted)
            .next()
        {
            return Err(CodingError::NotGranted(missing.clone()));
        }
        Ok(CodingArtifacts {
            work,
            check,
            decision,
            attempt,
            root,
        })
    }

    fn validate(&self, granted: &BTreeSet<String>) -> Result<(), CodingError> {
        self.validate_families()?;
        if self.max_repairs > MAX_REPAIRS || self.checks.len() > 32 {
            return Err(CodingError::Invalid("budget exceeds bound".into()));
        }
        let ids: BTreeSet<_> = self.checks.iter().map(|check| &check.id).collect();
        if ids.len() != self.checks.len() {
            return Err(CodingError::Invalid("duplicate check id".into()));
        }
        if let Some(decision) = &self.human_decision {
            decision.validate(granted)?;
        }
        Ok(())
    }

    fn validate_families(&self) -> Result<(), CodingError> {
        let implementer = self.implementer_family.trim().to_ascii_lowercase();
        let reviewer = self.reviewer_family.trim().to_ascii_lowercase();
        if implementer.is_empty() || reviewer.is_empty() {
            return Err(CodingError::Invalid("model family required".into()));
        }
        if implementer == reviewer {
            return Err(CodingError::SameFamily);
        }
        Ok(())
    }

    /// Root entry input for one coding run.
    pub fn input(&self, r: &CodingRequest) -> Value {
        let keys: serde_json::Map<String, Value> = (0..=self.max_repairs)
            .map(|i| {
                (
                    format!("a{i}"),
                    json!({"work": format!("{}/a{i}/work", r.key),
                           "review": format!("{}/a{i}/review", r.key)}),
                )
            })
            .collect();
        let mut input = json!({"subject": r.subject, "context": r.context, "base": r.base,
               "destination": r.destination, "source": r.source,
               "visibility": r.visibility, "keys": keys});
        if self.human_decision.is_some() {
            // A missing expiry is null, which admission refuses.
            input["decision_expires_at_ms"] = json!(r.decision_expires_at_ms);
        }
        input
    }

    fn review_context(&self) -> String {
        format!(
            "Cross-family review of the prepared candidate. reviewer_family={}; \
             implementer_family={}. Approve only a correct change; a review never \
             substitutes for a required check or a human decision.",
            self.reviewer_family.trim(),
            self.implementer_family.trim()
        )
    }
}

impl HumanDecision {
    fn validate(&self, granted: &BTreeSet<String>) -> Result<(), CodingError> {
        if !granted.contains(DECISION_CAPABILITY) {
            return Err(CodingError::NotGranted(DECISION_CAPABILITY.into()));
        }
        if !Self::valid_text(&self.question) || !self.valid_responders() {
            return Err(CodingError::Invalid("human decision".into()));
        }
        Ok(())
    }

    fn valid_text(value: &str) -> bool {
        !value.trim().is_empty() && value.len() <= TEXT
    }

    fn valid_responders(&self) -> bool {
        if !(1..=100).contains(&self.responders.len()) {
            return false;
        }
        let distinct = self.responders.iter().collect::<BTreeSet<_>>();
        distinct.len() == self.responders.len()
            && self.responders.iter().all(|r| Self::valid_text(r))
    }
}

mod attempt;
mod builder;
mod children;
mod expressions;
pub mod pilots;
mod root;
mod sealing;
use builder::{Builder, CompletionWait};
use expressions::*;
use sealing::ArtifactDraft;
