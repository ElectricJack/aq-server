//! Public recipe data for ONE actual CLI / finite local Responses integration.
//! This creates no liveness observation, credential, admission or host authority.
#![allow(dead_code)] // The controller consumes this fixture API in separate stages.
use anyhow::{Context, Result, ensure};
use aq_admission::{ArtifactKind, ArtifactRef, Digest, ObjectId, SourceSelection, Text};
use aq_profiles::{ContextInput, Delivery, ValidatedCatalog};
use aq_server::{
    agent_access::scheduling::TaskContext,
    intelligence::config::{CredentialConfig, IntelligenceConfig, MaterialDeliveryMode},
    profiled_execution::{
        config::{ProfiledConfig, ProjectPolicy, QualifiedRuntime},
        context_budget::{CapacityKind, Qualification, Rendering, Subject},
        context_projection,
        definitions::Definition,
    },
    project_bindings::domain::{OperationScope, ProjectBinding, ProjectCeilings, Reference},
};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use uuid::Uuid;

pub const ENVIRONMENT: &str = "aq-codex-finite-peer-trusted-node";
pub const AGENT: &str = "codex-result-pilot@1";
pub const INTELLIGENCE: &str = "codex-local-responses@1";
pub const EXECUTION: &str = "codex-worker-20s@1";
pub const GROUP: &str = "codex-result-pilot";
pub const MODEL: &str = "gpt-5.1-codex-mini";
pub const REASONING: &str = "low";
pub const REASONING_REFERENCE: &str = "codex-low@1";
pub const PROFILE: &str = "codex-0.166-trusted-node-named-readonly-result-local-v1";
pub const EXECUTION_MS: u64 = 20_000;
pub const WORKER_VERSION: &str = "0.5.1";
pub const SOURCE_DISK_BYTES: u64 = 21 * 1024 * 1024 * 1024 + 1024 * 1024;
const MEMORY_BYTES: u64 = 1024 * 1024 * 1024;
const CONTEXT_BYTES: u64 = 65_536;
const CLIENT_RESERVATION: u64 = 64 * 1024 * 1024;
const STARTUP_SHA: &str = "667d48a537baa35335fb3da1e589846e9d294a2bc24a8df58cf682181b15229a";
const CONTRACT_SHA: &str = "4fe711c405ead85b55a54de2d7446b2c106c4b8809b16bc57497ba2af1d35ef9";
const CODEX_SHA: &str = "b8a11fedf24b2dfb59a70c9e52b030f9a508dcd49cd2dd0d41956f4c785156b8";
const CLIENT_SHA: &str = "fb859cd0cdfe3ee2715cfd657fae90fc8281e85db4b088d08eb490b4607c2c3b";
const HOST_SHA: &str = "96aaafcc295b45e95accf67d06f9a90d5a1caf9dad09ebdd09034f7cf8ad9044";
const TMUX_SHA: &str = "034b15c64035f783d43862f2775eb4828f61571ca62c8199796000b97d556ecd";
const PLATFORM: &str = "Work on the exact pinned assignment in the authorized workspace. Use only the scoped tool permissions. Report source changes and evidence; a report is not task completion or authority to expand scope.";
const SCOPE: &str = "finite numeric-loopback Responses peer only; actual pinned Codex 0.166/tool/result mechanisms; trusted same-UID node, no hostile-UID secrecy, external provider/model/tokenizer/window or general production qualification";

fn canonical(value: impl Serialize) -> Result<String> {
    let mut value = serde_json::to_value(value)?;
    value.sort_all_objects();
    Ok(serde_json::to_string(&value)?)
}
fn reference(id: &str, bytes: &[u8]) -> Result<ArtifactRef> {
    Ok(ArtifactRef {
        id: Text::new(id)?,
        version: Text::new("1")?,
        digest: Digest::of(bytes),
    })
}
fn definition(id: &str, value: impl Serialize) -> Result<Definition> {
    raw_definition(id, canonical(value)?)
}
fn raw_definition(id: &str, body: String) -> Result<Definition> {
    Definition::new(
        reference(id, body.as_bytes())?,
        ArtifactKind::Definition,
        body,
    )
    .map_err(anyhow::Error::msg)
}
fn append(defs: &mut Vec<Definition>, id: &str, body: impl Serialize) -> Result<ArtifactRef> {
    let value = definition(id, body)?;
    let artifact = value.artifact().clone();
    defs.push(value);
    Ok(artifact)
}
fn permissions(project: Uuid, repository: Uuid) -> Vec<aq_session_api::Permission> {
    vec![
        aq_session_api::Permission {
            operation: "task.read".into(),
            scope_reference: format!("project:{project}"),
        },
        aq_session_api::Permission {
            operation: "knowledge.read".into(),
            scope_reference: format!("project:{project}"),
        },
        aq_session_api::Permission {
            operation: "source.write".into(),
            scope_reference: format!("repository:{repository}"),
        },
    ]
}
fn policy(project: Uuid, repository: Uuid) -> Result<Definition> {
    ensure!(
        !project.is_nil() && !repository.is_nil(),
        "real project/repository policy scope"
    );
    // This body is the closed policy understood by the actual B2 owner. Runtime
    // scope and scoped permissions remain in their own definitions/ceilings;
    // adding them here would make the strict policy parser refuse settlement.
    definition(
        &format!("aq.codex-pilot-project-policy:{project}"),
        json!({
            "schema_version":1,"kind":"source_change_reviewed_human_owner_no_checks_v1"
        }),
    )
}
/// Pure catalog metadata; reporting instructions are added by result_startup,
/// never duplicated here as another catalog module.
pub fn catalog(project: Uuid, repository: Uuid) -> Result<ValidatedCatalog> {
    ensure!(
        !project.is_nil() && !repository.is_nil(),
        "real project/repository IDs"
    );
    let value = json!({"schema_version":1,"modules":[{
        "id":"bounded-edit","version":1,"kind":"capability","purpose":"Implement and report one bounded change",
        "instruction":"bounded-edit.md","delivery":"startup","required":true,
        "requirements":permissions(project,repository).into_iter().map(|p|json!({"permission":p,"required":true})).collect::<Vec<_>>()
    }],"agents":[{"id":"codex-result-pilot","version":1,"role":"implementation",
        "behaviors":[],"capabilities":["bounded-edit@1"],"intelligence":INTELLIGENCE,"execution":EXECUTION}],
        "intelligence":[{"id":"codex-local-responses","version":1,"candidates":[{
            "id":"pinned-codex-mini","provider":"openai","model":MODEL,
            "reasoning_configuration_reference":REASONING_REFERENCE,"harnesses":["codex"]}],
            "selection_policy":{"schema_version":1,"fallback_on":[],"allow_unknown_availability":false,
                "observation_max_age_ms":5000,"max_output_tokens":4096,"max_attempts":1,"max_lineage_elapsed_ms":60000,
                "required_capabilities":capabilities()?,"budget":{"max_cost_microunits":null,"currency":null,
                    "require_complete_accounting":false,"require_hard_cost_enforcement":false,"unknown_cost":"allow_bounded"}}}],
        "execution":[{"id":"codex-worker-20s","version":1,"harnesses":["codex"],
            "environments":[ENVIRONMENT],"context_limit_bytes":CONTEXT_BYTES,
            "permissions":permissions(project,repository),"credential_profile_references":["aq.finite-responses-credential@1"]}]});
    let mut instructions = BTreeMap::new();
    instructions.insert("bounded-edit@1".into(), "Make only the requested source change in the authorized worktree. Check the changed file and report exact evidence. Preserve unrelated files.".into());
    Ok(aq_profiles::validate(
        serde_json::from_value(value)?,
        instructions,
    )?)
}
pub fn project(
    project: Uuid,
    repository: Uuid,
    generation: u64,
    workspace: Uuid,
    catalog: &ValidatedCatalog,
) -> Result<ProjectBinding> {
    let policy = policy(project, repository)?;
    Ok(ProjectBinding {
        schema_version: 1,
        project_id: project,
        name: "Finite Codex result integration".into(),
        enabled: true,
        repository_id: repository,
        repository_binding_generation: generation,
        repository_adapter_reference: "aq.git.configured".into(),
        graph_adapter_reference: "aq.graph.configured".into(),
        graph_workspace_id: workspace,
        catalog_digest: catalog.digest().into(),
        allowed_agent_profiles: vec![AGENT.into()],
        allowed_intelligence_profiles: vec![INTELLIGENCE.into()],
        allowed_execution_profiles: vec![EXECUTION.into()],
        allowed_node_ids: vec![],
        allow_enrolled_nodes: false,
        allowed_worker_groups: vec![GROUP.into()],
        allowed_environments: vec![ENVIRONMENT.into()],
        policy: Reference {
            identity: policy.artifact().id.as_str().into(),
            revision: "1".into(),
            digest: policy.artifact().digest.as_str().into(),
        },
        context: vec![],
        ceilings: ProjectCeilings {
            permissions: permissions(project, repository)
                .into_iter()
                .map(|p| OperationScope {
                    operation: p.operation,
                    scope_reference: p.scope_reference,
                })
                .collect(),
            context_bytes: CONTEXT_BYTES,
            wall_time_ms: EXECUTION_MS,
            memory_bytes: MEMORY_BYTES,
            cpu_millis: 1000,
            process_limit: 64,
            cost_limit_microunits: None,
            currency: None,
            require_hard_cost_enforcement: false,
        },
        publication: None,
    })
}
#[derive(Clone, Serialize)]
pub struct ProgramPin {
    pub path: PathBuf,
    pub artifact: ArtifactRef,
    pub bytes: u64,
}
pub struct ImagePins {
    pub worker: ProgramPin,
    pub worker_source: String,
    pub server_source: String,
    pub codex: ProgramPin,
    pub helper: ProgramPin,
    pub tmux: ProgramPin,
    pub client: ProgramPin,
    /// Exact public mapper definition, not executable source or an opaque label.
    pub result_recipe: String,
    pub startup: String,
    pub contract: String,
}
impl ImagePins {
    fn validate(&self) -> Result<()> {
        for source in [&self.worker_source, &self.server_source] {
            ensure!(
                source.len() == 40
                    && source
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
                "full source pin"
            );
        }
        for (pin, sha, bytes) in [
            (&self.codex, CODEX_SHA, Some(319346888)),
            (&self.client, CLIENT_SHA, Some(16655808)),
            (&self.helper, HOST_SHA, None),
            (&self.tmux, TMUX_SHA, None),
        ] {
            ensure!(
                pin.artifact.digest.as_str() == sha && bytes.is_none_or(|n| pin.bytes == n),
                "selected actual image pin"
            );
        }
        for pin in [
            &self.worker,
            &self.codex,
            &self.helper,
            &self.tmux,
            &self.client,
        ] {
            ensure!(
                pin.bytes > 0
                    && pin.path.is_absolute()
                    && pin.path.starts_with("/opt/aq-qualification")
                    && pin
                        .path
                        .components()
                        .all(|c| !matches!(c, std::path::Component::ParentDir)),
                "fixed readonly guest image route"
            );
        }
        ensure!(
            self.startup.len() == 1929
                && Digest::of(self.startup.as_bytes()).as_str() == STARTUP_SHA,
            "exact counted public startup"
        );
        ensure!(
            Digest::of(self.contract.as_bytes()).as_str() == CONTRACT_SHA,
            "exact public local-result contract"
        );
        let recipe: Value = serde_json::from_str(&self.result_recipe)?;
        ensure!(
            recipe["recipe"] == "codex-0.166-result-local-v1"
                && recipe["mapper"] == "aq-codex-mapper@0.1.1",
            "selected public mapper recipe"
        );
        Ok(())
    }
}
fn runtime_code(worker: &ProgramPin, worker_source: &str) -> Result<Definition> {
    definition(
        "aq.codex-finite-peer-runtime",
        json!({"schema_version":1,"worker":worker,"worker_source":worker_source,
            "runtime_version":WORKER_VERSION,"journal_format":12,"scope":SCOPE}),
    )
}
pub struct PrepareInput<'a> {
    pub actual: &'a crate::common::selection::ActualSelectionFixture,
    pub context: &'a TaskContext,
    pub catalog: &'a ValidatedCatalog,
    pub images: &'a ImagePins,
    pub now_ms: u64,
    pub base: ObjectId,
    pub retention: Uuid,
}
/// Public immutable recipe outputs. No credential bytes, live grant or owner.
pub struct Prepared {
    pub profiled: ProfiledConfig,
    pub runtime: ArtifactRef,
    pub qualification: ArtifactRef,
    pub adapter: ArtifactRef,
    pub environment: ArtifactRef,
    pub renderer: ArtifactRef,
    pub credential: ArtifactRef,
    pub harness: aq_selection::InstalledHarness,
    pub permission_plan: aq_session_protocol::PermissionPlan,
    pub permission_digest: Digest,
    pub client_manifest: Value,
    pub client_manifest_reference: ArtifactRef,
    pub composite_definition: Value,
    pub primary_manifest: Value,
    pub primary_manifest_digest: Digest,
    pub source_profile: ArtifactRef,
    pub source_disk_bytes: u64,
    pub phase_policy: ArtifactRef,
    pub phase_execution: ArtifactRef,
    pub phase_terminal: ArtifactRef,
    pub presence: Value,
    pub presence_reference: ArtifactRef,
    pub inventory_adapter: ArtifactRef,
    pub inventory_evidence: ArtifactRef,
    pub health: Value,
}
fn context(artifact: &ArtifactRef, text: String) -> ContextInput {
    ContextInput {
        source: aq_session_api::Source {
            identity: artifact.id.as_str().into(),
            revision: artifact.version.as_str().into(),
            digest: artifact.digest.as_str().into(),
        },
        text,
        delivery: Delivery::Startup,
        required: true,
        omission_priority: 0,
    }
}
pub fn prepare(input: PrepareInput<'_>) -> Result<Prepared> {
    use aq_admission::preparation_refresh_v1 as phase;
    input.images.validate()?;
    ensure!(
        input.now_ms > 0 && !input.retention.is_nil(),
        "real recipe time/retention"
    );
    let project = &input.actual.project.binding;
    ensure!(
        input.actual.task == input.context.revision
            && project.catalog_digest == input.catalog.digest(),
        "exact original actual task/catalog"
    );
    let expected_project = self::project(
        project.project_id,
        project.repository_id,
        project.repository_binding_generation,
        project.graph_workspace_id,
        input.catalog,
    )?;
    ensure!(
        serde_json::to_value(project)? == serde_json::to_value(expected_project)?,
        "unchanged supported actual project policy"
    );
    let permissions = permissions(project.project_id, project.repository_id);
    let projection = context_projection::project(
        input.context,
        project.project_id,
        project.repository_id,
        project.graph_workspace_id,
        &permissions,
        input.now_ms,
    )?;
    let project_policy = policy(project.project_id, project.repository_id)?;
    let project_text = format!(
        "Project policy (exact owner definition follows):\n{}",
        project_policy.body()
    );
    let project_input = reference(
        &format!(
            "aq.project-instructions/{}/{}",
            project_policy.artifact().digest.as_str(),
            Digest::of(project_text.as_bytes()).as_str()
        ),
        project_text.as_bytes(),
    )?;
    let startup = reference("aq.agent-result-startup", input.images.startup.as_bytes())?;
    let draft = aq_profiles::compile_draft(
        input.catalog,
        &aq_profiles::CompileRequest {
            profile: aq_session_api::ProfileRequest {
                catalog_digest: input.catalog.digest().into(),
                agent_profile: AGENT.into(),
                project_binding_id: project.project_id,
                expected_project_revision: input.actual.project.revision,
                task_context_revision: format!(
                    "{}:{}:{}",
                    input.context.revision.task_id,
                    input.context.revision.version,
                    input.context.revision.content_revision
                ),
            },
            ceilings: aq_profiles::Ceilings {
                installation: permissions.clone(),
                project: permissions.clone(),
                execution: permissions.clone(),
                caller: permissions,
            },
            available_tools: vec![
                "task.read".into(),
                "knowledge.read".into(),
                "source.write".into(),
            ],
            available_adapters: vec!["codex".into()],
            harness: "codex".into(),
            environment: ENVIRONMENT.into(),
            platform: context(
                &reference("aq.platform-native-instructions", PLATFORM.as_bytes())?,
                PLATFORM.into(),
            ),
            project: context(&project_input, project_text),
            assignment: context(&projection.artifact, String::from_utf8(projection.bytes)?),
            context: vec![context(&startup, input.images.startup.clone())],
            overrides: vec![],
        },
    )?;
    ensure!(
        draft.catalog_valid && draft.composition_valid && draft.blocking_reasons.is_empty(),
        "actual compiler rejected recipe: {:?}",
        draft.blocking_reasons
    );
    let composition = draft.composition.context("actual composition absent")?;
    let scopes = |v: &[aq_session_api::Permission]| -> Result<Vec<aq_session_protocol::Scope>> {
        v.iter()
            .map(|p| {
                Ok(aq_session_protocol::Scope {
                    operation: Text::new(&p.operation)?,
                    resource: Text::new(&p.scope_reference)?,
                })
            })
            .collect()
    };
    let effective = scopes(&composition.effective_permissions)?;
    let permission_plan = aq_session_protocol::PermissionPlan {
        requested: scopes(&composition.requested_permissions)?,
        effective: effective.clone(),
        required: effective,
        omissions: composition
            .omissions
            .iter()
            .map(|o| Text::new(&o.reason_code))
            .collect::<std::result::Result<_, _>>()?,
    };
    let permission_digest = Digest::of(canonical(&permission_plan)?.as_bytes());
    let images = input.images;
    let mut defs = vec![project_policy];
    let runtime_definition = runtime_code(&images.worker, &images.worker_source)?;
    let runtime = runtime_definition.artifact().clone();
    defs.push(runtime_definition);
    let environment = append(
        &mut defs,
        ENVIRONMENT,
        json!({"schema_version":1,"scope":SCOPE,"images":{"codex":images.codex,"host":images.helper,"tmux":images.tmux,"client":images.client},
        "premises":["readonly image mounts and ancestors; no writable aliases","all capabilities zero; NoNewPrivs=1","retained FD/mount/user namespace owner checked by NamedReadonlyEnvironment","fixed readonly /usr/bin:/bin tool PATH; OS bwrap 0.9.0 provisioned","fixture private network, PID and masked home views supplied independently"],
        "primary_image_reservation_bytes":536870912,"additional_client_image_reservation_bytes":CLIENT_RESERVATION,
        "bwrap":{"sha256":"e318903862396f96de3df57264e0158682b952fd3fb53ac23d876413e7b30f71","bytes":72160,"path":"/usr/bin/bwrap","provisioning":"readonly OS environment; not a NamedExecManifest image pin"}}),
    )?;
    let contract = raw_definition("aq.agent-result-local", images.contract.clone())?;
    let contract_ref = contract.artifact().clone();
    defs.push(contract);
    defs.push(raw_definition(
        "aq.agent-result-startup",
        images.startup.clone(),
    )?);
    let recipe = raw_definition("aq.codex-result-local-recipe", images.result_recipe.clone())?;
    let recipe_ref = recipe.artifact().clone();
    defs.push(recipe);
    let client_manifest = json!({"schema_version":1,"profile":"aq-agent-result-client-named-readonly-v1","contract":contract_ref,"environment":environment,
        "executable":{"path":images.client.path,"artifact":images.client.artifact},"executable_bytes":images.client.bytes});
    let client_manifest_reference = append(
        &mut defs,
        "aq.agent-result-client-installation",
        &client_manifest,
    )?;
    let composite_definition = json!({"schema_version":1,"profile":PROFILE,"runtime":runtime,"environment":environment,
        "client_manifest":client_manifest_reference,"client_executable":images.client.artifact,"contract":contract_ref,"recipe":recipe_ref,
        "permission_digest":permission_digest,"reserved_client_bytes":CLIENT_RESERVATION});
    let adapter = append(
        &mut defs,
        "aq.codex-result-local-adapter",
        &composite_definition,
    )?;
    let qualification = append(
        &mut defs,
        "aq.codex-finite-peer-runtime-qualification",
        json!({"schema_version":1,"runtime":runtime,"adapter":adapter,"environment":environment,
        "server_source":images.server_source,"scope":SCOPE,"qualification":"reviewed named-image, scoped cap injection, exact SourcePin and local result owner source; finite actual integration candidate only",
        "not_evidence":["no current Node health or inventory","no successful execution claimed by recipe construction","no external provider qualification"]}),
    )?;
    let harness_q = append(
        &mut defs,
        "aq.codex-finite-peer-harness-qualification",
        json!({"schema_version":1,"runtime":runtime,"qualification":qualification,"adapter":adapter,"environment":environment,
        "codex":images.codex,"host":images.helper,"tmux":images.tmux,"scope":SCOPE,"tool_environment":"inherit=none; fixed PATH=/usr/bin:/bin; snapshots/login/plugin sync disabled","max_output_tokens":4096,"cap_semantics":"per Responses request; no aggregate spending support"}),
    )?;
    let credential = append(
        &mut defs,
        "aq.finite-responses-credential",
        json!({"schema_version":1,"provider":"openai","account_reference":"finite-responses-account","endpoint_reference":"finite-private-loopback-responses","scope":SCOPE,"secret":"synthetic fixed fixture key; separately protected owner file; not included here"}),
    )?;
    let count = append(
        &mut defs,
        "aq.codex-finite-peer-counting",
        json!({"schema_version":1,"adapter":adapter,"runtime":runtime,"qualification":qualification,"harness_qualification":harness_q,
        "scope":SCOPE,"rendering":"exact UTF8 double-newline joined startup","maximum_tokens_per_utf8_byte":1,"fixed_overhead_tokens":32768,
        "basis":"conservative finite-peer planning units; prior actual CLI implicit context/tool capture bounded below 32768 bytes; NOT external model tokenizer evidence"}),
    )?;
    let capacity = append(
        &mut defs,
        "aq.codex-finite-peer-capacity",
        json!({"schema_version":1,"scope":SCOPE,"adapter":adapter,"planning_ceiling":131072,"kind":"qualified_conservative_planning_ceiling","basis":"bounded finite-peer integration policy; not model-native window"}),
    )?;
    let context_fact = Qualification {
        schema_version: 1,
        subject: Subject {
            runtime: runtime.clone(),
            runtime_qualification: qualification.clone(),
            harness: "codex".into(),
            cli_version: "0.166.0".into(),
            adapter: adapter.clone(),
            environment: environment.clone(),
            harness_qualification: harness_q.clone(),
            provider: "openai".into(),
            model: MODEL.into(),
            native_reasoning: REASONING.into(),
            reasoning_configuration_reference: REASONING_REFERENCE.into(),
            credential_profile: credential.clone(),
            credential_revision: 1,
            account_reference: "finite-responses-account".into(),
            endpoint_reference: "finite-private-loopback-responses".into(),
        },
        rendering: Rendering::Utf8DoubleNewlineV1,
        maximum_tokens_per_utf8_byte: 1,
        fixed_overhead_tokens: 32768,
        counting_evidence: count,
        capacity_kind: CapacityKind::QualifiedConservativePlanningCeiling,
        capacity_tokens: 131072,
        capacity_evidence: capacity,
        observed_at_ms: input.now_ms,
        expires_at_ms: input
            .now_ms
            .checked_add(120000)
            .context("qualification expiry")?,
    };
    context_fact.validate()?;
    let renderer = context_fact.artifact()?;
    let primary_manifest = json!({"schema_version":1,"profile":"codex-0.166-named-readonly-exec-stdin-v1","executable_bytes":images.codex.bytes,"environment":environment,
        "installation":{"schema_version":1,"adapter":adapter,"runtime":runtime,"qualification":qualification,"renderer":renderer,"handoff":"opaque_private_file_v1",
            "executable":{"path":images.codex.path,"artifact":images.codex.artifact},"helper":{"path":images.helper.path,"artifact":images.helper.artifact},"tmux":{"path":images.tmux.path,"artifact":images.tmux.artifact}}});
    let primary_manifest_digest = Digest::of(canonical(&primary_manifest)?.as_bytes());
    let source_profile = append(
        &mut defs,
        "aq.codex-pilot-default-git-profile",
        json!({"schema_version":1,"checkout_profile":"no_attributes_v1","max_expanded_bytes":10737418240u64,"max_bundle_bytes":1073741824u64,"source_disk_bytes":SOURCE_DISK_BYTES,"scope":"unchanged default aq-git bounds; actual service capabilities must match before admission"}),
    )?;
    let mut owners = vec![];
    for kind in phase::FactKind::all() {
        let authority = append(
            &mut defs,
            &format!("aq.codex-pilot-owner/{kind:?}"),
            json!({"schema_version":1,"kind":kind,"server_source":images.server_source,"scope":"actual authenticated daemon owner; no fabricated live observation; read/fence again at execution"}),
        )?;
        owners.push(phase::OwnerAuthority { kind, authority });
    }
    let phase_policy = append(
        &mut defs,
        "aq.accepted-preparation-refresh-policy/codex-finite-peer",
        json!({"schema_version":1,"contract":phase::contract_artifact()?,"runtime":runtime,"qualification":qualification,
        "phase":{"schema_version":2,"maximum_preparation_ms":30000,"maximum_execution_lease_ms":EXECUTION_MS,"maximum_observation_ms":2000},"source_profile":source_profile,"source_disk_bytes":SOURCE_DISK_BYTES,"owners":owners}),
    )?;
    let phase_execution = append(
        &mut defs,
        "aq.phase-refresh-execution-qualification/codex-finite-peer",
        json!({"schema_version":1,"contract":phase::contract_artifact()?,"runtime":runtime,"runtime_qualification":qualification,"proposal_policy":phase_policy,
        "adapters":"registry_observation_continuity_current_project_principal_inventory_health_unissued_graph_retention_private_v1"}),
    )?;
    let proof = append(
        &mut defs,
        "aq.codex-pilot-physical-proof-producer",
        json!({"schema_version":1,"worker":images.worker,"helper":images.helper,"tmux":images.tmux,"scope":"actual retained SessionHandle guardian/incarnation plus kernel ECHILD; a configured definition is not a stopped event"}),
    )?;
    let phase_terminal = append(
        &mut defs,
        "aq.phase-terminal-refresh-qualification/codex-finite-peer",
        json!({"schema_version":1,"contract":aq_phase_terminal_protocol::refresh_v1::contract_artifact()?,"runtime":runtime,"runtime_qualification":qualification,"proof_producer":proof}),
    )?;
    let inventory_adapter = append(
        &mut defs,
        "aq.codex-pilot-scoped-presence-adapter",
        json!({"schema_version":1,"worker":images.worker,"scope":"actual original grant bounded scoped material presence; public inventory lists provisionable profiles only"}),
    )?;
    let inventory_evidence = append(
        &mut defs,
        "aq.codex-pilot-scoped-presence-evidence",
        json!({"schema_version":1,"adapter":inventory_adapter,"runtime":runtime,"qualification":qualification,"scope":SCOPE,"not_evidence":"no live presence or inventory receipt is created here"}),
    )?;
    let presence = json!({"schema_version":1,"protocol":"aq.scoped-material-presence.v1","runtime":runtime,"runtime_qualification":qualification,"adapter":inventory_adapter,"adapter_qualification":inventory_evidence,
        "inventory_semantics":"provisionable_only_and_scoped_presence_v1","observation_protocol":"grant_issuance_bounded_boottime_v1","maximum_age_ms":30000,"maximum_ttl_ms":30000});
    let presence_reference = append(
        &mut defs,
        "aq.scoped-material-presence-qualification/codex-finite-peer",
        &presence,
    )?;
    let material = append(
        &mut defs,
        "aq.server-material",
        json!({"schema_version":1,"server_source":images.server_source,"adapter":"aq.server-material","version":1,"scope":"authenticated exact-attempt private material owner; protected synthetic finite-peer key only"}),
    )?;
    let capabilities = capabilities()?;
    let harness = aq_selection::InstalledHarness {
        harness: "codex".into(),
        cli_version: "0.166.0".into(),
        adapter: adapter.clone(),
        environment: environment.clone(),
        qualification: harness_q,
        failure_classifier: None,
        models: vec![aq_selection::NativeModelSupport {
            provider: "openai".into(),
            model: MODEL.into(),
            reasoning_configuration_reference: REASONING_REFERENCE.into(),
            native_reasoning: REASONING.into(),
        }],
        capabilities: capabilities.clone(),
        limits: aq_selection::NativeLimits {
            context_bytes: CONTEXT_BYTES,
            max_output_tokens: 4096,
            wall_time_ms: EXECUTION_MS,
            memory_bytes: MEMORY_BYTES,
            cpu_millis: 1000,
            process_limit: 64,
            hard_cost_enforcer: None,
        },
    };
    let profiled = ProfiledConfig {
        revision: 1,
        qualified_runtimes: vec![QualifiedRuntime {
            runtime: runtime.clone(),
            qualification: qualification.clone(),
            capabilities,
            features: features(),
            material_delivery: Some(aq_selection::DeliveryCapability {
                adapter: material,
                version: 1,
            }),
        }],
        projects: vec![ProjectPolicy {
            project_id: project.project_id,
            enabled: true,
            priority: 0,
            source: SourceSelection::Immutable { object: input.base },
            retention_handle: input.retention,
            placement: None,
            agent_grants: None,
        }],
        context_qualifications: vec![context_fact],
        definition_bodies: defs,
    };
    profiled.validate().map_err(anyhow::Error::msg)?;
    let health = json!({"scope":SCOPE,"provider":"openai","model":MODEL,"credential_profile":credential,"credential_revision":1,"account_reference":"finite-responses-account","endpoint_reference":"finite-private-loopback-responses",
        "requirements":["actual pinned images and preflight","actual finite peer listening","real enrolled Node inventory/presence/journal","operator assessment through public API at actual observation time; refresh only through negotiated owner"],"no_observation_created":true});
    Ok(Prepared {
        profiled,
        runtime,
        qualification,
        adapter,
        environment,
        renderer,
        credential,
        harness,
        permission_plan,
        permission_digest,
        client_manifest,
        client_manifest_reference,
        composite_definition,
        primary_manifest,
        primary_manifest_digest,
        source_profile,
        source_disk_bytes: SOURCE_DISK_BYTES,
        phase_policy,
        phase_execution,
        phase_terminal,
        presence,
        presence_reference,
        inventory_adapter,
        inventory_evidence,
        health,
    })
}
fn capabilities() -> Result<Vec<aq_session_protocol::Capability>> {
    [
        "aq.profiled-launch",
        "aq.terminal",
        "aq.harness.startup-context",
        "aq.harness.confined-worktree",
        "aq.harness.stop",
    ]
    .into_iter()
    .map(|id| {
        Ok(aq_session_protocol::Capability {
            id: Text::new(id)?,
            version: 1,
        })
    })
    .collect()
}
fn mapper_harness(
    adapter: &ArtifactRef,
    environment: &ArtifactRef,
) -> Result<aq_session_protocol::HarnessSettings> {
    Ok(aq_session_protocol::HarnessSettings {
        harness: Text::new("codex")?,
        cli_version: Text::new("0.166.0")?,
        adapter: adapter.clone(),
        environment: environment.clone(),
        // Match the actual producer's manifest projection in producer/build.rs:
        // launch/terminal are offer requirements; these three belong to harness.
        required_capabilities: capabilities()?[2..].to_vec(),
    })
}
fn features() -> Vec<aq_admission::transport::Feature> {
    use aq_admission::transport::Feature;
    vec![
        Feature::ArtifactTransferV1,
        Feature::ServerMaterialV1,
        Feature::DurableHostJournalV1,
    ]
}
impl Prepared {
    pub fn registry(&self, node: Uuid, secret_file: PathBuf) -> Result<IntelligenceConfig> {
        ensure!(
            !node.is_nil() && secret_file.is_absolute(),
            "actual node/protected credential path"
        );
        let registry = IntelligenceConfig {
            accounting: Default::default(),
            material_store: None,
            revision: 1,
            credentials: vec![CredentialConfig {
                kind: Default::default(),
                allowed_backends: aq_selection::default_allowed_backends(),
                credential: aq_selection::AuthorizedCredential {
                    profile: self.credential.clone(),
                    provider: "openai".into(),
                    account_reference: "finite-responses-account".into(),
                    endpoint_reference: "finite-private-loopback-responses".into(),
                    revision: 1,
                    enabled: true,
                    provisionable: false,
                    material_owner: None,
                    allowed_backends: aq_selection::default_allowed_backends(),
                },
                allowed_node_ids: vec![node],
                allow_enrolled_nodes: false,
                secret_file: Some(secret_file),
                delivery_mode: Some(MaterialDeliveryMode::ServerMaterialV1),
            }],
            qualified_harnesses: vec![self.harness.clone()],
        };
        registry.validate().map_err(anyhow::Error::msg)?;
        Ok(registry)
    }
    /// Writes are performed later by the trusted controller under its retained
    /// private state-directory FD; these returned paths carry no filesystem grant.
    pub fn files(&self) -> Result<BTreeMap<String, String>> {
        Ok(BTreeMap::from([
            ("client.json".into(), canonical(&self.client_manifest)?),
            (
                "composite.json".into(),
                canonical(&self.composite_definition)?,
            ),
            ("adapter.json".into(), canonical(&self.primary_manifest)?),
            ("inventory.json".into(), canonical(&self.presence)?),
        ]))
    }
    pub fn native_json(&self, state: &Path) -> Result<Value> {
        ensure!(
            state == Path::new("/var/lib/aq-qualification/state"),
            "closed private guest state route"
        );
        Ok(
            json!({"schema_version":1,"state_dir":state,"git_socket":state.join("git.sock"),"git_root":state.join("git"),
            "capacity":{"slots":1,"memory_bytes":MEMORY_BYTES,"cpu_millis":1000,"disk_bytes":25769803776u64},
            "runtime":{"runtime":self.runtime,"qualification":self.qualification},"adapter":self.adapter,"renderer":self.renderer,
            "adapter_manifest":state.join("config/adapter.json"),"adapter_manifest_digest":self.primary_manifest_digest,
            "capabilities":capabilities()?,"features":features(),"negotiation":aq_admission::preparation_refresh_v1::Negotiation{contract:aq_admission::preparation_refresh_v1::contract_artifact()?,features:aq_admission::preparation_refresh_v1::required_features()},
            "source_profile":self.source_profile,"maximum_preparation_ms":30000,
            "inventory_definition":state.join("config/inventory.json"),"inventory_reference":self.presence_reference,"inventory_adapter":self.inventory_adapter,
            "inventory_evidence":self.inventory_evidence,"harness_qualifications":[self.harness.qualification],"inventory_ttl_ms":30000,
            "executor":{"kind":PROFILE,"environment":self.environment,"client_manifest":state.join("config/client.json"),"client_manifest_digest":self.client_manifest_reference.digest,
                "composite_definition":state.join("config/composite.json"),"settings":{"host_root":state.join("host"),"columns":80,"rows":24,"permissions_digest":self.permission_digest,
                    "harness":mapper_harness(&self.adapter,&self.environment)?,
                    "model":{"provider":"openai","model":MODEL,"reasoning":REASONING,"max_output_tokens":4096,"spending_budget":null}}}}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_code_selects_new_worker_version_without_changing_journal_format() {
        let worker = ProgramPin {
            path: "/opt/aq-qualification/aq-worker".into(),
            artifact: reference(
                "aq.recipe-test-worker",
                b"pure worker metadata, not executable proof",
            )
            .unwrap(),
            bytes: 1,
        };
        let definition = runtime_code(&worker, &"a".repeat(40)).unwrap();
        let body: Value = serde_json::from_str(definition.body()).unwrap();
        assert_eq!(body["runtime_version"], "0.5.1");
        assert_eq!(body["journal_format"], 12);
        assert_eq!(body["worker"], serde_json::to_value(worker).unwrap());
        assert_eq!(
            definition.artifact().digest,
            Digest::of(definition.body().as_bytes())
        );
    }

    #[test]
    fn mapper_harness_matches_manifest_projection_without_offer_only_requirements() {
        // Pure metadata, no fabricated offer or execution grant. The actual
        // producer projects required_caps[2..] into manifest.harness and keeps
        // all five on the offer; the worker compares HarnessSettings exactly.
        let adapter = reference("aq.recipe-test-adapter", b"pure adapter metadata").unwrap();
        let environment =
            reference("aq.recipe-test-environment", b"pure environment metadata").unwrap();
        let all = capabilities().unwrap();
        let expected = aq_session_protocol::HarnessSettings {
            harness: Text::new("codex").unwrap(),
            cli_version: Text::new("0.166.0").unwrap(),
            adapter: adapter.clone(),
            environment: environment.clone(),
            required_capabilities: all[2..].to_vec(),
        };
        let mapped = mapper_harness(&adapter, &environment).unwrap();
        assert_eq!(mapped, expected);
        assert_eq!(all.len(), 5);
        assert_eq!(mapped.required_capabilities.len(), 3);
        assert_eq!(
            mapped
                .required_capabilities
                .iter()
                .map(|c| c.id.as_str())
                .collect::<Vec<_>>(),
            [
                "aq.harness.startup-context",
                "aq.harness.confined-worktree",
                "aq.harness.stop"
            ]
        );
        let mut old = mapped.clone();
        old.required_capabilities = all;
        assert_ne!(old, expected);
    }

    #[test]
    fn selected_policy_is_accepted_by_actual_b2_owner_before_project_creation() {
        use aq_server::profiled_execution::settlement::eligibility::PilotPolicy;
        let project = Uuid::new_v4();
        let repository = Uuid::new_v4();
        let selected = policy(project, repository).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(selected.body()).unwrap(),
            json!({"schema_version":1,"kind":"source_change_reviewed_human_owner_no_checks_v1"})
        );
        assert!(PilotPolicy::from_definition(&selected).is_ok());
        let mut unsupported: Value = serde_json::from_str(selected.body()).unwrap();
        unsupported["permissions"] = json!(permissions(project, repository));
        let unsupported = definition("aq.recipe-test-unsupported-policy", unsupported).unwrap();
        assert!(PilotPolicy::from_definition(&unsupported).is_err());
    }

    #[test]
    fn actual_catalog_is_canonical_and_requests_only_existing_scoped_permissions() {
        let project = Uuid::new_v4();
        let repository = Uuid::new_v4();
        let catalog = catalog(project, repository).unwrap();
        let bytes = catalog.canonical_bytes().unwrap();
        let roundtrip = ValidatedCatalog::from_artifact(&bytes).unwrap();
        assert_eq!(roundtrip.canonical_bytes().unwrap(), bytes);
        assert_eq!(catalog.artifact().catalog.execution[0].permissions.len(), 3);
        assert_eq!(catalog.artifact().catalog.modules.len(), 1);
        assert!(
            !String::from_utf8(bytes)
                .unwrap()
                .contains("AQ_AGENT_RESULT")
        );
        let selected = catalog.artifact().catalog.intelligence[0]
            .selection_policy
            .as_ref()
            .unwrap();
        assert_eq!(selected.max_output_tokens, 4096);
        assert!(!selected.allow_unknown_availability);
        assert_eq!(selected.observation_max_age_ms, 5000);
    }

    #[test]
    fn project_binds_real_ids_catalog_and_exact_twenty_second_policy() {
        let project_id = Uuid::new_v4();
        let repository = Uuid::new_v4();
        let workspace = Uuid::new_v4();
        let catalog = catalog(project_id, repository).unwrap();
        let project = project(project_id, repository, 7, workspace, &catalog).unwrap();
        let config = aq_server::project_bindings::domain::ProjectConfig {
            worker_groups: BTreeMap::from([(GROUP.into(), vec![Uuid::new_v4()])]),
        };
        project.validate(&config).unwrap();
        let mut invalid = project.clone();
        invalid.policy.identity = format!("aq.codex-pilot-project-policy/{project_id}");
        assert!(invalid.validate(&config).is_err());
        assert_eq!(project.repository_binding_generation, 7);
        assert_eq!(project.graph_workspace_id, workspace);
        assert_eq!(project.catalog_digest, catalog.digest());
        assert_eq!(project.ceilings.wall_time_ms, 20_000);
        assert!(project.allowed_node_ids.is_empty());
        let definition = policy(project_id, repository).unwrap();
        assert_eq!(project.policy.digest, definition.artifact().digest.as_str());
        assert_eq!(
            definition.artifact().digest,
            Digest::of(definition.body().as_bytes())
        );
        assert!(
            project
                .ceilings
                .permissions
                .iter()
                .all(|p| p.operation != "result.submit")
        );
    }

    #[test]
    fn public_definition_ref_is_exact_canonical_body_and_not_label_hash() {
        let body = json!({"scope":"pure metadata test only","a":2,"b":1});
        let definition = definition("aq.recipe-test", body).unwrap();
        assert_eq!(
            definition.artifact().digest,
            Digest::of(definition.body().as_bytes())
        );
        assert_ne!(definition.artifact().digest, Digest::of(b"aq.recipe-test"));
        assert_eq!(
            definition.body(),
            "{\"a\":2,\"b\":1,\"scope\":\"pure metadata test only\"}"
        );
    }
}
