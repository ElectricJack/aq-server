//! Roles and profiles (spec 0014): a role, an intelligence profile and an
//! execution profile, each one Markdown document, compiled together into an
//! immutable content-addressed [`Snapshot`]. Pure: no database, HTTP or I/O.
//!
//! This module is built alongside the catalog it replaces. The document
//! grammar is specified in the crate README.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

mod defaults;
mod markdown;
mod permissions;
mod selection;
mod startup;
#[cfg(test)]
mod tests;
mod validate;

pub use defaults::{default_documents, platform_text};
pub use markdown::{ParseError, parse_execution, parse_intelligence, parse_role};
pub use permissions::{effective_permissions, requested_permissions};
pub use selection::{Launch, ResolveError, SelectionDefaults, SelectionOverrides};
pub use startup::{StartupError, StartupInput, StartupPart, StartupText, assemble_startup};
pub use validate::{
    DocumentLimits, DocumentSet, Problem, Snapshot, SourceDocument, compile, validate,
};

/// The harnesses an execution profile or candidate may name (spec 0014 §5.3).
pub const HARNESSES: [&str; 4] = ["claude", "codex", "gemini", "opencode"];
/// The one source operation a profile permission names. It is scoped to the
/// project's repository; it is not an agent grant, so `aq-agent-grants` does
/// not list it.
pub const SOURCE_WRITE: &str = "source.write";

/// Whether a role may list `operation`: the agent-grant vocabulary
/// (`aq_agent_grants::op::ALL`) and [`SOURCE_WRITE`].
pub fn operation_known(operation: &str) -> bool {
    aq_agent_grants::grantable(operation) || operation == SOURCE_WRITE
}
/// Whether `harness` accepts the reasoning `effort` of a candidate. This is
/// exactly the table the worker launches with (aq-worker `reasoning.rs`): a
/// value outside it is refused by the worker by name, so a document naming it
/// is refused here. Every harness accepts `default`, its own setting.
pub fn harness_supports(harness: &str, effort: &str) -> bool {
    let supported: &[&str] = match harness {
        "claude" => &["default", "off", "low", "medium", "high", "xhigh", "max"],
        "codex" | "opencode" => &["default", "low", "medium", "high"],
        "gemini" => &["default"],
        _ => return false,
    };
    supported.contains(&effort)
}
/// The Claude harness runs Anthropic models and the Codex harness OpenAI
/// models (the pairing manual sessions already apply). Other harnesses take
/// any provider.
pub fn provider_fits_harness(provider: &str, harness: &str) -> bool {
    match harness {
        "claude" => provider == "anthropic",
        "codex" => provider == "openai",
        _ => true,
    }
}

/// Which of the three documents a text is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Role,
    Intelligence,
    Execution,
}
impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Role => "role",
            Self::Intelligence => "intelligence profile",
            Self::Execution => "execution profile",
        }
    }
}

/// What kind of work the agent does (spec 0014 §5.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Role {
    pub id: String,
    pub name: String,
    pub description: String,
    /// The id of the execution profile this role runs under.
    pub execution: String,
    /// AQ operations, without scope: see [`effective_permissions`].
    pub operations: Vec<String>,
    pub enabled: bool,
    /// The text under `## Role`.
    pub role_md: String,
    /// The text under `## Rules`; empty when the document has none.
    pub rules_md: String,
}

/// One model an intelligence profile may run, with its reasoning effort
/// stated directly (spec 0014 §5.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Candidate {
    pub provider: String,
    pub model: String,
    pub reasoning: String,
    pub harnesses: Vec<String>,
}
impl Candidate {
    /// A stable identity derived from the content, for selection records. It
    /// does not change when candidates are reordered; it is unique within a
    /// valid profile.
    pub fn derived_id(&self) -> String {
        format!("{}:{}:{}", self.provider, self.model, self.reasoning)
    }
}

/// How capable a model the work needs (spec 0014 §5.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Intelligence {
    pub id: String,
    pub name: String,
    pub description: String,
    /// In preference order.
    pub candidates: Vec<Candidate>,
    /// The keys stated under `## Selection`; the rest are the defaults.
    #[serde(default, skip_serializing_if = "SelectionOverrides::is_empty")]
    pub selection: SelectionOverrides,
}

/// The four consistent settings of [`crate::ExecutionConfinement`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Confinement {
    /// The harness always runs in a node-owned sandbox container.
    Container,
    /// A container is permitted, not required.
    ContainerPermitted,
    /// Native confinement only; a container is denied.
    Native,
    /// Native confinement is required.
    NativeRequired,
}
impl Confinement {
    pub const ALL: [Self; 4] = [
        Self::Container,
        Self::ContainerPermitted,
        Self::Native,
        Self::NativeRequired,
    ];
    pub fn name(self) -> &'static str {
        match self {
            Self::Container => "container",
            Self::ContainerPermitted => "container-permitted",
            Self::Native => "native",
            Self::NativeRequired => "native-required",
        }
    }
    /// The value the worker receives.
    pub fn execution_confinement(self) -> crate::ExecutionConfinement {
        let (permit_container, require_native, require_container) = match self {
            Self::Container => (true, false, true),
            Self::ContainerPermitted => (true, false, false),
            Self::Native => (false, false, false),
            Self::NativeRequired => (false, true, false),
        };
        crate::ExecutionConfinement {
            permit_container,
            require_native,
            require_container,
        }
    }
}

/// The limits of one attempt under an execution profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub wall_time_ms: u64,
    pub memory_bytes: u64,
    pub cpu_millis: u64,
    pub process_limit: u64,
    pub startup_context_bytes: u64,
    pub max_output_tokens: u64,
}

/// Where and how the harness runs (spec 0014 §5.3). It names no project and
/// carries no permissions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Execution {
    pub id: String,
    pub name: String,
    pub description: String,
    pub harnesses: Vec<String>,
    pub environment: String,
    pub confinement: Confinement,
    /// Credential profile per harness.
    pub credentials: BTreeMap<String, String>,
    pub limits: Limits,
}
