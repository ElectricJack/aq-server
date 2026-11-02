//! Synthetic qualified owners, actual public Graph/private Git I/O and SQL/router
//! assembly. No provider, real model or physical runtime qualification is claimed.
use super::selection::{self, SelectionFixture};
use aq_admission::{
    ArtifactKind, ArtifactRef, Digest,
    control::{ControlReply, decode_reply},
    transport::{Feature, Poll, Register, TransportDocument, WorkerIdentity},
};
use aq_server::{
    AppState,
    profiled_execution::{
        config::{ProfiledConfig, ProjectPolicy, QualifiedRuntime},
        context_budget::{CapacityKind, Qualification, Rendering, Subject},
        definitions::Definition,
        policies,
    },
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};
use tower::ServiceExt;
use uuid::Uuid;
pub struct Case {
    pub env: SelectionFixture,
    pub request: Poll,
    pub mode: Arc<AtomicU8>,
    pub controlled_clock: Option<Arc<std::sync::atomic::AtomicI64>>,
    pub observation_entered: Arc<tokio::sync::Semaphore>,
    pub observation_resume: Arc<tokio::sync::Semaphore>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}
impl Drop for Case {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}
fn art(id: &str, body: &str) -> ArtifactRef {
    super::profiled::artifact(id, body.as_bytes())
}
/// Actual-owner base only. No synthetic model registry, node snapshot, runtime,
/// Graph/Git peer, heartbeat, inventory, health or Poll is installed by this path.
/// The outer caller owns real process evidence and actual recipe qualification.
pub async fn setup_actual(
    input: &selection::ActualSetup,
) -> anyhow::Result<selection::ActualSelectionFixture> {
    selection::setup_actual(input).await
}
pub async fn setup() -> Case {
    let mut env = selection::selection_fixture().await;
    let pool = env.env.pool.clone();
    // This lane intentionally installs the frozen guard explicitly. Ordered
    // registration26/27 is independently owned/qualified by the schema lane.
    let schema: String =
        sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
            .fetch_one(&pool)
            .await
            .unwrap();
    if schema.parse::<u32>().unwrap() < 27 {
        sqlx::raw_sql(include_str!(
            "../../migrations/0027_native_definition_identities.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
    }
    let mut tasks = vec![];
    let mode = Arc::new(AtomicU8::new(0));
    let task = env.lineage.task_id;
    let workspace = env.lineage.workspace_id;
    let repository = env.frozen.project.binding.repository_id;
    let node = json!({"id":task,"workspace_id":workspace,"kind":"task","version":1,"content_revision":1,"title":"Synthetic native admission task","body":"Read the scoped source and report evidence.","acceptance_criteria":"Report exact inspected revisions.","labels":[],"metadata":{},"task_state":"active","verification":"unverified","provenance":[],"retired":false,"deleted":false,"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z","created_by":"fixture","last_verified_by":null,"last_verified_at":null,"retirement_reason":null,"content_type":null,"summary":null,"summary_of_revision":null,"valid_from":null,"valid_until":null,"review_at":null,"successor_id":null});
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let peer_mode = mode.clone();
    let observation_entered = Arc::new(tokio::sync::Semaphore::new(0));
    let observation_resume = Arc::new(tokio::sync::Semaphore::new(0));
    let peer_entered = observation_entered.clone();
    let peer_resume = observation_resume.clone();
    let app = axum::Router::new().fallback(axum::routing::get(move |uri: axum::http::Uri| {
        let mut node = node.clone();
        let mode = peer_mode.load(Ordering::SeqCst);
        let entered = peer_entered.clone();
        let resume = peer_resume.clone();
        async move {
            if mode == 6 && uri.path().ends_with("/ready") {
                // Exact external-observation boundary; normal fixtures never arm
                // this mode. Permits prevent lost notifications in race tests.
                entered.add_permits(1);
                tokio::time::timeout(std::time::Duration::from_secs(2), resume.acquire())
                    .await
                    .expect("bounded synthetic observation resume")
                    .unwrap()
                    .forget();
            }
            if mode == 4 {
                node["body"] = json!("changed authorized root without CAS increment");
            }
            if mode == 5 {
                node["metadata"] = json!({"private_daemon_note":"not part of startup"});
            }
            let value = if uri.path().ends_with("/ready") {
                json!({"items":if mode == 3 { vec![] } else { vec![node] },"next_cursor":null})
            } else if uri.path().ends_with("/context") {
                if mode == 1 {
                    node["version"] = json!(2);
                }
                if mode == 2 {
                    node["body"] = json!("different content without ready CAS");
                }
                json!({"node":node,"nodes":[],"edges":[],"pinned_revisions":[],"truncated":false})
            } else {
                json!({"items":[],"next_cursor":null})
            };
            ([("X-AQ-Graph-API-Version", "1")], axum::Json(value))
        }
    }));
    tasks.push(tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    }));
    let token = env.env._dir.path().join("graph-token");
    env.env.state.agent_access = Some(Arc::new(
        aq_server::agent_access::Runtime::new(&aq_server::agent_access::AgentAccessConfig {
            graph_url: format!("http://{addr}"),
            graph_token_file: token,
            credential_ttl_ms: 10000,
            mutation_actor_id: None,
        })
        .await
        .unwrap(),
    ));
    env.env.state.clock = Arc::new(chrono::Utc::now);
    let policy_body = "Synthetic owner policy: scoped read and mandatory review.";
    env.env.body["allowed_agent_profiles"] = json!(["reviewer@2"]);
    env.env.body["policy"] = json!({"identity":"native-test-policy","revision":"1","digest":Digest::of(policy_body.as_bytes())});
    env.env.app = aq_server::build_admin_router(env.env.state.clone());
    let (status, body) = selection::call(
        &env.env.app,
        &selection::path(),
        Some(json!({"operation_id":Uuid::new_v4(),"expected_revision":1,"binding":env.env.body})),
        "owner@localhost",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let runtime = art("native-test-runtime", "Synthetic runtime definition");
    let runtime_q = art(
        "native-test-runtime-qualification",
        "Synthetic runtime evidence",
    );
    let capabilities = [
        "aq.profiled-launch",
        "aq.terminal",
        "aq.harness.startup-context",
        "aq.harness.confined-worktree",
        "aq.harness.stop",
    ]
    .into_iter()
    .map(|id| aq_session_protocol::Capability {
        id: aq_admission::Text::new(id).unwrap(),
        version: 1,
    })
    .collect::<Vec<_>>();
    let features = vec![
        Feature::ArtifactTransferV1,
        Feature::ServerMaterialV1,
        Feature::DurableHostJournalV1,
    ];
    let harness = &env.input.node.harnesses[0];
    let credential = &env.input.credentials[0];
    let now = chrono::Utc::now().timestamp_millis() as u64;
    let count = art("native-test-counting", "Synthetic counting bound");
    let capacity = art("native-test-capacity", "Synthetic capacity bound");
    let fact = Qualification {
        schema_version: 1,
        subject: Subject {
            runtime: runtime.clone(),
            runtime_qualification: runtime_q.clone(),
            harness: harness.harness.clone(),
            cli_version: harness.cli_version.clone(),
            adapter: harness.adapter.clone(),
            environment: harness.environment.clone(),
            harness_qualification: harness.qualification.clone(),
            provider: credential.provider.clone(),
            model: "model-a".into(),
            native_reasoning: "high".into(),
            reasoning_configuration_reference: "reasoning-a@1".into(),
            credential_profile: credential.profile.clone(),
            credential_revision: credential.revision,
            account_reference: credential.account_reference.clone(),
            endpoint_reference: credential.endpoint_reference.clone(),
        },
        rendering: Rendering::Utf8DoubleNewlineV1,
        maximum_tokens_per_utf8_byte: 1,
        fixed_overhead_tokens: 37,
        counting_evidence: count.clone(),
        capacity_kind: CapacityKind::QualifiedConservativePlanningCeiling,
        capacity_tokens: 65536,
        capacity_evidence: capacity.clone(),
        observed_at_ms: now - 1,
        expires_at_ms: now + 60000,
    };
    let mut definitions = vec![];
    for (r, body) in [
        (runtime.clone(), "Synthetic runtime definition".to_owned()),
        (runtime_q.clone(), "Synthetic runtime evidence".to_owned()),
        (harness.adapter.clone(), harness.adapter.id.as_str().into()),
        (
            harness.environment.clone(),
            harness.environment.id.as_str().into(),
        ),
        (
            harness.qualification.clone(),
            harness.qualification.id.as_str().into(),
        ),
        (
            credential.profile.clone(),
            credential.profile.id.as_str().into(),
        ),
        (count, "Synthetic counting bound".into()),
        (capacity, "Synthetic capacity bound".into()),
        (art("native-test-policy", policy_body), policy_body.into()),
    ] {
        definitions.push(Definition::new(r, ArtifactKind::Definition, body).unwrap());
    }
    let handle = Uuid::new_v4();
    let mut config = (*env.env.state.config).clone();
    let mut intelligence = config.intelligence.as_ref().unwrap().config.clone();
    intelligence.revision += 1;
    for c in &mut intelligence.credentials {
        if c.allowed_node_ids.contains(&env.env.node) {
            c.delivery_mode =
                Some(aq_server::intelligence::config::MaterialDeliveryMode::ServerMaterialV1);
        }
    }
    config.intelligence = Some(Arc::new(intelligence.load().unwrap()));
    config.profiled_execution = Some(Arc::new(ProfiledConfig {
        revision: 1,
        qualified_runtimes: vec![QualifiedRuntime {
            runtime: runtime.clone(),
            qualification: runtime_q,
            capabilities: capabilities.clone(),
            features: features.clone(),
            material_delivery: Some(aq_selection::DeliveryCapability {
                adapter: art("native-test-material-delivery", "Delivery adapter"),
                version: 1,
            }),
        }],
        projects: vec![ProjectPolicy {
            project_id: env.lineage.project_id,
            enabled: true,
            priority: 0,
            source: aq_admission::SourceSelection::Immutable {
                object: aq_admission::ObjectId {
                    format: aq_admission::ObjectFormat::Sha1,
                    hex: "a".repeat(40),
                },
            },
            retention_handle: handle,
        }],
        context_qualifications: vec![fact],
        definition_bodies: definitions,
    }));
    env.env.state.config = Arc::new(config);
    env.env.app = aq_server::build_admin_router(env.env.state.clone());
    assert_eq!(
        selection::call(
            &env.env.app,
            "/api/intelligence/register",
            Some(json!({})),
            "owner@localhost"
        )
        .await
        .0,
        200
    );
    policies::install(&env.env.state).await.unwrap();
    let aq_selection::ExecutionTarget::Worker { node_id, boot_id } = env.input.node.target else {
        panic!()
    };
    let identity = WorkerIdentity { node_id, boot_id };
    let registration = Register {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: identity.clone(),
        runtime,
        capabilities,
        features,
    };
    let response = post(
        &env.env.state,
        &env.env.credential,
        "/v1/worker/profiled/register",
        serde_json::to_vec(&TransportDocument::Register(registration)).unwrap(),
    )
    .await;
    assert_eq!(response.status(), 200);
    let git = env.env._dir.path().join("native-git");
    let socket = env.env._dir.path().join("native-git.sock");
    std::fs::create_dir_all(&git).unwrap();
    let producer = Uuid::new_v4();
    std::fs::write(
        git.join("format.json"),
        json!({"format_version":1,"installation_id":producer}).to_string(),
    )
    .unwrap();
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let root = git.clone();
    tasks.push(tokio::spawn(async move{use tokio::io::{AsyncBufReadExt,AsyncWriteExt,BufReader};loop{let (stream,_)=listener.accept().await.unwrap();let root=root.clone();tokio::spawn(async move{
        let mut reader=BufReader::new(stream);let mut line=vec![];reader.read_until(b'\n',&mut line).await.unwrap();let r:Value=serde_json::from_slice(&line).unwrap();
        let result=match r["operation"].as_str().unwrap(){
            "repo.show"=>json!({"repository_id":repository,"binding_generation":1,"identity":"synthetic-native-repository","mode":"central","object_format":"sha1"}),
            "inspect"=>json!({"oid":{"algorithm":"sha1","hex":"a".repeat(40)},"tree":{"algorithm":"sha1","hex":"b".repeat(40)}}),
            "retention.inspect"=>{let id=r["params"]["handle_id"].as_str().unwrap();if id==handle.to_string(){json!({"version":1,"id":handle,"repository_id":repository,"binding_generation":1,"generation":1,"state":"active","refs":[{"name":"refs/aq-git/checkpoints/synthetic","oid":"a".repeat(40)}],"release_operation":null,"release_reason":null})}else{json!({"version":1,"id":id,"repository_id":repository,"binding_generation":1,"generation":1,"resource":"transfer","refs":[{"name":format!("refs/aq-git/transfers/{id}"),"oid":"a".repeat(40)}],"state":"active","dependencies":[],"release_operation":null,"release_reason":null})}},
            "artifact.export"=>{let op=r["operation_id"].as_str().unwrap();let dir=root.join("artifacts").join(op);std::fs::create_dir_all(&dir).unwrap();let bundle=b"synthetic bytes, not a qualified Git bundle";let manifest=json!({"format_version":1,"repository_id":repository,"repository_identity":"synthetic-native-repository","binding_generation":1,"transfer_id":op,"object_format":"sha1","base":r["params"]["base"],"source":r["params"]["source"],"prerequisites":[],"bundle_size":bundle.len(),"bundle_sha256":Digest::of(bundle),"producer_installation_id":producer,"reference_only":false,"caller_correlation":r["params"]["caller_correlation"]});std::fs::write(dir.join("objects.bundle"),bundle).unwrap();std::fs::write(dir.join("manifest.json"),serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();json!({"artifact_handle":op,"retention_handle":op,"manifest":manifest})},
            other=>panic!("unexpected private Git operation {other}"),
        };
        let mut response=json!({"protocol_version":1,"request_id":r["request_id"],"ok":true,"result":result});if let Some(op)=r.get("operation_id"){response["operation_id"]=op.clone();}let mut bytes=serde_json::to_vec(&response).unwrap();bytes.push(b'\n');if let Err(error)=reader.into_inner().write_all(&bytes).await {
            // A real refusal cancels sibling observations and closes its UDS.
            // Expected cancellation is not a successful response or peer panic.
            assert!(matches!(error.kind(),std::io::ErrorKind::BrokenPipe|std::io::ErrorKind::ConnectionReset),"unexpected synthetic Git response failure");
        }
    });}}));
    env.env.state.source_retention = Some(Arc::new(
        aq_server::source_retention::Runtime::new(
            aq_server::source_retention::SourceRetentionConfig {
                git_root: git,
                git_socket: socket,
                staging_root: env.env._dir.path().join("native-staging"),
                max_staging_bytes: 4_194_304,
                max_bundle_bytes: 1_048_576,
                bulk_concurrency: 1,
                incomplete_ttl_secs: 60,
            },
        )
        .unwrap(),
    ));
    Case {
        env,
        request: Poll {
            schema_version: 1,
            operation_id: Uuid::new_v4(),
            identity,
            inventory_sequence: 1,
            available: aq_admission::Resources {
                slots: 1,
                memory_bytes: 1_073_741_824,
                cpu_millis: 60000,
                disk_bytes: 2_097_152,
            },
            limit: 1,
            after_offer: None,
        },
        mode,
        controlled_clock: None,
        observation_entered,
        observation_resume,
        tasks,
    }
}
pub async fn post(
    state: &AppState,
    credential: &str,
    path: &str,
    bytes: Vec<u8>,
) -> axum::response::Response {
    aq_server::build_worker_router(state.clone())
        .oneshot(
            axum::http::Request::post(path)
                .header("authorization", format!("Bearer {credential}"))
                .header("content-type", "application/json")
                .body(axum::body::Body::from(bytes))
                .unwrap(),
        )
        .await
        .unwrap()
}
pub async fn poll(c: &Case) -> (axum::http::StatusCode, Value) {
    let r = post(
        &c.env.env.state,
        &c.env.env.credential,
        "/v1/worker/profiled/poll",
        serde_json::to_vec(&c.request).unwrap(),
    )
    .await;
    let status = r.status();
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}
/// Actual mounted publication, not a helper-inserted execution grant or wrapper.
pub async fn published() -> (Case, aq_admission::startup::NativeOffer) {
    let c = setup().await;
    let (status, body) = poll(&c).await;
    assert_eq!(status, 200, "{body}");
    let ControlReply::Poll(mut page) = decode_reply(&serde_json::to_vec(&body).unwrap()).unwrap()
    else {
        panic!("{body}")
    };
    page.validate_for(&c.request).unwrap();
    assert_eq!(page.offers.len(), 1);
    (c, page.offers.remove(0))
}
pub fn acceptance(
    c: &Case,
    native: &aq_admission::startup::NativeOffer,
) -> aq_admission::transport::Accept {
    aq_admission::transport::Accept {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: c.request.identity.clone(),
        acceptance: aq_session_protocol::ProfiledAcceptance {
            binding: native.offered.offer.binding.clone(),
            advertised_capabilities: native.offered.offer.required_capabilities.clone(),
        },
        reservation: aq_admission::transport::NoLaunchReservation {
            reservation_id: Uuid::new_v4(),
            reservation_operation_id: Uuid::new_v4(),
            reservation_journal_digest: Digest::of(
                b"synthetic durable no-launch reservation; no physical host qualification",
            ),
        },
        guardian_id: Uuid::new_v4(),
        launch_id: Uuid::new_v4(),
        journal_revision: 1,
        journal_digest: Digest::of(b"synthetic reserved owner journal"),
        reservation_remaining_ms: 10000,
    }
}
pub async fn accept(
    c: &Case,
    request: &aq_admission::transport::Accept,
) -> aq_admission::control::PreparationDelivery {
    let nonce = Uuid::new_v4();
    let response = aq_server::build_worker_router(c.env.env.state.clone())
        .oneshot(
            axum::http::Request::post("/v1/worker/profiled/accept")
                .header("authorization", format!("Bearer {}", c.env.env.credential))
                .header("content-type", "application/json")
                .header("aq-delivery-nonce", nonce.to_string())
                .body(axum::body::Body::from(serde_json::to_vec(request).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
    let ControlReply::Preparation(delivery) = decode_reply(&bytes).unwrap() else {
        panic!()
    };
    delivery.validate_for(request, nonce).unwrap();
    delivery
}
