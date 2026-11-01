use crate::*;
use aq_session_api::{
    Candidate, ContractDocument, Eligibility, Omission, OverrideExplanation, Permission,
    ProfileExplanation, ProfileRequest, Selection, Source,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ceilings {
    pub installation: Vec<Permission>,
    pub project: Vec<Permission>,
    pub execution: Vec<Permission>,
    pub caller: Vec<Permission>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextInput {
    pub source: Source,
    pub text: String,
    pub delivery: Delivery,
    pub required: bool,
    /// Smaller values are omitted first; ties preserve input order.
    pub omission_priority: u32,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Override {
    pub field: String,
    pub value: u64,
    pub source: Source,
    pub reason_code: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompileRequest {
    pub profile: ProfileRequest,
    pub ceilings: Ceilings,
    pub available_tools: Vec<String>,
    pub available_adapters: Vec<String>,
    pub harness: String,
    pub environment: String,
    pub platform: ContextInput,
    pub project: ContextInput,
    pub assignment: ContextInput,
    pub context: Vec<ContextInput>,
    pub overrides: Vec<Override>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Instruction {
    pub source: Source,
    pub delivery: Delivery,
    pub required: bool,
    pub text: String,
}
/// Frozen pre-selection composition, not a runnable worker LaunchManifest.
/// AQ-20 must seal concrete model/observation/budget/authority bindings later.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenComposition {
    pub schema_version: u32,
    pub catalog_digest: String,
    pub agent_profile: Source,
    pub intelligence_profile: Source,
    pub execution_profile: Source,
    pub project_binding_id: uuid::Uuid,
    pub project_revision: u64,
    pub task_context_revision: String,
    /// Legacy schema1 only; never serialized in native schema2.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub graph_access_profile_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph_access_policy_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph_access_policy_kind: Option<GraphAccessPolicyKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_permissions: Option<Vec<Permission>>,
    pub harness: String,
    pub environment: String,
    pub instruction_order: Vec<Instruction>,
    pub effective_permissions: Vec<Permission>,
    pub context_limit_bytes: u64,
    pub overrides: Vec<OverrideExplanation>,
    pub omissions: Vec<Omission>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Compilation {
    pub catalog_valid: bool,
    pub composition_valid: bool,
    pub selection_pending: bool,
    pub blocking_reasons: Vec<String>,
    pub composition_digest: Option<String>,
    pub composition: Option<FrozenComposition>,
    pub explanation: ProfileExplanation,
}
pub fn compile(
    catalog: &ValidatedCatalog,
    request: &CompileRequest,
    graph_access_profile_digest: &str,
) -> Result<Compilation> {
    match compile_internal(catalog, request, Some(graph_access_profile_digest))? {
        CompileOutput::Legacy(value) => Ok(*value),
        _ => unreachable!(),
    }
}
/// Compile model-independent instructions and permissions before graph policy
/// sealing. No profile digest or placeholder model is needed to make a draft.
pub fn compile_draft(
    catalog: &ValidatedCatalog,
    request: &CompileRequest,
) -> Result<CompilationDraft> {
    match compile_internal(catalog, request, None)? {
        CompileOutput::Draft(value) => Ok(*value),
        _ => unreachable!(),
    }
}
enum CompileOutput {
    Legacy(Box<Compilation>),
    Draft(Box<CompilationDraft>),
}
fn compile_internal(
    catalog: &ValidatedCatalog,
    request: &CompileRequest,
    graph_access_profile_digest: Option<&str>,
) -> Result<CompileOutput> {
    ensure(
        request.profile.catalog_digest == catalog.digest(),
        "request catalog digest mismatch",
    )?;
    aq_session_api::decode(
        &serde_json::to_vec(&ContractDocument::ProfileExplain(request.profile.clone())).unwrap(),
    )
    .map_err(Error::Invalid)?;
    let subject_contexts = [request.project.clone(), request.assignment.clone()];
    let crate::composition_core::Compiled {
        agent,
        intelligence,
        role_source,
        limit,
        blocking,
        overrides,
        flattened,
        requested,
        effective,
        denied,
        omissions,
        references,
        required_bytes,
        optional_bytes,
        instructions,
    } = crate::composition_core::compile_core(
        catalog,
        &crate::composition_core::CoreRequest {
            catalog_digest: &request.profile.catalog_digest,
            agent_profile: &request.profile.agent_profile,
            ceilings: [
                &request.ceilings.installation,
                &request.ceilings.project,
                &request.ceilings.execution,
                &request.ceilings.caller,
            ],
            available_tools: &request.available_tools,
            available_adapters: &request.available_adapters,
            harness: &request.harness,
            environment: &request.environment,
            platform: &request.platform,
            subject_contexts: &subject_contexts,
            context: &request.context,
            overrides: &request.overrides,
            context_limit_bytes: None,
            context_entry_bytes: MAX_INSTRUCTION_BYTES,
            accounting: crate::composition_core::Accounting::RawText,
            required_context_error: "platform/project/assignment must be startup required",
        },
    )?;
    let Some(graph_access_profile_digest) = graph_access_profile_digest else {
        let composition = blocking.is_empty().then(|| CompositionDraft {
            catalog_digest: catalog.digest().into(),
            agent_profile: role_source,
            intelligence_profile: catalog
                .source("intelligence", &agent.intelligence)
                .expect("validated catalog"),
            execution_profile: catalog
                .source("execution", &agent.execution)
                .expect("validated catalog"),
            project_binding_id: request.profile.project_binding_id,
            project_revision: request.profile.expected_project_revision,
            task_context_revision: request.profile.task_context_revision.clone(),
            harness: request.harness.clone(),
            environment: request.environment.clone(),
            instruction_order: instructions,
            requested_permissions: requested,
            effective_permissions: effective,
            context_limit_bytes: limit,
            overrides,
            omissions,
        });
        return Ok(CompileOutput::Draft(Box::new(CompilationDraft {
            catalog_valid: true,
            composition_valid: blocking.is_empty(),
            blocking_reasons: blocking,
            composition,
        })));
    };
    let explanation = ProfileExplanation {
        schema_version: 1,
        request: request.profile.clone(),
        valid: false,
        agent_profile: role_source.clone(),
        execution_profile: catalog.source("execution", &agent.execution)?,
        graph_access_profile_digest: graph_access_profile_digest.into(),
        flattened_modules: flattened,
        overrides: overrides.clone(),
        requested_permissions: requested,
        effective_permissions: effective.clone(),
        denied_requirements: denied,
        omissions: omissions.clone(),
        reference_material: references,
        required_bytes,
        optional_bytes,
        estimated_tokens: None,
        tokenizer_version: None,
        selection: Selection {
            intelligence_profile: catalog.source("intelligence", &agent.intelligence)?,
            selected_candidate_id: None,
            snapshot_digest: digest(&request)?,
            decision_trace: vec![
                "selection_pending:AQ-20 must authorize observations and choose concrete model"
                    .into(),
            ],
            candidates: intelligence
                .candidates
                .iter()
                .map(|c| Candidate {
                    candidate_id: c.id.clone(),
                    provider: c.provider.clone(),
                    model: c.model.clone(),
                    harness: request.harness.clone(),
                    reasoning_configuration_reference: c.reasoning_configuration_reference.clone(),
                    eligibility: if c.harnesses.contains(&request.harness) {
                        Eligibility::StaleUnknown
                    } else {
                        Eligibility::Unsupported
                    },
                    reason_codes: vec!["selection_pending".into()],
                    observations: vec![],
                })
                .collect(),
        },
    };
    aq_session_api::decode(
        &serde_json::to_vec(&ContractDocument::ProfileExplanation(Box::new(
            explanation.clone(),
        )))
        .unwrap(),
    )
    .map_err(Error::Invalid)?;
    let composition = blocking.is_empty().then(|| FrozenComposition {
        schema_version: 1,
        catalog_digest: catalog.digest().into(),
        agent_profile: role_source,
        intelligence_profile: explanation.selection.intelligence_profile.clone(),
        execution_profile: explanation.execution_profile.clone(),
        project_binding_id: request.profile.project_binding_id,
        project_revision: request.profile.expected_project_revision,
        task_context_revision: request.profile.task_context_revision.clone(),
        graph_access_profile_digest: graph_access_profile_digest.into(),
        graph_access_policy_digest: None,
        graph_access_policy_kind: None,
        requested_permissions: None,
        harness: request.harness.clone(),
        environment: request.environment.clone(),
        instruction_order: instructions,
        effective_permissions: effective,
        context_limit_bytes: limit,
        overrides,
        omissions,
    });
    let composition_digest = composition.as_ref().map(digest).transpose()?;
    Ok(CompileOutput::Legacy(Box::new(Compilation {
        catalog_valid: true,
        composition_valid: blocking.is_empty(),
        selection_pending: true,
        blocking_reasons: blocking,
        composition_digest,
        composition,
        explanation,
    })))
}
