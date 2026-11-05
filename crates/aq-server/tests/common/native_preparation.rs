//! Synthetic qualified native authority with a REAL resolver decision, registry,
//! policy installer and private file owner. Not full offer assembly qualification.
use super::{
    profiled::{self, Fixture},
    selection::{self, SelectionFixture},
};
use aq_admission::{
    ArtifactRef, Digest, ObjectFormat, SourceAdmission, SourceSelection, Text,
    transport::{
        Accept, Feature, NoLaunchReservation, Register, TransportDocument, WorkerIdentity,
    },
};
use aq_server::{
    intelligence::resolver::{self, ResolveRequest},
    profiled_execution::{
        config::{ProfiledConfig, ProjectPolicy, QualifiedRuntime},
        policies, source, source_exports,
    },
    source_retention::{Runtime, SourceRetentionConfig},
};
use serde_json::{Value, json};
use std::sync::Arc;
use uuid::Uuid;
pub struct Case {
    pub f: Fixture,
    pub request: Accept,
    pub env: SelectionFixture,
    pub peer: tokio::task::JoinHandle<()>,
    pub pause: Arc<std::sync::atomic::AtomicBool>,
    pub entered: Arc<tokio::sync::Notify>,
    pub resume: Arc<tokio::sync::Notify>,
}
impl Drop for Case {
    fn drop(&mut self) {
        self.peer.abort();
    }
}
pub async fn setup() -> Case {
    setup_with_capabilities(true, true).await
}
pub async fn setup_with_capabilities(qualified_required: bool, registered_journal: bool) -> Case {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tower::ServiceExt;
    let mut env = selection::selection_fixture().await;
    let aq_selection::ExecutionTarget::Worker { node_id, boot_id } = env.input.node.target else {
        unreachable!()
    };
    let identity = aq_worker_protocol::NodeIdentity { node_id, boot_id };
    let shared: Value = serde_json::from_str(include_str!(
        "../../../../vendor/aq-session-protocol-0.1.0/artifacts/fixtures.json"
    ))
    .unwrap();
    let prototype: aq_session_protocol::ProfiledOffer = serde_json::from_value(
        shared["valid_documents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["kind"] == "launch")
            .unwrap()["body"]
            .clone(),
    )
    .unwrap();
    let mut capabilities = prototype.required_capabilities.clone();
    for c in &prototype.manifest.harness.required_capabilities {
        if !capabilities.contains(c) {
            capabilities.push(c.clone());
        }
    }
    let runtime = profiled::artifact("synthetic-preparation-runtime", b"component runtime");
    let qualification = profiled::artifact(
        "synthetic-preparation-qualification",
        b"component qualification only",
    );
    let delivery = aq_selection::DeliveryCapability {
        adapter: profiled::artifact("synthetic-material-delivery", b"delivery adapter"),
        version: 1,
    };
    let mut features = vec![
        Feature::ArtifactTransferV1,
        Feature::ServerMaterialV1,
        Feature::DurableHostJournalV1,
    ];
    if !registered_journal {
        features.retain(|f| *f != Feature::DurableHostJournalV1);
    }
    let mut qualified_capabilities = capabilities.clone();
    if !qualified_required {
        qualified_capabilities.pop();
    }
    let retention_handle = Uuid::new_v4();
    let mut config = (*env.env.state.config).clone();
    let mut intelligence = config.intelligence.as_ref().unwrap().config.clone();
    intelligence.revision += 1;
    for c in &mut intelligence.credentials {
        if c.allowed_node_ids.contains(&node_id) {
            c.delivery_mode =
                Some(aq_server::intelligence::config::MaterialDeliveryMode::ServerMaterialV1);
        }
    }
    config.intelligence = Some(Arc::new(intelligence.load().unwrap()));
    config.profiled_execution = Some(Arc::new(ProfiledConfig {
        revision: 1,
        definition_bodies: vec![],
        context_qualifications: vec![],
        qualified_runtimes: vec![QualifiedRuntime {
            runtime: runtime.clone(),
            qualification,
            capabilities: qualified_capabilities,
            features: features.clone(),
            material_delivery: Some(delivery),
        }],
        projects: vec![ProjectPolicy {
            project_id: env.lineage.project_id,
            enabled: true,
            priority: 0,
            source: SourceSelection::Immutable {
                object: aq_admission::ObjectId {
                    format: ObjectFormat::Sha1,
                    hex: "a".repeat(40),
                },
            },
            retention_handle,
            placement: None,
        }],
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
        axum::http::StatusCode::OK
    );
    policies::install(&env.env.state).await.unwrap();
    let register = Register {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: WorkerIdentity { node_id, boot_id },
        runtime,
        capabilities,
        features,
    };
    let response = aq_server::build_worker_router(env.env.state.clone())
        .oneshot(
            axum::http::Request::post("/v1/worker/profiled/register")
                .header("authorization", format!("Bearer {}", env.env.credential))
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    serde_json::to_vec(&TransportDocument::Register(register)).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(response.status().is_success());
    let decision = resolver::resolve_for_project_policy(
        &env.env.state,
        ResolveRequest {
            operation_id: env.operation,
            lineage: env.lineage.clone(),
            project: env.frozen.clone(),
            compositions: env.input.compositions.clone(),
            replacement: None,
        },
    )
    .await
    .unwrap();
    let selected = decision.outcome.resolved.unwrap();
    let mut state = profiled::state_from_selection(&selected, &identity);
    state.source.repository_id = env.frozen.project.binding.repository_id;
    state.source.binding_generation = env.frozen.project.binding.repository_binding_generation;
    let mut f = Fixture {
        db: env.env._db.clone(),
        state: env.env.state.clone(),
        node: identity,
        profiled: state,
        selection: selected,
        decision: decision.decision_id,
        actor: env.actor,
        credential: env.env.credential.clone(),
        time: env.env.time.clone(),
    };
    let now = f.time.load(std::sync::atomic::Ordering::SeqCst) as u64;
    let policy = f
        .state
        .config
        .profiled_execution
        .as_ref()
        .unwrap()
        .project_policy_record(env.lineage.project_id)
        .unwrap();
    let admission = SourceAdmission {
        schema_version: 1,
        project_id: env.lineage.project_id,
        project_revision: 1,
        repository_id: f.profiled.source.repository_id,
        repository_binding_generation: 1,
        policy: policy.artifact().unwrap(),
        selection: policy.source,
        resolved_object: aq_admission::ObjectId {
            format: ObjectFormat::Sha1,
            hex: f.profiled.source.base.hex.clone(),
        },
        observed_at_ms: now,
        expires_at_ms: now + 30000,
    };
    let mut body = json!({"schema_version":1,"admission":admission,"retention_handle":retention_handle,"evidence":{"commit_inspection":{"oid":{"algorithm":"sha1","hex":f.profiled.source.base.hex},"tree":{"algorithm":"sha1","hex":"b".repeat(40)}},"retention_inspection":{"version":1,"id":retention_handle,"repository_id":admission.repository_id,"binding_generation":1,"generation":1,"state":"active","refs":[{"name":"refs/aq-git/checkpoints/synthetic","oid":f.profiled.source.base.hex}],"release_operation":null,"release_reason":null}}});
    body.sort_all_objects();
    let bytes = serde_json::to_vec(&body).unwrap();
    let artifact = ArtifactRef {
        id: Text::new(format!(
            "aq.source-admission-observation/{}",
            Digest::of(&bytes).as_str()
        ))
        .unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(&bytes),
    };
    let observation = source::Observation {
        admission: admission.clone(),
        retention_handle,
        artifact: artifact.clone(),
        bytes: bytes.clone(),
    };
    aq_server::profiled_execution::artifacts::retain(
        &f.state,
        aq_server::profiled_execution::artifacts::Retain {
            artifact_id: Uuid::new_v4(),
            project_id: admission.project_id,
            owner: aq_server::profiled_execution::artifacts::OwnerKind::SourceRetention,
            artifact: artifact.clone(),
            kind: aq_admission::ArtifactKind::Context,
            bytes: &bytes,
            retain_until_ms: now + 600000,
        },
    )
    .await
    .unwrap();
    source::retain_admission(&f.state, Uuid::new_v4(), &observation, now + 600000)
        .await
        .unwrap();
    let socket = env.env._dir.path().join("preparation-source.sock");
    f.state.source_retention = Some(Arc::new(
        Runtime::new(SourceRetentionConfig {
            keep_per_task: 2,
            max_age: 30 * 86400,
            git_socket: socket.clone(),
            git_root: env.env._dir.path().join("preparation-git"),
            staging_root: env.env._dir.path().join("preparation-stage"),
            max_bundle_bytes: 1048576,
            max_staging_bytes: 4194304,
            bulk_concurrency: 2,
            incomplete_ttl_secs: 60,
        })
        .unwrap(),
    ));
    let reserve = source_exports::Request {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        project_id: admission.project_id,
        project_revision: 1,
        repository_id: admission.repository_id,
        logical_identity: f.profiled.source.logical_identity.clone(),
        binding_generation: 1,
        base: f.profiled.source.base.clone(),
        source_retention_handle: retention_handle,
        source_observation: artifact.clone(),
    };
    let source_exports::Reservation::Pending(token) =
        source_exports::reserve(&f.state, reserve).await.unwrap()
    else {
        panic!()
    };
    let fact = super::source_exports::synthetic_fact_at(&token, now);
    source_exports::finalize(&f.state, &token, &fact)
        .await
        .unwrap();
    let spec = super::source_exports::typed_spec(&mut f, &admission, &artifact);
    profiled::insert_with_retained_decision(&f, &spec).await;
    let pool = f.db.pool().await;
    sqlx::query("INSERT INTO profiled_authority_hot(attempt_id,state,record_revision,authority_revision,grant_revision) VALUES($1,$2,1,1,0)").bind(f.profiled.offer.binding.attempt_id).bind(json!(aq_server::execution::authority::hot::HotState::from_compat(&f.profiled).unwrap())).execute(&pool).await.unwrap();
    let value: Value = serde_json::from_slice(&fact.canonical_bytes().unwrap()).unwrap();
    let retention = value["retention_inspection"].clone();
    let inspection=serde_json::from_value(json!({"request_digest":token.request_digest(),"export_operation_id":token.request().operation_id,"inspection":retention,"observed_at_ms":now,"expires_at_ms":now+30000})).unwrap();
    let mut tx = pool.begin().await.unwrap();
    source_exports::link_locked(
        &f.state,
        &mut tx,
        &f.profiled.offer.binding.session,
        &token,
        &fact,
        &inspection,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let listener = tokio::net::UnixListener::bind(socket).unwrap();
    let pause = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let entered = Arc::new(tokio::sync::Notify::new());
    let resume = Arc::new(tokio::sync::Notify::new());
    let (peer_pause, peer_entered, peer_resume) = (pause.clone(), entered.clone(), resume.clone());
    let peer = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let mut line = String::new();
            stream.read_line(&mut line).await.unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["operation"], "retention.inspect");
            if peer_pause.load(std::sync::atomic::Ordering::SeqCst) {
                peer_entered.notify_one();
                peer_resume.notified().await;
            }
            let mut bytes=serde_json::to_vec(&json!({"protocol_version":1,"request_id":request["request_id"],"ok":true,"result":retention})).unwrap();
            bytes.push(b'\n');
            stream.get_mut().write_all(&bytes).await.unwrap();
        }
    });
    let request = Accept {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: WorkerIdentity { node_id, boot_id },
        acceptance: aq_session_protocol::ProfiledAcceptance {
            binding: f.profiled.offer.binding.clone(),
            advertised_capabilities: f.profiled.offer.required_capabilities.clone(),
        },
        reservation: NoLaunchReservation {
            reservation_id: Uuid::new_v4(),
            reservation_operation_id: Uuid::new_v4(),
            reservation_journal_digest: Digest::of(b"synthetic durable no-launch reservation"),
        },
        guardian_id: Uuid::new_v4(),
        launch_id: Uuid::new_v4(),
        journal_revision: 1,
        journal_digest: Digest::of(b"synthetic durable reserved journal"),
        reservation_remaining_ms: 10000,
    };
    Case {
        f,
        request,
        env,
        peer,
        pause,
        entered,
        resume,
    }
}
pub async fn context(case: &Case) -> aq_server::worker_api::AuthenticatedNode {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "authorization",
        format!("Bearer {}", case.f.credential).parse().unwrap(),
    );
    aq_server::worker_api::authenticate_context(&case.f.state, &headers)
        .await
        .unwrap()
}
