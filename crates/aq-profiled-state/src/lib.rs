//! Actual profiled state representation. It is deliberately separate from the
//! legacy fixture wire/state. No HTTP handler accepts this stored projection.
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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Stop {
    Unknown,
    Running,
    NotStarted { receipt: ArtifactRef },
    Stopped { receipt: ArtifactRef },
}
impl Stop {
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
        self.offer.validate().map_err(|_| "profiled offer")?;
        if !matches!(
            self.offer.binding.session,
            aq_session_protocol::SessionIdentity::Remote { .. }
        ) {
            return Err("remote profiled state needs remote authority");
        }
        aq_worker_protocol::Validate::validate(&self.source).map_err(|_| "source binding")?;
        aq_worker_protocol::Validate::validate(&self.graph).map_err(|_| "graph binding")?;
        if self.offer.manifest.bindings.graph_id != self.graph.workspace_id
            || self
                .offer
                .manifest
                .bindings
                .graph_access_profile
                .id
                .as_str()
                != self.graph.profile_id
            || self
                .offer
                .manifest
                .bindings
                .graph_access_profile
                .version
                .as_str()
                != self.graph.profile_revision
            || self.offer.binding.graph_access_profile_digest.as_str() != self.graph.profile_digest
        {
            return Err("graph binding mismatch");
        }
        if self.cancellation_id.is_some_and(|id| id.is_nil()) {
            return Err("nil cancellation");
        }
        if let Some(grant) = &self.grant {
            grant.validate().map_err(|_| "profiled grant")?;
            if grant.binding != self.offer.binding
                || self
                    .execution_deadline_ms
                    .is_none_or(|d| d == 0 || grant.expires_at_ms > d)
            {
                return Err("grant binding/deadline");
            }
        }
        if self.status == Status::Granted
            && (self.grant.is_none()
                || self.revoked
                || self.cancellation_id.is_some()
                || self.stop.known_stopped())
        {
            return Err("inconsistent granted authority");
        }
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
pub mod fairness;
pub mod replacement;
pub mod replacement_sql;
/// The body of `FUNCTION <name>(…) … AS $$<body>$$;` in a migration's SQL (the schema
/// attestation compares it with the installed function's source).
pub fn sql_function_body<'a>(sql: &'a str, name: &str) -> Option<&'a str> {
    let (_, tail) = sql.split_once(&format!("FUNCTION {name}("))?;
    let (_, tail) = tail.split_once("AS $$")?;
    tail.split_once("$$;").map(|(body, _)| body)
}
