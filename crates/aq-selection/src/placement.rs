//! Sandbox placement facts (C4): template pin, backend capabilities, node
//! ceilings and placement preference. The server derives them from the node's
//! authenticated B1 advertisement and the catalog's activation-time template
//! resolution; selection only turns them into typed refusals.
use crate::candidate_core::{CandidateResult, profile_key};
use crate::{BackendKind, CandidateState, ConcreteChoice, Digest, InstalledHarness};
use aq_session_protocol::ArtifactRef;
use serde::{Deserialize, Serialize};

/// Present only for nodes that registered a sandbox backend advertisement.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Placement {
    /// Advertised backend capabilities (kebab-case B1 names).
    pub capabilities: Vec<String>,
    /// Capabilities the project requires; any missing one refuses.
    pub required_capabilities: Vec<String>,
    pub templates: Vec<AdvertisedTemplate>,
    /// Activation-time `Execution.environments` entry -> template digest.
    pub environment_templates: Vec<EnvironmentTemplate>,
    pub ceiling: Option<NodeCeiling>,
    /// A higher-preference eligible backend is online with free capacity.
    pub preferred_peer: Option<BackendKind>,
    /// The node's advertised sandbox backend; drives confinement qualification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<BackendKind>,
}

/// Harness confinement the frozen launch manifest requires (exactly one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfinementMode {
    /// `aq.harness.confined-worktree`
    Native,
    /// `aq.harness.container-confinement`
    Container,
}
impl ConfinementMode {
    pub fn capability(self) -> &'static str {
        match self {
            ConfinementMode::Native => "aq.harness.confined-worktree",
            ConfinementMode::Container => "aq.harness.container-confinement",
        }
    }
}

/// Per template x backend confinement qualification, mirroring aq-worker
/// `QualificationRecord`: docker-local is qualified container-only for every
/// template; other backends have no container evidence and run native.
pub fn container_only(backend: Option<BackendKind>) -> bool {
    backend == Some(BackendKind::DockerLocal)
}

/// The adapters whose harness always runs in a node-owned sandbox container
/// (MS1: Claude Code driven in a docker-local sandbox by the node).
pub const NODE_SANDBOX_ADAPTER: &str = "aq.claude-result-local-adapter";
/// The docker-local Codex counterpart (aq-worker W1). The interactive Codex
/// adapter `aq.codex-result-local-adapter` is not a sandbox adapter.
pub const NODE_SANDBOX_CODEX_ADAPTER: &str = "aq.codex-docker-result-local-adapter";

/// Whether `adapter` is a node sandbox adapter (Claude or Codex).
pub fn is_node_sandbox_adapter(adapter: &str) -> bool {
    adapter == NODE_SANDBOX_ADAPTER || adapter == NODE_SANDBOX_CODEX_ADAPTER
}

/// `require_container` (MS1) bypasses B1 placement facts, so it is admitted only
/// for a node-sandbox adapter and only when the node's authenticated
/// capability snapshot advertises container confinement for that harness.
fn require_container(
    profile: &aq_profiles::ExecutionConfinement,
    harness: &crate::InstalledHarness,
) -> Result<ConfinementMode, (CandidateState, &'static str)> {
    if profile.require_native || !profile.permit_container {
        return Err((
            CandidateState::PolicyDenied,
            "execution_confinement_contradictory",
        ));
    }
    if !is_node_sandbox_adapter(harness.adapter.id.as_str()) {
        return Err((
            CandidateState::Unsupported,
            "container_required_adapter_unqualified",
        ));
    }
    let container = ConfinementMode::Container.capability();
    if !harness
        .capabilities
        .iter()
        .any(|c| c.id.as_str() == container && c.version == 1)
    {
        return Err((
            CandidateState::Unsupported,
            "container_confinement_not_advertised",
        ));
    }
    Ok(ConfinementMode::Container)
}

/// Admits the execution profile's confinement permission on the placement's
/// backend before placement. A native-required profile is refused on a
/// container-only backend, and container confinement needs explicit permit.
pub fn admit_confinement(
    placement: Option<&Placement>,
    profile: &aq_profiles::ExecutionConfinement,
    harness: &crate::InstalledHarness,
) -> Result<ConfinementMode, (CandidateState, &'static str)> {
    if profile.require_container {
        return require_container(profile, harness);
    }
    if !container_only(placement.and_then(|p| p.backend)) {
        return Ok(ConfinementMode::Native);
    }
    if profile.require_native {
        return Err((
            CandidateState::Unsupported,
            "execution_requires_native_confinement",
        ));
    }
    if !profile.permit_container {
        return Err((
            CandidateState::PolicyDenied,
            "container_confinement_not_permitted",
        ));
    }
    Ok(ConfinementMode::Container)
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AdvertisedTemplate {
    pub digest: Digest,
    pub harnesses: Vec<AdvertisedHarness>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AdvertisedHarness {
    pub harness: String,
    pub version: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentTemplate {
    pub environment: String,
    pub template: Digest,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NodeCeiling {
    Concurrency,
    Spend,
    /// A spend ceiling is configured but no comparable spend is accounted.
    SpendUnaccounted,
    /// The node's measured disk usage is above the placement refusal line.
    DiskPressure,
}
impl NodeCeiling {
    pub fn reason(self) -> &'static str {
        match self {
            NodeCeiling::Concurrency => "node_concurrency_ceiling_reached",
            NodeCeiling::Spend => "node_spend_ceiling_reached",
            NodeCeiling::SpendUnaccounted => "node_spend_unaccounted",
            NodeCeiling::DiskPressure => "node_disk_pressure",
        }
    }
}

pub(crate) fn check(placement: &Placement, harness: &InstalledHarness) -> CandidateResult {
    check_parts(
        placement,
        &harness.harness,
        &harness.cli_version,
        &harness.environment,
    )
}

/// Fence-time re-check of a frozen selection against freshly derived placement
/// facts. It applies the same checks, in the same order and with the same typed
/// reason codes, as selection.
pub fn recheck(placement: &Placement, choice: &ConcreteChoice) -> Result<(), &'static str> {
    check_parts(
        placement,
        &choice.harness,
        &choice.cli_version,
        &choice.environment,
    )
    .map_err(|(_, reason)| reason)
}

fn check_parts(
    placement: &Placement,
    harness: &str,
    cli_version: &str,
    environment: &ArtifactRef,
) -> CandidateResult {
    check_availability(placement)?;
    let unsupported = |reason| Err((CandidateState::Unsupported, reason));
    let key = profile_key(environment);
    let Some(resolved) = placement
        .environment_templates
        .iter()
        .find(|t| t.environment == key)
        .or_else(|| {
            placement
                .environment_templates
                .iter()
                .find(|t| t.environment == environment.id.as_str())
        })
    else {
        return unsupported("environment_template_unresolved");
    };
    if resolved.template != environment.digest {
        return unsupported("environment_template_digest_mismatch");
    }
    let Some(template) = placement
        .templates
        .iter()
        .find(|t| t.digest == resolved.template)
    else {
        return unsupported("template_not_advertised");
    };
    if !template
        .harnesses
        .iter()
        .any(|h| h.harness == harness && h.version == cli_version)
    {
        return unsupported("template_missing_selected_harness_version");
    }
    Ok(())
}

fn check_availability(placement: &Placement) -> CandidateResult {
    if let Some(ceiling) = placement.ceiling {
        return Err((CandidateState::Unavailable, ceiling.reason()));
    }
    if placement.preferred_peer.is_some() {
        return Err((
            CandidateState::Unavailable,
            "placement_preferred_backend_available",
        ));
    }
    let unsupported = |reason| Err((CandidateState::Unsupported, reason));
    if placement
        .required_capabilities
        .iter()
        .any(|c| !placement.capabilities.contains(c))
    {
        return unsupported("required_backend_capability_unsupported");
    }
    Ok(())
}
