//! Actual sealed profiled fixture qualification; no production grant seeding.
mod common;
use aq_server::execution::{
    authority::{self, AuthorityError, Kind, Policy},
    profiled::{Status, Stop},
};
use aq_session_protocol::{
    ArtifactRef, Digest, ProfiledGrant, SessionIdentity, Text, canonical_digest,
};
use aq_worker_protocol::{AttemptRef, ObjectAlgorithm, ObjectId, RegisterRequest};
use common::profiled::{NOW, fixture, insert};
use serde_json::{Value, json};
use sqlx::Row;
use std::sync::{Arc, atomic::Ordering};
use uuid::Uuid;
#[tokio::test]
async fn common_task_claim_and_legacy_decoders_cannot_cross_profiled_kind() {
    let f = fixture().await;
    let attempt = insert(&f).await;
    let legacy = aq_worker_protocol::Offer {
        offer_id: Uuid::new_v4(),
        identity: f.node.clone(),
        attempt: AttemptRef {
            attempt_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            generation: 1,
        },
        spec: aq_worker_protocol::ExecutionSpec {
            project_id: f.selection.lineage.project_id,
            harness: aq_worker_protocol::FAKE_HARNESS_CAPABILITY.into(),
            resolved_spec_digest: String::new(),
            fixture_output: "legacy-compatible".into(),
            resources: aq_worker_protocol::Resources {
                slots: 1,
                memory_bytes: 100,
                cpu_millis: 100,
                disk_bytes: 100,
            },
            required_capabilities: vec![aq_worker_protocol::FAKE_HARNESS_CAPABILITY.into()],
            bindings: None,
        }
        .seal(),
        offer_expires_at_ms: NOW + 10000,
        reservation_ttl_ms: 10000,
    };
    assert!(matches!(
        f.state
            .store
            .execution_offer(f.selection.lineage.task_id, &legacy)
            .await,
        Err(aq_server::execution::Error::Refused(
            aq_worker_protocol::ErrorCode::IdConflict,
            _
        ))
    ));
    assert!(matches!(
        f.state.store.execution_get(attempt).await,
        Err(aq_server::execution::Error::Refused(
            aq_worker_protocol::ErrorCode::NotFound,
            _
        ))
    ));
    let poll = f
        .state
        .store
        .execution_poll(
            &aq_worker_protocol::PollRequest {
                identity: f.node.clone(),
                operation_id: Uuid::new_v4(),
                inventory_sequence: 1,
                available: legacy.spec.resources.clone(),
                max_items: 8,
            },
            NOW,
        )
        .await
        .unwrap();
    assert!(poll.offers.is_empty());
    let mut replacement = f.node.clone();
    replacement.boot_id = Uuid::new_v4();
    let registration = RegisterRequest {
        identity: replacement,
        operation_id: Uuid::new_v4(),
        protocol_version: 2,
        worker_version: "0.1.0".into(),
        capabilities: vec![aq_worker_protocol::FAKE_HARNESS_CAPABILITY.into()],
    };
    assert!(matches!(
        f.state
            .store
            .execution_replace_incarnation(&registration)
            .await,
        Err(aq_server::execution::Error::Refused(
            aq_worker_protocol::ErrorCode::CapacityUnavailable,
            _
        ))
    ));
    let pool = f.db.pool().await;
    let held: bool =
        sqlx::query_scalar("SELECT holds_task FROM execution_attempts WHERE attempt_id=$1")
            .bind(attempt)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(held);
}
#[tokio::test]
async fn shared_view_fences_exact_seal_identity_live_time_and_revision_semantics() {
    let mut f = fixture().await;
    f.profiled.status = Status::Granted;
    f.profiled.execution_deadline_ms = Some(NOW + 60_000);
    f.profiled.grant = Some(ProfiledGrant {
        binding: f.profiled.offer.binding.clone(),
        grant_id: Uuid::new_v4(),
        expires_at_ms: NOW + 5000,
    });
    f.profiled.stop = Stop::Running;
    let attempt = insert(&f).await;
    let pool = f.db.pool().await;
    let mut tx = pool.begin().await.unwrap();
    let first = authority::load_locked(
        &f.state,
        &mut tx,
        &f.profiled.offer.binding.session,
        Policy::TerminalActive,
    )
    .await
    .unwrap();
    assert_eq!(first.kind, Kind::ProfiledV1);
    assert_eq!(
        first.grant_id,
        f.profiled.grant.as_ref().map(|g| g.grant_id)
    );
    assert_eq!(first.execution_deadline_ms, Some(NOW + 60_000));
    tx.rollback().await.unwrap();
    f.profiled.grant.as_mut().unwrap().expires_at_ms = NOW + 10_000;
    sqlx::query("UPDATE execution_attempts SET state=$2 WHERE attempt_id=$1")
        .bind(attempt)
        .bind(json!(f.profiled))
        .execute(&pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    let renewed = authority::load_locked(
        &f.state,
        &mut tx,
        &f.profiled.offer.binding.session,
        Policy::TerminalActive,
    )
    .await
    .unwrap();
    assert_eq!(renewed.authority_revision, first.authority_revision);
    assert!(renewed.record_revision > first.record_revision);
    assert!(renewed.grant_revision > first.grant_revision);
    tx.rollback().await.unwrap();
    let mut uncertain = f.profiled.clone();
    uncertain.stop = Stop::Unknown;
    sqlx::query("UPDATE execution_attempts SET state=$2 WHERE attempt_id=$1")
        .bind(attempt)
        .bind(json!(uncertain))
        .execute(&pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    assert!(matches!(
        authority::load_locked(
            &f.state,
            &mut tx,
            &f.profiled.offer.binding.session,
            Policy::TerminalActive
        )
        .await,
        Err(AuthorityError::Unsupported)
    ));
    tx.rollback().await.unwrap();
    sqlx::query("UPDATE execution_attempts SET state=$2 WHERE attempt_id=$1")
        .bind(attempt)
        .bind(json!(f.profiled))
        .execute(&pool)
        .await
        .unwrap();
    f.time.store((NOW + 10_000) as i64, Ordering::SeqCst);
    let mut tx = pool.begin().await.unwrap();
    assert!(matches!(
        authority::load_locked(
            &f.state,
            &mut tx,
            &f.profiled.offer.binding.session,
            Policy::TerminalActive
        )
        .await,
        Err(AuthorityError::Expired)
    ));
    tx.rollback().await.unwrap();
    f.time.store(NOW as i64, Ordering::SeqCst);
    let mut foreign = f.profiled.offer.binding.session.clone();
    if let SessionIdentity::Remote { boot_id, .. } = &mut foreign {
        *boot_id = Uuid::new_v4();
    }
    let mut tx = pool.begin().await.unwrap();
    assert!(matches!(
        authority::load_locked(&f.state, &mut tx, &foreign, Policy::TerminalActive).await,
        Err(AuthorityError::Identity)
    ));
    tx.rollback().await.unwrap();
    f.profiled.status = Status::Cancelling;
    f.profiled.cancellation_id = Some(Uuid::new_v4());
    sqlx::query("UPDATE execution_attempts SET state=$2 WHERE attempt_id=$1")
        .bind(attempt)
        .bind(json!(f.profiled))
        .execute(&pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    assert!(matches!(
        authority::load_locked(
            &f.state,
            &mut tx,
            &f.profiled.offer.binding.session,
            Policy::TerminalActive
        )
        .await,
        Err(AuthorityError::Revoked)
    ));
    tx.rollback().await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    let historical = authority::load_locked(
        &f.state,
        &mut tx,
        &f.profiled.offer.binding.session,
        Policy::TerminalHistorical,
    )
    .await
    .unwrap();
    assert!(historical.authority_revision > renewed.authority_revision);
    tx.rollback().await.unwrap();
}
#[tokio::test]
async fn migration_freezes_common_identity_profiled_boot_generation_deadline_and_immutable_seal() {
    let f = fixture().await;
    let attempt = insert(&f).await;
    let pool = f.db.pool().await;
    for query in [
        "UPDATE execution_attempts SET kind='legacy_fixture' WHERE attempt_id=$1",
        "UPDATE execution_attempts SET session_id=gen_random_uuid() WHERE attempt_id=$1",
        "UPDATE execution_attempts SET state=jsonb_set(state,'{offer,binding,assignment_generation}','2') WHERE attempt_id=$1",
    ] {
        assert!(
            sqlx::query(query)
                .bind(attempt)
                .execute(&pool)
                .await
                .is_err()
        );
    }
    assert!(
        sqlx::query("UPDATE profiled_launches SET manifest_digest=$2 WHERE attempt_id=$1")
            .bind(attempt)
            .bind(Digest::of(b"changed").as_str())
            .execute(&pool)
            .await
            .is_err()
    );
    sqlx::query("UPDATE execution_attempts SET execution_deadline_ms=$2 WHERE attempt_id=$1")
        .bind(attempt)
        .bind((NOW + 5000) as i64)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        sqlx::query("UPDATE execution_attempts SET execution_deadline_ms=$2 WHERE attempt_id=$1")
            .bind(attempt)
            .bind((NOW + 6000) as i64)
            .execute(&pool)
            .await
            .is_err()
    );
    let row = sqlx::query("SELECT kind,state FROM execution_attempts WHERE attempt_id=$1")
        .bind(attempt)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("kind"), "profiled_v1");
    assert_eq!(row.get::<Value, _>("state"), json!(f.profiled));
}
#[tokio::test]
async fn actual_profiled_source_adapter_accepts_scoped_late_retention_and_refuses_foreign_spec() {
    use aq_server::source_retention::{Runtime, SourceRetentionConfig};
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let mut f = fixture().await;
    let attempt = insert(&f).await;
    let dir = tempfile::tempdir().unwrap();
    f.state.source_retention = Some(Arc::new(
        Runtime::new(SourceRetentionConfig {
            keep_per_task: 2,
            max_age: 30 * 86400,
            staging_root: dir.path().join("staging"),
            git_root: dir.path().join("git"),
            git_socket: dir.path().join("git.sock"),
            max_staging_bytes: 2 * 1024 * 1024,
            max_bundle_bytes: 4096,
            bulk_concurrency: 2,
            incomplete_ttl_secs: 1,
        })
        .unwrap(),
    ));
    let app = aq_server::build_worker_router(f.state.clone());
    let manifest = aq_worker_protocol::TransferManifest {
        transfer_id: Uuid::new_v4(),
        role: aq_worker_protocol::SourceRole::Checkpoint,
        checkpoint: ObjectId {
            algorithm: ObjectAlgorithm::Sha1,
            hex: "b".repeat(40),
        },
        attempt: match &f.profiled.offer.binding.session {
            SessionIdentity::Remote {
                attempt_id,
                session_id,
                assignment_generation,
                ..
            } => AttemptRef {
                attempt_id: *attempt_id,
                session_id: *session_id,
                generation: *assignment_generation,
            },
            _ => unreachable!(),
        },
        resolved_spec_digest: f
            .profiled
            .offer
            .binding
            .execution_spec_digest
            .as_str()
            .into(),
        source: f.profiled.source.clone(),
        artifact_id: Uuid::new_v4(),
        manifest_digest: Digest::of(b"synthetic-source-manifest").as_str().into(),
        bundle_digest: None,
        manifest_bytes: 128,
        bundle_bytes: 0,
    };
    let body = json!({"identity":f.node,"operation_id":Uuid::new_v4(),"manifest":manifest});
    let call = |body: Value| {
        let app = app.clone();
        let token = f.credential.clone();
        async move {
            let response = app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/execution/sources/begin")
                        .header("authorization", format!("Bearer {token}"))
                        .header("content-type", "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = response.status();
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            (status, serde_json::from_slice::<Value>(&bytes).unwrap())
        }
    };
    assert_eq!(call(body.clone()).await.0, StatusCode::FORBIDDEN);
    f.profiled.status = Status::Granted;
    f.profiled.execution_deadline_ms = Some(NOW + 60000);
    f.profiled.grant = Some(ProfiledGrant {
        binding: f.profiled.offer.binding.clone(),
        grant_id: Uuid::new_v4(),
        expires_at_ms: NOW,
    });

    sqlx::query(
        "UPDATE execution_attempts SET state=$2,execution_deadline_ms=$3 WHERE attempt_id=$1",
    )
    .bind(attempt)
    .bind(json!(f.profiled))
    .bind((NOW + 60000) as i64)
    .execute(&f.db.pool().await)
    .await
    .unwrap();
    let (status, first) = call(body.clone()).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let mut foreign = body.clone();
    foreign["operation_id"] = json!(Uuid::new_v4());
    foreign["manifest"]["resolved_spec_digest"] = json!(Digest::of(b"foreign-spec"));
    assert_eq!(call(foreign).await.0, StatusCode::FORBIDDEN);
    f.profiled.status = Status::Cancelling;
    f.profiled.cancellation_id = Some(Uuid::new_v4());
    sqlx::query("UPDATE execution_attempts SET state=$2 WHERE attempt_id=$1")
        .bind(attempt)
        .bind(json!(f.profiled))
        .execute(&f.db.pool().await)
        .await
        .unwrap();
    f.time.store((NOW + 60_000) as i64, Ordering::SeqCst);
    let (status, replay) = call(body).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replay, first);
}
#[tokio::test]
async fn authority_expiry_is_sampled_after_an_observed_node_lock_wait() {
    let mut f = fixture().await;
    f.profiled.status = Status::Granted;
    f.profiled.execution_deadline_ms = Some(NOW + 60_000);
    f.profiled.grant = Some(ProfiledGrant {
        binding: f.profiled.offer.binding.clone(),
        grant_id: Uuid::new_v4(),
        expires_at_ms: NOW + 5000,
    });
    f.profiled.stop = Stop::Running;
    insert(&f).await;
    let pool = f.db.pool().await;
    let mut held = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(f.node.node_id)
        .execute(&mut *held)
        .await
        .unwrap();
    let worker_pool = pool.clone();
    let state = f.state.clone();
    let expected = f.profiled.offer.binding.session.clone();
    let (pid_tx, pid_rx) = tokio::sync::oneshot::channel();
    let waiting = tokio::spawn(async move {
        let mut tx = worker_pool.begin().await.unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        pid_tx.send(pid).unwrap();
        let result =
            authority::load_locked(&state, &mut tx, &expected, Policy::TerminalActive).await;
        tx.rollback().await.unwrap();
        result
    });
    let pid = pid_rx.await.unwrap();
    let observer = f.db.pool().await;
    let mut blocked = false;
    for _ in 0..100 {
        let lock: bool = sqlx::query_scalar(
            "SELECT COALESCE(wait_event_type='Lock',false) FROM pg_stat_activity WHERE pid=$1",
        )
        .bind(pid)
        .fetch_one(&observer)
        .await
        .unwrap();
        if lock {
            blocked = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    assert!(
        blocked,
        "actual node lock wait must be observed before clock advance"
    );
    f.time.store((NOW + 5000) as i64, Ordering::SeqCst);
    held.rollback().await.unwrap();
    assert!(matches!(
        waiting.await.unwrap(),
        Err(AuthorityError::Expired)
    ));
}
#[tokio::test]
async fn actual_profiled_graph_adapter_issues_exact_api01_binding_and_revokes_on_current_authority()
{
    use aq_server::agent_access::{AgentAccessConfig, Runtime};
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use http_body_util::BodyExt;
    use std::os::unix::fs::PermissionsExt;
    use tower::ServiceExt;
    let mut f = fixture().await;
    let api: Value = serde_json::from_str(include_str!(
        "../../../protocol/agent-access/v1/fixtures/base.json"
    ))
    .unwrap();
    let profile = api["context"]["profile"].clone();
    let context = api["context"]["context"].clone();
    let profile_digest = aq_agent_api::digest(&profile).unwrap();
    let context_digest = aq_agent_api::digest(&context).unwrap();
    f.profiled.graph.profile_id = profile["profile_id"].as_str().unwrap().into();
    f.profiled.graph.profile_revision = profile["revision"].as_str().unwrap().into();
    f.profiled.graph.profile_digest = profile_digest.clone();
    f.profiled.graph.context_digest = context_digest.clone();
    f.profiled.offer.manifest.bindings.graph_access_profile = ArtifactRef {
        id: Text::new(&f.profiled.graph.profile_id).unwrap(),
        version: Text::new(&f.profiled.graph.profile_revision).unwrap(),
        digest: serde_json::from_value(json!(profile_digest)).unwrap(),
    };
    f.profiled.offer.binding.graph_access_profile_digest = f
        .profiled
        .offer
        .manifest
        .bindings
        .graph_access_profile
        .digest
        .clone();
    f.profiled.offer.binding.launch_manifest_digest =
        canonical_digest(&f.profiled.offer.manifest).unwrap();
    f.profiled.status = Status::Granted;
    f.profiled.stop = Stop::Running;
    f.profiled.execution_deadline_ms = Some(NOW + 60_000);
    f.profiled.grant = Some(ProfiledGrant {
        binding: f.profiled.offer.binding.clone(),
        grant_id: Uuid::new_v4(),
        expires_at_ms: NOW + 5000,
    });
    let attempt = insert(&f).await;
    let SessionIdentity::Remote {
        session_id,
        assignment_generation,
        ..
    } = f.profiled.offer.binding.session
    else {
        unreachable!()
    };
    let binding = json!({"task_id":f.selection.lineage.task_id,"attempt_id":attempt,"session_id":session_id,"generation":assignment_generation,"node_id":f.node.node_id,"boot_id":f.node.boot_id,"project_id":f.selection.lineage.project_id,"workspace_id":f.selection.lineage.workspace_id,"resolved_spec_digest":f.profiled.offer.binding.execution_spec_digest,"profile_digest":profile_digest,"context_digest":context_digest});
    assert!(aq_agent_api::valid("Binding", &binding));
    sqlx::query("INSERT INTO agent_launch_bindings(attempt_id,node_id,binding,profile,context) VALUES($1,$2,$3,$4,$5)").bind(attempt).bind(f.node.node_id).bind(&binding).bind(profile).bind(context).execute(&f.db.pool().await).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("graph-key");
    std::fs::write(&key, b"synthetic-service-key-000000000000000000").unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    f.state.agent_access = Some(Arc::new(
        Runtime::new(&AgentAccessConfig {
            graph_url: "http://127.0.0.1:9/".into(),
            graph_token_file: key,
            mutation_actor_id: None,
            credential_ttl_ms: 1000,
        })
        .await
        .unwrap(),
    ));
    let app = aq_server::build_worker_router(f.state.clone());
    let issue = json!({"identity":f.node,"attempt":{"attempt_id":attempt,"session_id":session_id,"generation":assignment_generation},"expected_credential_version":0});
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v2/worker/agent-session")
                .header("authorization", format!("Bearer {}", f.credential))
                .header("content-type", "application/json")
                .header("X-AQ-Agent-API-Version", "1")
                .body(Body::from(issue.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let issued: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let token = issued["token"].as_str().unwrap();
    assert_eq!(issued["context"]["binding"], binding);
    let fetch = || {
        let app = app.clone();
        let token = token.to_owned();
        async move {
            app.oneshot(
                Request::builder()
                    .uri("/v1/agent/context")
                    .header("authorization", format!("Bearer {token}"))
                    .header("X-AQ-Agent-API-Version", "1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
        }
    };
    assert_eq!(fetch().await.status(), StatusCode::OK);
    f.profiled.status = Status::Cancelling;
    f.profiled.cancellation_id = Some(Uuid::new_v4());
    sqlx::query("UPDATE execution_attempts SET state=$2 WHERE attempt_id=$1")
        .bind(attempt)
        .bind(json!(f.profiled))
        .execute(&f.db.pool().await)
        .await
        .unwrap();
    assert_eq!(fetch().await.status(), StatusCode::UNAUTHORIZED);
}
#[tokio::test]
async fn database_derives_revisions_on_noop_and_bounds_uncompressed_state_and_material_verifier() {
    let f = fixture().await;
    let attempt = insert(&f).await;
    let pool = f.db.pool().await;
    let before:(i64,i64,i64)=sqlx::query_as("SELECT record_revision,authority_revision,grant_revision FROM execution_attempts WHERE attempt_id=$1").bind(attempt).fetch_one(&pool).await.unwrap();
    for value in [0i64, 999] {
        sqlx::query("UPDATE execution_attempts SET record_revision=$2,authority_revision=$2,grant_revision=$2 WHERE attempt_id=$1").bind(attempt).bind(value).execute(&pool).await.unwrap();
        let after:(i64,i64,i64)=sqlx::query_as("SELECT record_revision,authority_revision,grant_revision FROM execution_attempts WHERE attempt_id=$1").bind(attempt).fetch_one(&pool).await.unwrap();
        assert_eq!(
            before, after,
            "exact replay cannot alter server-derived revisions"
        );
    }
    let mut huge = json!(f.profiled);
    huge["padding"] = json!("x".repeat(4 * 1024 * 1024));
    assert!(
        sqlx::query("UPDATE execution_attempts SET state=$2 WHERE attempt_id=$1")
            .bind(attempt)
            .bind(huge)
            .execute(&pool)
            .await
            .is_err()
    );
    for verifier in ["a".repeat(63), "A".repeat(64), "a".repeat(65)] {
        assert!(sqlx::query("INSERT INTO admission_material_leases(lease_id,operation_id,request_digest,node_id,boot_id,prospective_attempt_id,offer_id,credential_identity,credential_revision,purpose,lease,verifier,state,issued_at_ms,expires_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7,'synthetic-credential@1',1,'model-authentication',$8,$9,'pending',1000,2000)").bind(Uuid::new_v4()).bind(Uuid::new_v4()).bind(Digest::of(b"synthetic-request").as_str()).bind(f.node.node_id).bind(f.node.boot_id).bind(attempt).bind(f.profiled.offer.binding.offer_id).bind(json!({"qualification":"synthetic-column-bound-only"})).bind(verifier).execute(&pool).await.is_err());
    }
    let mut prepared = f.profiled;
    prepared.status = Status::Preparing;
    sqlx::query("UPDATE execution_attempts SET state=$2,record_revision=999,authority_revision=999,grant_revision=999 WHERE attempt_id=$1").bind(attempt).bind(json!(prepared)).execute(&pool).await.unwrap();
    let changed:(i64,i64,i64)=sqlx::query_as("SELECT record_revision,authority_revision,grant_revision FROM execution_attempts WHERE attempt_id=$1").bind(attempt).fetch_one(&pool).await.unwrap();
    assert_eq!(changed, (before.0 + 1, before.1 + 1, before.2));
}
