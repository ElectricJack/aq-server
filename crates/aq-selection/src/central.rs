//! Pure taskless central selection. Data pins are never authenticated authority.
mod pins;
mod types;
mod validate;
use crate::{candidate_core::*, validate::fresh, *};
use serde::{Deserialize, Serialize};
pub use types::*;
pub const MAX_INPUT_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_OUTPUT_BYTES: usize = 256 * 1024;
fn refused(reason: &str) -> CentralSelectionOutcome {
    CentralSelectionOutcome {
        resolved: None,
        traces: vec![],
        reason_codes: vec![reason.into()],
        accounting: AccountingState::Unavailable,
        token_count: (),
        spent_microunits: (),
        authority: AuthorityState::None,
    }
}
/// Owner facts remain untrusted data here; a current owner must authenticate them.
pub fn select_central(
    catalog: &aq_profiles::ValidatedCatalog,
    input: &CentralSelectionInputs,
) -> Result<CentralSelectionOutcome> {
    let validated = validate::validate(catalog, input)?;
    let Some(policy) = &validated.intelligence.selection_policy else {
        return Ok(refused("selection_policy_missing"));
    };
    let turn = &input.policy.value;
    let reg = &input.registry.value;
    let inst = &input.installation.value;
    let global = if !input.reservation.value.policy.value.enabled || !turn.enabled {
        Some("central_policy_disabled")
    } else if input.now_ms >= input.original_expires_at_ms {
        Some("central_original_deadline_elapsed")
    } else if !fresh(reg.observed_at_ms, reg.expires_at_ms, input.now_ms) {
        Some("central_registry_snapshot_stale")
    } else if policy.max_output_tokens > turn.max_output_tokens {
        Some("central_output_ceiling_conflict")
    } else if !turn.allow_unknown_cost
        || turn.ceilings.cost_limit_microunits.is_some()
        || turn.ceilings.require_hard_cost_enforcement
        || policy.budget.max_cost_microunits.is_some()
        || policy.budget.require_complete_accounting
        || policy.budget.require_hard_cost_enforcement
        || policy
            .required_capabilities
            .iter()
            .any(|c| c.id.as_str() == "aq.budget.task-cost")
    {
        Some("central_cost_accounting_unavailable")
    } else {
        None
    };
    let compositions: Vec<_> = input
        .compositions
        .iter()
        .map(|b| CoreComposition {
            digest: &b.artifact.reference.digest,
            harness: &b.composition.harness,
            environment: &b.composition.environment,
            context_limit_bytes: b.composition.context_limit_bytes,
            startup_bytes: b.startup.utf8.len() as u64,
        })
        .collect();
    let allowed: Vec<_> = turn.allowed_environments.iter().map(profile_key).collect();
    let delivery = |credential: &AuthorizedCredential| {
        inst.credential_bindings
            .iter()
            .any(|b| {
                b.credential_profile == credential.profile
                    && b.credential_revision == credential.revision
                    && b.available
                    && fresh(b.observed_at_ms, b.expires_at_ms, input.now_ms)
            })
            .then_some(None)
    };
    let cost_check = |h: &InstalledHarness, _: Option<&CandidateEstimate>| {
        if !turn.allowed_environments.contains(&h.environment) {
            Err((
                CandidateState::PolicyDenied,
                "central_environment_pin_denied",
            ))
        } else {
            Ok(())
        }
    };
    let mut ceilings = turn.ceilings.clone();
    ceilings.wall_time_ms = ceilings
        .wall_time_ms
        .min(input.original_expires_at_ms.saturating_sub(input.now_ms));
    let core = evaluate(
        validated.intelligence,
        validated.execution,
        policy,
        &CoreInputs {
            target: ObservationTarget::CentralInstallation,
            installation: CoreInstallation {
                harnesses: &inst.harnesses,
                observed_at_ms: inst.observed_at_ms,
                expires_at_ms: inst.expires_at_ms,
            },
            credentials: &reg.credentials,
            observations: &reg.observations,
            estimates: &[],
            compositions: &compositions,
            ceilings: &ceilings,
            allowed_environments: &allowed,
            now_ms: input.now_ms,
            global,
            environment_denied: "central_environment_denied",
            installation_stale: "central_installation_snapshot_stale",
            delivery: &delivery,
            cost: &cost_check,
        },
    )?;
    let resolved = if let Some((choice, credential_revision, index)) = core.selected {
        let bundle = input
            .compositions
            .iter()
            .find(|b| b.artifact.reference.digest == choice.composition_digest)
            .ok_or_else(|| SelectionError::Invalid("central chosen composition absent".into()))?;
        let mut expires = input
            .original_expires_at_ms
            .min(reg.expires_at_ms)
            .min(inst.expires_at_ms)
            .min(input.now_ms.saturating_add(2000));
        for binding in &inst.credential_bindings {
            if binding.credential_profile == choice.credential_profile
                && binding.credential_revision == credential_revision
            {
                expires = expires.min(binding.expires_at_ms);
            }
        }
        expires = influencing_expiry(
            &reg.observations,
            input.now_ms,
            policy,
            &core.traces,
            index,
            expires,
        );
        ensure(expires > input.now_ms, "central selected metadata expired")?;
        let selection = ResolvedCentralSelection {
            schema_version: 1,
            subject: turn.subject.clone(),
            reservation: input.reservation.reference.clone(),
            policy: input.policy.reference.clone(),
            reservation_policy: turn.reservation_policy.clone(),
            composition_policy: turn.composition_policy.clone(),
            compatibility: turn.compatibility.clone(),
            catalog: turn.catalog.clone(),
            agent_profile: turn.agent_profile.clone(),
            intelligence_profile: turn.intelligence_profile.clone(),
            execution_profile: turn.execution_profile.clone(),
            registry: input.registry.reference.clone(),
            installation: input.installation.reference.clone(),
            snapshot_digest: digest(input)?,
            choice: CentralChoice {
                candidate_id: choice.candidate_id,
                profile_candidate_id: choice.profile_candidate_id,
                model_settings: aq_session_protocol::ModelSettings {
                    provider: Text::new(choice.provider)
                        .map_err(|_| SelectionError::Invalid("central provider".into()))?,
                    model: Text::new(choice.model)
                        .map_err(|_| SelectionError::Invalid("central model".into()))?,
                    reasoning: Text::new(choice.native_reasoning)
                        .map_err(|_| SelectionError::Invalid("central reasoning".into()))?,
                    max_output_tokens: choice.max_output_tokens,
                    spending_budget: None,
                },
                reasoning_configuration_reference: choice.reasoning_configuration_reference,
                harness_settings: aq_session_protocol::HarnessSettings {
                    harness: Text::new(choice.harness)
                        .map_err(|_| SelectionError::Invalid("central harness".into()))?,
                    cli_version: Text::new(choice.cli_version)
                        .map_err(|_| SelectionError::Invalid("central cli version".into()))?,
                    adapter: choice.adapter,
                    environment: choice.environment,
                    required_capabilities: policy.required_capabilities.clone(),
                },
                qualification: choice.qualification,
                credential_profile: choice.credential_profile,
                credential_revision,
                account_reference: choice.account_reference,
                endpoint_reference: choice.endpoint_reference,
                composition: bundle.artifact.reference.clone(),
                startup: bundle.startup.reference.clone(),
                resources: choice.resources,
            },
            selected_at_ms: input.now_ms,
            expires_at_ms: expires,
            accounting: AccountingState::Unavailable,
            token_count: (),
            spent_microunits: (),
            authority: AuthorityState::None,
        };
        let utf8 = String::from_utf8(pins::canonical(&selection, MAX_OUTPUT_BYTES)?)
            .map_err(|_| SelectionError::Invalid("central selection UTF8".into()))?;
        Some(CentralSelectionArtifact {
            reference: pins::reference("selection", &selection, MAX_OUTPUT_BYTES)?,
            utf8,
            selection,
        })
    } else {
        None
    };
    let result = CentralSelectionOutcome {
        resolved,
        traces: core.traces,
        reason_codes: core.reason_codes,
        accounting: AccountingState::Unavailable,
        token_count: (),
        spent_microunits: (),
        authority: AuthorityState::None,
    };
    pins::bounded(&result, MAX_OUTPUT_BYTES)?;
    Ok(result)
}
/// Exact deterministic rederivation, never current authorization or retention.
pub fn verify_central_selection(
    catalog: &aq_profiles::ValidatedCatalog,
    input: &CentralSelectionInputs,
    outcome: &CentralSelectionOutcome,
) -> Result<()> {
    let expected = select_central(catalog, input)?;
    ensure(
        pins::canonical(&expected, MAX_OUTPUT_BYTES)?
            == pins::canonical(outcome, MAX_OUTPUT_BYTES)?,
        "central selection differs from deterministic inputs",
    )
}
#[derive(Serialize, Deserialize, schemars::JsonSchema)]
#[serde(
    tag = "document",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Document {
    Inputs(Box<CentralSelectionInputs>),
    Outcome(Box<CentralSelectionOutcome>),
}
pub fn decode(bytes: &[u8]) -> Result<Document> {
    ensure(
        !bytes.is_empty() && bytes.len() <= MAX_INPUT_BYTES,
        "central selection document bound",
    )?;
    // Every value is a closed typed record (no caller maps/Value fields).
    // Serde rejects duplicate record fields, unknown fields, float-to-integer
    // coercions and trailing data; its recursion bound remains enabled.
    serde_json::from_slice(bytes)
        .map_err(|_| SelectionError::Invalid("central selection closed JSON fields".into()))
}
pub fn schema() -> serde_json::Value {
    let mut value =
        serde_json::to_value(schemars::schema_for!(Document)).expect("central selection schema");
    // Central-v1 cannot express a task-cumulative spending budget.
    value["$defs"]["ModelSettings"]["properties"]["spending_budget"] =
        serde_json::json!({"type":"null"});
    value["$id"] = serde_json::json!("urn:agent-q:central-selection:1");
    value["x-input-maxBytes"] = serde_json::json!(MAX_INPUT_BYTES);
    value["x-output-maxBytes"] = serde_json::json!(MAX_OUTPUT_BYTES);
    value["x-execution-authority"] = serde_json::json!(false);
    value["x-required-validation"] = serde_json::json!(
        "select_central / verify_central_selection plus independent current owner authentication"
    );
    value
}
