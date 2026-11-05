//! Synthetic actual profiled state, never a fake legacy ExecutionSpec. These
//! fixtures qualify authority/SQL fences, not a provider, secret lease or launch.
use aq_server::{
    AppState,
    config::Config,
    execution::profiled::{Effects, ProfiledState, Retention, Status, Stop},
    identity::verifier_from_config,
    store::NewNode,
    test_support::TestDb,
    tokens,
};
use aq_session_protocol::{
    ArtifactRef, Digest, ProfiledOffer, SecretRef, SessionIdentity, Text, canonical_digest,
};
use aq_worker_protocol::{
    GraphBinding, NodeIdentity, ObjectAlgorithm, ObjectId, RegisterRequest, SourceBinding,
};
use chrono::{Duration, TimeZone, Utc};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicI64, Ordering},
};
use uuid::Uuid;
pub const NOW: u64 = 1_000_000;
pub fn artifact(id: &str, bytes: &[u8]) -> ArtifactRef {
    ArtifactRef {
        id: Text::new(id).unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(bytes),
    }
}
pub struct Fixture {
    pub db: Arc<TestDb>,
    pub state: AppState,
    pub node: NodeIdentity,
    pub profiled: ProfiledState,
    pub selection: aq_selection::ResolvedSelection,
    pub decision: Uuid,
    pub actor: Uuid,
    pub credential: String,
    pub time: Arc<AtomicI64>,
}
pub async fn fixture() -> Fixture {
    let db = Arc::new(TestDb::create().await);
    let store = db.store().await;
    let actual = Utc.timestamp_millis_opt(NOW as i64).unwrap();
    let owner = store
        .bootstrap_owner("owner@localhost", "dev", "owner@localhost", actual)
        .await
        .unwrap()
        .unwrap();
    let credential = tokens::new_node_credential();
    store
        .create_enrollment(
            "profiled-fixture",
            owner.id,
            actual,
            actual + Duration::minutes(5),
        )
        .await
        .unwrap();
    let node = store
        .redeem_enrollment(
            "profiled-fixture",
            NewNode {
                name: "profiled-fixture".into(),
                credential_hash: tokens::verifier(&credential),
                worker_version: "0.1.0".into(),
                protocol_version: 1,
            },
            actual,
        )
        .await
        .unwrap()
        .unwrap();
    let identity = NodeIdentity {
        node_id: node.id,
        boot_id: Uuid::new_v4(),
    };
    store
        .execution_register(&RegisterRequest {
            identity: identity.clone(),
            operation_id: Uuid::new_v4(),
            protocol_version: 2,
            worker_version: "0.1.0".into(),
            capabilities: vec![aq_worker_protocol::FAKE_HARNESS_CAPABILITY.into()],
        })
        .await
        .unwrap();
    let raw: Value = serde_json::from_str(include_str!("../fixtures/selection-v2.json")).unwrap();
    let catalog = aq_profiles::ValidatedCatalog::from_artifact(
        raw["catalog_artifact_json"].as_str().unwrap().as_bytes(),
    )
    .unwrap();
    let mut input: aq_selection::SelectionInputs =
        serde_json::from_value(raw["inputs"].clone()).unwrap();
    input.project.target = aq_selection::ExecutionTarget::Worker {
        node_id: node.id,
        boot_id: identity.boot_id,
    };
    input.node.target = input.project.target.clone();
    input.project.expires_at_ms = NOW + 30_000;
    input.node.observed_at_ms = NOW - 10;
    input.node.expires_at_ms = NOW + 30_000;
    input.budget.observed_at_ms = NOW - 10;
    input.budget.expires_at_ms = NOW + 30_000;
    input.now_ms = NOW;
    for c in &mut input.node.credential_bindings {
        c.observed_at_ms = NOW - 10;
        c.expires_at_ms = NOW + 30_000;
    }
    for o in &mut input.observations {
        o.received_at_ms = NOW - 10;
        o.expires_at_ms = NOW + 30_000;
        o.cooldown_until_ms = NOW - 10;
    }
    let selected = aq_selection::select(&catalog, &input)
        .unwrap()
        .resolved
        .unwrap();
    let state_projection = state_from_selection(&selected, &identity);
    let cfg: Config = toml::from_str(
        r#"format_version=1
environment="development"
[server]
admin_bind="127.0.0.1:0"
worker_bind="127.0.0.1:0"
public_origin="http://127.0.0.1:8080"
[auth]
mode="dev"
dev_email="owner@localhost"
[bootstrap]
owner_email="owner@localhost"
enabled=true
"#,
    )
    .unwrap();
    let time = Arc::new(AtomicI64::new(NOW as i64));
    let clock = time.clone();
    let state = AppState {
        store,
        identity: verifier_from_config(&cfg.auth, reqwest::Client::new()),
        config: Arc::new(cfg),
        clock: Arc::new(move || {
            Utc.timestamp_millis_opt(clock.load(Ordering::SeqCst))
                .unwrap()
        }),
        agent_access: None,
        source_retention: None,
    };
    Fixture {
        db,
        state,
        node: identity,
        profiled: state_projection,
        selection: selected,
        decision: Uuid::new_v4(),
        actor: owner.id,
        credential,
        time,
    }
}
/// Synthetic full authority shape from an actual selected value. This helper
/// supplies no public graph/source owner qualification by itself.
pub fn state_from_selection(
    selected: &aq_selection::ResolvedSelection,
    identity: &NodeIdentity,
) -> ProfiledState {
    let shared: Value = serde_json::from_str(include_str!(
        "../../../../vendor/aq-session-protocol-0.1.0/artifacts/fixtures.json"
    ))
    .unwrap();
    let mut offer: ProfiledOffer = serde_json::from_value(
        shared["valid_documents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["kind"] == "launch")
            .unwrap()["body"]
            .clone(),
    )
    .unwrap();
    let session = SessionIdentity::Remote {
        node_id: identity.node_id,
        boot_id: identity.boot_id,
        attempt_id: Uuid::new_v4(),
        assignment_generation: 1,
        session_id: Uuid::new_v4(),
    };
    let SessionIdentity::Remote { attempt_id, .. } = session else {
        unreachable!()
    };
    offer.binding.session = session.clone();
    offer.binding.attempt_id = attempt_id;
    offer.binding.offer_id = Uuid::new_v4();
    offer.manifest.bindings.session = session;
    offer.manifest.bindings.attempt_id = attempt_id;
    offer.manifest.bindings.project_id = selected.lineage.project_id;
    offer.manifest.bindings.graph_id = selected.lineage.workspace_id;
    offer.manifest.bindings.task_id = selected.lineage.task_id;
    offer.manifest.catalog = ArtifactRef {
        id: Text::new("profile-catalog").unwrap(),
        version: Text::new("1").unwrap(),
        digest: selected.catalog_digest.clone(),
    };
    offer.manifest.bindings.project_revision = selected.project_revision;
    let choice = &selected.choice;
    offer.manifest.model.provider = Text::new(&choice.provider).unwrap();
    offer.manifest.model.model = Text::new(&choice.model).unwrap();
    offer.manifest.model.reasoning = Text::new(&choice.native_reasoning).unwrap();
    offer.manifest.model.max_output_tokens = choice.max_output_tokens;
    offer.manifest.harness.harness = Text::new(&choice.harness).unwrap();
    offer.manifest.harness.cli_version = Text::new(&choice.cli_version).unwrap();
    offer.manifest.harness.adapter = choice.adapter.clone();
    offer.manifest.harness.environment = choice.environment.clone();
    offer.manifest.resources.memory_bytes = choice.resources.memory_bytes;
    offer.manifest.resources.cpu_millis = choice.resources.cpu_millis;
    offer.manifest.resources.process_limit = choice.resources.process_limit;
    offer.manifest.resources.wall_time_ms = choice.resources.wall_time_ms;
    offer.manifest.context.reserved_output_tokens = choice.max_output_tokens;
    offer.manifest.credentials = vec![SecretRef {
        credential_profile: choice.credential_profile.clone(),
        purpose: Text::new("model-authentication").unwrap(),
    }];
    let graph_artifact = artifact("graph-profile", b"synthetic-frozen-graph-profile");
    offer.manifest.bindings.graph_access_profile = graph_artifact.clone();
    offer.binding.graph_access_profile_digest = graph_artifact.digest.clone();
    let spec = json!({"schema_version":1,"kind":"synthetic_profiled_authority_fixture","task_id":selected.lineage.task_id});
    offer.binding.execution_spec_digest = aq_selection::digest(&spec).unwrap();
    offer.binding.launch_manifest_digest = canonical_digest(&offer.manifest).unwrap();
    let source = SourceBinding {
        repository_id: Uuid::new_v4(),
        binding_generation: 1,
        logical_identity: "fixture/repository".into(),
        base: ObjectId {
            algorithm: ObjectAlgorithm::Sha1,
            hex: "a".repeat(40),
        },
    };
    let graph = GraphBinding {
        workspace_id: selected.lineage.workspace_id,
        profile_id: graph_artifact.id.as_str().into(),
        profile_revision: graph_artifact.version.as_str().into(),
        profile_digest: graph_artifact.digest.as_str().into(),
        context_digest: Digest::of(b"graph-context").as_str().into(),
    };
    let state_projection = ProfiledState {
        schema_version: 1,
        offer,
        status: Status::Offered,
        grant: None,
        execution_deadline_ms: None,
        source,
        graph,
        cancellation_id: None,
        revoked: false,
        stop: Stop::Unknown,
        retention: Retention::Pending,
        effects: Effects::Unknown,
    };
    state_projection.validate().unwrap();
    state_projection
}
pub async fn insert(f: &Fixture) -> Uuid {
    let spec = json!({"schema_version":1,"kind":"synthetic_profiled_authority_fixture","task_id":f.selection.lineage.task_id});
    insert_spec(f, spec, false, false, None).await
}
/// Like `insert`, with the native offer clock recorded on the launch so the
/// lineage history used by failed-attempt replacement is known.
pub async fn insert_offered(f: &Fixture, offered_at_ms: u64) -> Uuid {
    let spec = json!({"schema_version":1,"kind":"synthetic_profiled_authority_fixture","task_id":f.selection.lineage.task_id});
    insert_spec(f, spec, false, false, Some(offered_at_ms)).await
}
pub async fn insert_typed(
    f: &Fixture,
    spec: &aq_admission::execution_spec::ProfiledExecutionSpec,
) -> Uuid {
    spec.validate_against(&f.profiled.offer).unwrap();
    insert_spec(f, json!(spec), false, false, None).await
}
pub async fn insert_sibling(f: &Fixture) -> Uuid {
    let spec = json!({"schema_version":1,"kind":"synthetic_profiled_authority_fixture","task_id":f.selection.lineage.task_id});
    insert_spec(f, spec, true, false, None).await
}
pub async fn insert_with_retained_decision(
    f: &Fixture,
    spec: &aq_admission::execution_spec::ProfiledExecutionSpec,
) -> Uuid {
    spec.validate_against(&f.profiled.offer).unwrap();
    let old: Value =
        sqlx::query_scalar("SELECT decision FROM intelligence_decisions WHERE decision_id=$1")
            .bind(f.decision)
            .fetch_one(&f.db.pool().await)
            .await
            .unwrap();
    assert_eq!(old["outcome"]["resolved"], json!(f.selection));
    insert_spec(f, json!(spec), false, true, None).await
}
async fn insert_spec(
    f: &Fixture,
    spec: Value,
    reuse_project: bool,
    retained_decision: bool,
    offered_at_ms: Option<u64>,
) -> Uuid {
    let now = if retained_decision {
        f.time.load(Ordering::SeqCst) as u64
    } else {
        NOW
    };
    let pool = f.db.pool().await;
    let p = &f.profiled;
    let binding = &p.offer.binding;
    let SessionIdentity::Remote { session_id, .. } = binding.session else {
        unreachable!()
    };
    if !retained_decision {
        sqlx::query("INSERT INTO intelligence_decisions(decision_id,actor_id,operation_id,payload_digest,decision) VALUES($1,$2,$3,$4,$5)").bind(f.decision).bind(f.actor).bind(Uuid::new_v4()).bind(Digest::of(b"synthetic-request").as_str()).bind(json!({"outcome":{"resolved":f.selection}})).execute(&pool).await.unwrap();
    }
    sqlx::query("INSERT INTO execution_attempts(attempt_id,task_id,node_id,offer_id,session_id,kind,state,execution_deadline_ms) VALUES($1,$2,$3,$4,$5,'profiled_v1',$6,$7)").bind(binding.attempt_id).bind(f.selection.lineage.task_id).bind(f.node.node_id).bind(binding.offer_id).bind(session_id).bind(json!(p)).bind(p.execution_deadline_ms.map(|v|v as i64)).execute(&pool).await.unwrap();
    // A real admission stores real owner observations. These synthetic table rows
    // exercise the same concrete decoder and manifest/selection digest checks.
    let project = json!({"schema_version":1,"project_id":f.selection.lineage.project_id,"name":"Synthetic authority fixture","enabled":true,"repository_id":p.source.repository_id,"repository_binding_generation":1,"repository_adapter_reference":"aq.git.configured","graph_adapter_reference":"aq.graph.configured","graph_workspace_id":f.selection.lineage.workspace_id,"catalog_digest":f.selection.catalog_digest,"allowed_agent_profiles":["implementer@2"],"allowed_intelligence_profiles":["engineering@2"],"allowed_execution_profiles":["confined-development@2"],"allowed_node_ids":[f.node.node_id],"allowed_worker_groups":[],"allowed_environments":["confined-worktree"],"policy":{"identity":"synthetic-policy","revision":"1","digest":Digest::of(b"synthetic-policy")},"context":[],"ceilings":{"permissions":[],"context_bytes":8192,"wall_time_ms":60000,"memory_bytes":268435456,"cpu_millis":60000,"process_limit":8,"cost_limit_microunits":null,"currency":null,"require_hard_cost_enforcement":false}});
    let typed: aq_server::project_bindings::domain::ProjectBinding =
        serde_json::from_value(project.clone()).unwrap();
    typed.validate(&Default::default()).unwrap();
    let observed = aq_server::project_bindings::domain::BindingObservation {
        repository_id: p.source.repository_id,
        graph_workspace_id: f.selection.lineage.workspace_id,
        repository_adapter_reference: "aq.git.configured".into(),
        graph_adapter_reference: "aq.graph.configured".into(),
        repository: aq_server::project_bindings::domain::ExternalStatus::Verified,
        graph: aq_server::project_bindings::domain::ExternalStatus::Verified,
        repository_generation: Some(1),
        observed_at_ms: now,
        expires_at_ms: now + 30_000,
    };
    if reuse_project {
        let existing: Value = sqlx::query_scalar(
            "SELECT binding FROM project_execution_bindings WHERE project_id=$1",
        )
        .bind(f.selection.lineage.project_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            existing, project,
            "sibling fixture must reuse the exact sealed project"
        );
    } else {
        sqlx::query("INSERT INTO project_execution_bindings(project_id,revision,binding,validated_at_ms,binding_observation) VALUES($1,1,$2,$3,$4) ON CONFLICT(project_id) DO NOTHING").bind(f.selection.lineage.project_id).bind(project).bind(now as i64).bind(json!(observed)).execute(&pool).await.unwrap();
    }
    let observation = if retained_decision {
        serde_json::from_value::<aq_admission::execution_spec::ProfiledExecutionSpec>(spec.clone())
            .unwrap()
            .source
    } else {
        aq_admission::SourceAdmission {
            schema_version: 1,
            project_id: f.selection.lineage.project_id,
            project_revision: 1,
            repository_id: p.source.repository_id,
            repository_binding_generation: 1,
            policy: artifact("synthetic-source-policy", b"immutable-base-policy"),
            selection: aq_admission::SourceSelection::Immutable {
                object: aq_admission::ObjectId {
                    format: aq_admission::ObjectFormat::Sha1,
                    hex: p.source.base.hex.clone(),
                },
            },
            resolved_object: aq_admission::ObjectId {
                format: aq_admission::ObjectFormat::Sha1,
                hex: p.source.base.hex.clone(),
            },
            observed_at_ms: now,
            expires_at_ms: now + 30_000,
        }
    };
    observation.validate().unwrap();
    let source_observation = Uuid::new_v4();
    sqlx::query("INSERT INTO admission_source_observations(observation_id,project_id,source_policy_revision,observation,observed_at_ms,expires_at_ms) VALUES($1,$2,1,$3,$4,$5)").bind(source_observation).bind(f.selection.lineage.project_id).bind(json!(observation)).bind(now as i64).bind((now+30_000) as i64).execute(&pool).await.unwrap();
    let mut observation = json!({"kind":"synthetic-readiness-observation","task_id":f.selection.lineage.task_id,"version":1,"content_revision":1,"observed_at_ms":now,"expires_at_ms":now+30_000});
    if let Some(ms) = offered_at_ms {
        observation["native_offer"] = json!({"offered":{"clock":{"server_time_ms":ms}}});
    }
    sqlx::query("INSERT INTO profiled_launches(attempt_id,decision_id,source_observation_id,launch_binding,launch_manifest,manifest_digest,execution_spec,execution_spec_digest,graph_task_observation,source_binding,offer_expires_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)").bind(binding.attempt_id).bind(f.decision).bind(source_observation).bind(json!(binding)).bind(json!(p.offer.manifest)).bind(binding.launch_manifest_digest.as_str()).bind(spec).bind(binding.execution_spec_digest.as_str()).bind(observation).bind(json!(p.source)).bind((now+30_000) as i64).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO intelligence_attempt_selections(attempt_id,selection_digest,selection) VALUES($1,$2,$3)").bind(binding.attempt_id).bind(aq_selection::digest(&f.selection).unwrap().as_str()).bind(json!(f.selection)).execute(&pool).await.unwrap();
    binding.attempt_id
}
