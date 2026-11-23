//! Actual profiled state representation. It is deliberately separate from the
//! legacy fixture wire/state. No HTTP handler accepts this stored projection.
pub mod authority_cache;
pub mod graph_completion;
pub mod hot;
pub mod inline_artifact;
pub mod reconciliation;
pub mod source_export;
use aq_session_protocol::{ArtifactRef, ProfiledGrant, ProfiledOffer, Validate};
use aq_worker_protocol::{GraphBinding, SourceBinding};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Offered,
    Preparing,
    Granted,
    Cancelling,
    Finished,
    Declined,
    Unknown,
    NeverGranted,
}
impl Status {
    /// Whether an attempt in this state may write a refreshed subscription login
    /// back to the login store (its material lease is checked separately). A live
    /// attempt may. One exception to "a revoked attempt gets nothing": a granted
    /// attempt that was cancelled (`Cancelling`) may still return the login its
    /// sandbox refreshed, until closure revokes the lease. The write-back grants the
    /// attempt nothing; without it a refresh inside a sandbox that is then cancelled
    /// would rotate the token at the provider and strand the stored login. A stopped
    /// attempt, and one cancelled before it was granted (no sandbox ran), may not.
    pub fn login_write_back(self, revoked: bool, cancelled: bool) -> bool {
        match self {
            Self::Preparing | Self::Granted => !revoked && !cancelled,
            Self::Cancelling => true,
            _ => false,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Stop {
    Unknown,
    Running,
    NotStarted { receipt: ArtifactRef },
    Stopped { receipt: ArtifactRef },
}
impl Stop {
    fn receipt(&self) -> Option<&ArtifactRef> {
        match self {
            Self::NotStarted { receipt } | Self::Stopped { receipt } => Some(receipt),
            _ => None,
        }
    }
    pub fn known_stopped(&self) -> bool {
        matches!(self, Self::NotStarted { .. } | Self::Stopped { .. })
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Retention {
    Unknown,
    Pending,
    NotRequired,
    Retained { receipt: ArtifactRef },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Effects {
    Unknown,
    Pending { count: u32 },
    Quiescent { receipt: ArtifactRef },
}
impl Retention {
    fn receipt(&self) -> Option<&ArtifactRef> {
        if let Self::Retained { receipt } = self {
            Some(receipt)
        } else {
            None
        }
    }
}
impl Effects {
    fn receipt(&self) -> Option<&ArtifactRef> {
        if let Self::Quiescent { receipt } = self {
            Some(receipt)
        } else {
            None
        }
    }
}
/// Receipt references are admitted only by their owning runtime validators.
/// Deserialization by itself does not verify a receipt or release a task claim.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfiledState {
    pub schema_version: u32,
    pub offer: ProfiledOffer,
    pub status: Status,
    pub grant: Option<ProfiledGrant>,
    pub execution_deadline_ms: Option<u64>,
    pub source: SourceBinding,
    pub graph: GraphBinding,
    pub cancellation_id: Option<Uuid>,
    pub revoked: bool,
    pub stop: Stop,
    pub retention: Retention,
    pub effects: Effects,
}
fn reference(value: &ArtifactRef) -> Result<(), &'static str> {
    value.id.validate().map_err(|_| "receipt identity")?;
    value.version.validate().map_err(|_| "receipt version")?;
    Ok(())
}
impl ProfiledState {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.status == Status::NeverGranted {
            return Err("never-granted closure requires adopted central owner proof");
        }
        if self.schema_version != 1 {
            return Err("profiled state version");
        }
        self.validate_bindings()?;
        self.validate_grant()?;
        self.validate_receipts()
    }
    fn validate_bindings(&self) -> Result<(), &'static str> {
        self.offer.validate().map_err(|_| "profiled offer")?;
        if !matches!(
            self.offer.binding.session,
            aq_session_protocol::SessionIdentity::Remote { .. }
        ) {
            return Err("remote profiled state needs remote authority");
        }
        aq_worker_protocol::Validate::validate(&self.source).map_err(|_| "source binding")?;
        aq_worker_protocol::Validate::validate(&self.graph).map_err(|_| "graph binding")?;
        let profile = &self.offer.manifest.bindings.graph_access_profile;
        let graph_matches = self.offer.manifest.bindings.graph_id == self.graph.workspace_id
            && profile.id.as_str() == self.graph.profile_id
            && profile.version.as_str() == self.graph.profile_revision
            && self.offer.binding.graph_access_profile_digest.as_str() == self.graph.profile_digest;
        if !graph_matches {
            return Err("graph binding mismatch");
        }
        Ok(())
    }
    fn validate_grant(&self) -> Result<(), &'static str> {
        if self.cancellation_id.is_some_and(|id| id.is_nil()) {
            return Err("nil cancellation");
        }
        if let Some(grant) = &self.grant {
            grant.validate().map_err(|_| "profiled grant")?;
            let in_window = self
                .execution_deadline_ms
                .is_some_and(|d| d != 0 && grant.expires_at_ms <= d);
            if grant.binding != self.offer.binding || !in_window {
                return Err("grant binding/deadline");
            }
        }
        if self.status == Status::Granted && !self.has_live_grant() {
            return Err("inconsistent granted authority");
        }
        Ok(())
    }
    fn has_live_grant(&self) -> bool {
        self.grant.is_some()
            && !self.revoked
            && self.cancellation_id.is_none()
            && !self.stop.known_stopped()
    }
    fn validate_receipts(&self) -> Result<(), &'static str> {
        match &self.stop {
            Stop::NotStarted { receipt } | Stop::Stopped { receipt } => reference(receipt)?,
            _ => {}
        }
        match &self.retention {
            Retention::Retained { receipt } => reference(receipt)?,
            Retention::NotRequired if !matches!(self.stop, Stop::NotStarted { .. }) => {
                return Err("retention not required only with verified nonstart");
            }
            _ => {}
        }
        match &self.effects {
            Effects::Quiescent { receipt } => reference(receipt)?,
            Effects::Pending { count } if *count == 0 => return Err("empty pending effects"),
            _ => {}
        }
        Ok(())
    }
    /// Projection only: the state producer must verify real owning-domain
    /// receipts before using this to clear holds_task. References are not proof.
    pub fn release_ready(&self) -> bool {
        self.stop.known_stopped()
            && matches!(
                self.retention,
                Retention::Retained { .. } | Retention::NotRequired
            )
            && matches!(self.effects, Effects::Quiescent { .. })
    }
}
pub mod claim;
pub mod default_policy;
pub mod effect_profile;
pub use effect_profile as effect_ledger;
pub mod fairness;
pub mod git_author;
pub mod native_offer;
pub mod replacement;
pub mod replacement_sql;
pub mod result_facts;
pub mod result_reads;
/// The body of `FUNCTION <name>(…) … AS $$<body>$$;` in a migration's SQL (the schema
/// attestation compares it with the installed function's source).
pub fn sql_function_body<'a>(sql: &'a str, name: &str) -> Option<&'a str> {
    let (_, tail) = sql.split_once(&format!("FUNCTION {name}("))?;
    let (_, tail) = tail.split_once("AS $$")?;
    tail.split_once("$$;").map(|(body, _)| body)
}

#[cfg(test)]
mod login_write_back_tests {
    use super::Status::*;
    #[test]
    fn a_live_attempt_and_a_cancelled_granted_one_may_write_a_login_back() {
        for live in [Preparing, Granted] {
            assert!(live.login_write_back(false, false));
            assert!(
                !live.login_write_back(true, true),
                "cancelled before a grant"
            );
            assert!(!live.login_write_back(true, false));
            assert!(!live.login_write_back(false, true));
        }
        assert!(Cancelling.login_write_back(true, true));
        for over in [Offered, Finished, Declined, Unknown, NeverGranted] {
            assert!(!over.login_write_back(false, false) && !over.login_write_back(true, true));
        }
    }
}
