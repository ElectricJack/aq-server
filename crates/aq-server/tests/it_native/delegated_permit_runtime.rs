//! Durable delegated authority, exercised with the injected server clock.
use crate::common;
#[path = "delegated_permit_runtime/restart.rs"]
mod restart;
#[path = "delegated_permit_runtime/routes.rs"]
mod routes;
use aq_admission_sandbox::sandbox::*;
use aq_server::delegated_permits::{CredentialKey, runtime};
use axum::http::HeaderMap;
use uuid::Uuid;

async fn setup() -> (
    common::profiled::Fixture,
    DelegatedPermit,
    PhaseExecutionAuthority,
) {
    let (f, start) = common::native_started::setup(true).await;
    let authority = PhaseExecutionAuthority {
        profile: PhaseProfile::PhaseV2,
        receipt_digest: aq_admission_sandbox::Digest::of(b"retained execution receipt"),
        grant: aq_session_protocol::ProfiledGrant {
            binding: start.binding.clone(),
            grant_id: start.grant_id,
            expires_at_ms: common::profiled::NOW + 1000,
        },
        issued_at_ms: common::profiled::NOW,
        hard_deadline_ms: common::profiled::NOW + 30_000,
    };
    let id = Uuid::new_v4();
    let p = DelegatedPermit {
        schema_version: 1,
        permit_id: id,
        credential: CredentialKey::new([7; 32]).credential(id).unwrap(),
        binding: PermitBinding {
            attempt_id: start.binding.attempt_id,
            grant_generation: start.binding.assignment_generation,
            sandbox_id: Uuid::new_v4(),
            channel_id: Uuid::new_v4(),
            node: aq_admission_sandbox::transport::WorkerIdentity {
                node_id: f.node.node_id,
                boot_id: f.node.boot_id,
            },
        },
        scope: PermitScope::Execution,
        issued_at_ms: authority.issued_at_ms,
        expires_at_ms: authority.grant.expires_at_ms,
        hard_deadline_ms: authority.hard_deadline_ms,
    };
    let pool = f.db.pool().await;
    let mut tx = pool.begin().await.unwrap();
    runtime::install_execution_locked(&mut tx, Uuid::new_v4(), [42; 32], &p, &authority)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    (f, p, authority)
}
fn headers(p: &DelegatedPermit) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(
        "authorization",
        format!("Bearer {}", p.credential.expose()).parse().unwrap(),
    );
    h.insert(
        "aq-runner-channel",
        serde_json::to_string(&p.binding).unwrap().parse().unwrap(),
    );
    h
}
#[tokio::test]
async fn permit_scope_refuses_foreign_attempt_and_expired_use_after_authentication() {
    let (f, p, _) = setup().await;
    let h = headers(&p);
    let auth = runtime::authenticate_runner(
        &f.state,
        &h,
        RunnerOperation::Observe,
        Some(p.binding.attempt_id),
    )
    .await
    .unwrap();
    assert!(
        runtime::authenticate_runner(&f.state, &h, RunnerOperation::Observe, Some(Uuid::new_v4()))
            .await
            .is_err()
    );
    assert!(
        aq_server::worker_api::authenticate_context(&f.state, &h)
            .await
            .is_err()
    );
    // Batch attempt scoping is covered by the typed checks in
    // delegated_typed_attempt_checks_reject_foreign_and_mixed_bindings.
    let mut stale = p.clone();
    stale.binding.node.boot_id = Uuid::new_v4();
    assert!(
        runtime::authenticate_runner(&f.state, &headers(&stale), RunnerOperation::Observe, None)
            .await
            .is_err()
    );
    f.time
        .store(p.expires_at_ms as i64, std::sync::atomic::Ordering::SeqCst);
    let pool = f.db.pool().await;
    let mut tx = pool.begin().await.unwrap();
    assert!(
        aq_server::worker_api::lock_current_node(&mut tx, &auth)
            .await
            .is_err()
    );
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn delegated_typed_attempt_checks_reject_foreign_and_mixed_bindings() {
    let (f, p, authority) = setup().await;
    let auth = runtime::authenticate_runner(&f.state, &headers(&p), RunnerOperation::Observe, None)
        .await
        .unwrap();
    let binding = authority.grant.binding;
    runtime::require_binding(&auth, &binding).unwrap();
    runtime::require_attempts(&auth, [binding.attempt_id, binding.attempt_id]).unwrap();
    assert!(runtime::require_attempts(&auth, []).is_err());
    assert!(runtime::require_attempts(&auth, [binding.attempt_id, Uuid::new_v4()]).is_err());
    let mut foreign = binding.clone();
    foreign.attempt_id = Uuid::new_v4();
    assert!(runtime::require_binding(&auth, &foreign).is_err());
    foreign = binding.clone();
    let aq_session_protocol::SessionIdentity::Remote { attempt_id, .. } = &mut foreign.session
    else {
        panic!("remote binding")
    };
    *attempt_id = Uuid::new_v4();
    assert!(runtime::require_binding(&auth, &foreign).is_err());
    // Exercise a decoded route body too: its primary attempt matches, while the
    // nested session belongs to another sandbox. This must fail before fetch.
    let fetch = aq_admission::transport::MaterialFetch {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: aq_admission::transport::WorkerIdentity {
            node_id: p.binding.node.node_id,
            boot_id: p.binding.node.boot_id,
        },
        binding: foreign,
        lease_id: Uuid::new_v4(),
    };
    assert_eq!(
        request(
            &f.state,
            "/v1/worker/profiled/material",
            &headers(&p),
            serde_json::to_value(fetch).unwrap()
        )
        .await
        .status(),
        403
    );
}

#[allow(dead_code)]
#[path = "../common/phase_refresh_terminal.rs"]
mod phase_refresh_fixture;

async fn request(
    state: &aq_server::AppState,
    path: &str,
    headers: &HeaderMap,
    body: serde_json::Value,
) -> axum::response::Response {
    use tower::ServiceExt;
    let mut request = axum::http::Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .header("aq-delivery-nonce", Uuid::new_v4().to_string());
    for (name, value) in headers {
        request = request.header(name, value);
    }
    let app = aq_server::build_worker_router(state.clone());
    let request = request
        .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    // Poll the HTTP service as its own task, as the listener does, instead of
    // nesting its large phase-owner future in the lifecycle fixture's stack.
    tokio::spawn(async move { app.oneshot(request).await.unwrap() })
        .await
        .unwrap()
}

#[tokio::test]
async fn accept_issues_and_grant_atomically_upgrades_same_permit() {
    Box::pin(exercise_phase(0)).await;
}
#[tokio::test]
async fn node_reboot_requires_readoption_before_renewal_extends() {
    Box::pin(exercise_phase(1)).await;
}
#[tokio::test]
async fn reboot_after_native_stop_readopts_only_result_and_exact_stopped_replay() {
    Box::pin(exercise_phase(7)).await;
}
#[tokio::test]
async fn server_restart_mid_attempt_resumes_or_preserves() {
    Box::pin(exercise_phase(9)).await;
}
#[tokio::test]
async fn readoption_refuses_backend_template_capability_changes_and_expired_grants() {
    for mode in 2..=6 {
        Box::pin(exercise_phase(mode)).await;
    }
}
#[tokio::test]
async fn owner_switch_on_renews_past_the_observation_age_without_a_row() {
    Box::pin(exercise_phase(10)).await;
}
#[tokio::test]
async fn expired_permit_is_refused_by_every_runner_route() {
    Box::pin(exercise_phase(8)).await;
}
async fn exercise_phase(mode: u8) {
    use aq_admission::preparation_refresh_v1::Document;
    let mut f = Box::pin(phase_refresh_fixture::prepared_terminal()).await;
    let mut config = (*f.c.env.env.state.config).clone();
    config.delegated_permits = Some(std::sync::Arc::new(CredentialKey::new([7; 32])));
    f.c.env.env.state.config = std::sync::Arc::new(config);
    let state = &f.c.env.env.state;
    let native: serde_json::Value =
        sqlx::query_scalar("SELECT state#>'{native,request}' FROM execution_nodes LIMIT 1")
            .fetch_one(&f.c.env.env.pool)
            .await
            .unwrap();
    let registration = Registration {
        schema_version: 2,
        registration: serde_json::from_value(native).unwrap(),
        backend: BackendAdvertisement {
            kind: BackendKind::Boat,
            capabilities: vec![
                BackendCapability::VmIsolation,
                BackendCapability::PersistentDiskDuringAttempt,
            ],
            max_concurrent_sandboxes: 2,
            spend_ceiling: None,
            templates: vec![TemplateAdvertisement {
                digest: aq_admission_sandbox::Digest::of(b"pinned template"),
                harnesses: vec![HarnessVersion {
                    harness: "test".into(),
                    version: "1.0.0".into(),
                }],
                estimated_cold_start_ms: 1000,
                estimated_cold_start_cost: Money {
                    currency: "USD".into(),
                    micros: 100,
                },
            }],
        },
    };
    let mut node_headers = HeaderMap::new();
    node_headers.insert(
        "authorization",
        format!("Bearer {}", f.c.env.env.credential)
            .parse()
            .unwrap(),
    );
    let registration_body =
        serde_json::to_value(SandboxDocument::Register(registration.clone())).unwrap();
    if mode != 6 {
        let registered = request(
            state,
            "/v2/worker/register",
            &node_headers,
            registration_body,
        )
        .await;
        assert_eq!(registered.status(), 200, "registration v2");
    }
    let q = PermitIssuanceRequest {
        schema_version: 1,
        operation_id: f.q.original.operation_id,
        profile: PhaseProfile::PhaseRefreshV1,
        accept_request_digest: serde_json::from_value(
            serde_json::to_value(f.q.digest().unwrap()).unwrap(),
        )
        .unwrap(),
        attempt_id: f.q.original.acceptance.binding.attempt_id,
        grant_generation: f.q.original.acceptance.binding.assignment_generation,
        sandbox_id: Uuid::new_v4(),
        node: serde_json::from_value(serde_json::to_value(&f.q.original.identity).unwrap())
            .unwrap(),
    };
    let mut h = HeaderMap::new();
    h.insert(
        "authorization",
        format!("Bearer {}", f.c.env.env.credential)
            .parse()
            .unwrap(),
    );
    h.insert(
        "aq-permit-issuance",
        serde_json::to_string(&q).unwrap().parse().unwrap(),
    );
    let body = serde_json::to_value(Document::Accept(Box::new(f.q.clone()))).unwrap();
    let response = request(state, "/v1/worker/profiled-v2/accept", &h, body.clone()).await;
    assert_eq!(response.status(), 200);
    let issued: PermitIssuanceResponse =
        serde_json::from_slice(response.headers()["aq-permit-issuance"].as_bytes()).unwrap();
    assert_eq!(response.headers()["aq-permit-revision"], "1");
    issued
        .validate_for(&q, (state.clock)().timestamp_millis() as u64)
        .unwrap();
    let replay = request(state, "/v1/worker/profiled-v2/accept", &h, body.clone()).await;
    assert_eq!(replay.status(), 200);
    assert!(response.headers()["aq-permit-issuance"] == replay.headers()["aq-permit-issuance"]);
    let (bound, total): (i64, i64) = sqlx::query_as("SELECT count(*) FILTER (WHERE m.permit_id=$1), count(*) FROM admission_material_leases m JOIN profiled_launch_materials l ON l.lease_id=m.lease_id WHERE l.attempt_id=$2")
        .bind(issued.permit.permit_id)
        .bind(q.attempt_id)
        .fetch_one(&f.c.env.env.pool)
        .await
        .unwrap();
    assert!(
        total > 0 && bound == total,
        "issuance binds the attempt's private material to its permit"
    );
    let mut changed = q.clone();
    changed.sandbox_id = Uuid::new_v4();
    h.insert(
        "aq-permit-issuance",
        serde_json::to_string(&changed).unwrap().parse().unwrap(),
    );
    assert_eq!(
        request(state, "/v1/worker/profiled-v2/accept", &h, body)
            .await
            .status(),
        409
    );
    let h = headers(&issued.permit);
    assert!(
        runtime::authenticate_runner(state, &h, RunnerOperation::Started, None)
            .await
            .is_err()
    );
    assert!(
        runtime::authenticate_runner(state, &h, RunnerOperation::Prepared, None)
            .await
            .is_ok()
    );
    let upgrade = PermitUpgradeRequest {
        schema_version: 1,
        operation_id: f.prepared.original.operation_id,
        profile: q.profile,
        prepare_request_digest: serde_json::from_value(
            serde_json::to_value(f.prepared.digest().unwrap()).unwrap(),
        )
        .unwrap(),
        accepted_receipt_digest: issued.authority.accepted_receipt_digest.clone(),
        permit_id: issued.permit.permit_id,
        binding: issued.permit.binding.clone(),
    };
    let mut h = h;
    h.insert(
        "aq-permit-upgrade",
        serde_json::to_string(&upgrade).unwrap().parse().unwrap(),
    );
    let body = serde_json::to_value(Document::Prepare(Box::new(f.prepared.clone()))).unwrap();
    let response = request(state, "/v1/worker/profiled-v2/prepare", &h, body.clone()).await;
    let status = response.status();
    if status != 200 {
        let bytes = axum::body::to_bytes(response.into_body(), 16384)
            .await
            .unwrap();
        panic!("prepare: {status} {}", String::from_utf8_lossy(&bytes));
    }
    let upgraded: PermitUpgradeResponse =
        serde_json::from_slice(response.headers()["aq-permit-upgrade"].as_bytes()).unwrap();
    let installed_revision: u64 = response.headers()["aq-permit-revision"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(installed_revision, 2);
    upgraded
        .validate_for(
            &upgrade,
            &issued.authority,
            &issued.permit,
            (state.clock)().timestamp_millis() as u64,
        )
        .unwrap();
    assert_eq!(upgraded.permit.permit_id, issued.permit.permit_id);
    assert!(upgraded.permit.credential.expose() == issued.permit.credential.expose());
    let replay = request(state, "/v1/worker/profiled-v2/prepare", &h, body).await;
    assert_eq!(replay.status(), 200);
    assert!(response.headers()["aq-permit-upgrade"] == replay.headers()["aq-permit-upgrade"]);
    assert!(
        runtime::authenticate_runner(
            state,
            &headers(&upgraded.permit),
            RunnerOperation::Started,
            None
        )
        .await
        .is_ok()
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM delegated_permits")
        .fetch_one(&f.c.env.env.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    let stored: String = sqlx::query_scalar("SELECT state::text FROM delegated_permit_current")
        .fetch_one(&f.c.env.env.pool)
        .await
        .unwrap();
    assert!(!stored.contains(upgraded.permit.credential.expose()));
    let bytes = axum::body::to_bytes(response.into_body(), 131072)
        .await
        .unwrap();
    let Document::ExecutionDelivery(delivery) =
        aq_admission::preparation_refresh_v1::decode(&bytes).unwrap()
    else {
        panic!("wrong execution response")
    };
    let started = phase_refresh_fixture::started_request(&f, &delivery.receipt);
    let mut started_headers = headers(&upgraded.permit);
    started_headers.insert("aq-phase-terminal-api-version", "1".parse().unwrap());
    let response = request(
        state,
        "/v1/worker/profiled-v2/started",
        &started_headers,
        serde_json::to_value(&started).unwrap(),
    )
    .await;
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 16384)
        .await
        .unwrap();
    assert_eq!(status, 200, "started: {}", String::from_utf8_lossy(&bytes));
    if mode == 8 {
        routes::exercise(&f, &upgraded, &started, installed_revision).await;
    } else if mode == 10 {
        owner_switch_renewal(&f, &upgraded, &started, installed_revision).await;
    } else if mode != 0 {
        exercise_reboot(
            &f,
            &upgraded,
            &started,
            installed_revision,
            &registration,
            &node_headers,
            mode,
        )
        .await;
    } else {
        exercise_same_boot_renewal(&f, &upgraded, &started, installed_revision).await;
    }
}

/// Fix round 2: with no observation row and the renewal past the policy's
/// observation age (5 s here), a renewal is refused while the switch is unset
/// and succeeds once the owner's switch is on (its synthesized attestation
/// reaches the phase health check).
async fn owner_switch_renewal(
    f: &phase_refresh_fixture::Prepared,
    upgraded: &PermitUpgradeResponse,
    started: &aq_phase_terminal_protocol::refresh_v1::StartedRequest,
    installed_revision: u64,
) {
    use std::sync::atomic::Ordering;
    let (state, pool) = (&f.c.env.env.state, &f.c.env.env.pool);
    sqlx::query("DELETE FROM intelligence_observations")
        .execute(pool)
        .await
        .unwrap();
    f.c.controlled_clock
        .as_ref()
        .unwrap()
        .fetch_add(6_000, Ordering::SeqCst);
    let q = RunnerRenewalRequest {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        authority: upgraded.authority.clone(),
        permit: upgraded.permit.clone(),
        revision: installed_revision,
    };
    let body = serde_json::to_value(SandboxDocument::RunnerRenewalRequest(q.clone())).unwrap();
    let unset = request(
        state,
        "/v1/worker/profiled/renew",
        &headers(&q.permit),
        body,
    )
    .await;
    assert_ne!(
        unset.status(),
        200,
        "no attestation, no switch: stale health"
    );
    let owner: Uuid = sqlx::query_scalar("SELECT id FROM users WHERE email='owner@localhost'")
        .fetch_one(pool)
        .await
        .unwrap();
    let now = (state.clock)();
    let mut c = pool.acquire().await.unwrap();
    aq_machine_control::set_owner_availability(&mut c, true, 1, owner, Uuid::new_v4(), now)
        .await
        .unwrap();
    drop(c);
    exercise_same_boot_renewal(f, upgraded, started, installed_revision).await;
}

async fn exercise_same_boot_renewal(
    f: &phase_refresh_fixture::Prepared,
    upgraded: &PermitUpgradeResponse,
    started: &aq_phase_terminal_protocol::refresh_v1::StartedRequest,
    installed_revision: u64,
) {
    use std::sync::atomic::Ordering;
    let state = &f.c.env.env.state;
    let clock = f.c.controlled_clock.as_ref().unwrap();
    clock.fetch_add(1, Ordering::SeqCst);
    let q = RunnerRenewalRequest {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        authority: upgraded.authority.clone(),
        permit: upgraded.permit.clone(),
        revision: installed_revision,
    };
    let body = serde_json::to_value(SandboxDocument::RunnerRenewalRequest(q.clone())).unwrap();
    let h = headers(&q.permit);
    let response = request(state, "/v1/worker/profiled/renew", &h, body.clone()).await;
    let context: PermitContext =
        serde_json::from_str(response.headers()["aq-permit-context"].to_str().unwrap()).unwrap();
    let server_now: u64 = response.headers()["aq-server-now-ms"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(context.grant_expires_at_ms, q.permit.expires_at_ms);
    assert_eq!(context.binding, q.permit.binding);
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 16384)
        .await
        .unwrap();
    assert_eq!(status, 200, "renew: {}", String::from_utf8_lossy(&bytes));
    let SandboxDocument::RunnerRenewalResponse(reply) = serde_json::from_slice(&bytes).unwrap()
    else {
        panic!("wrong response kind")
    };
    reply
        .validate_for(
            &q,
            &PermitContext {
                binding: q.permit.binding.clone(),
                phase: GrantPhase::Granted,
                grant_expires_at_ms: q.permit.expires_at_ms,
                hard_deadline_ms: q.permit.hard_deadline_ms,
            },
            reply.delivery.as_ref().unwrap().nonce,
            clock.load(Ordering::SeqCst) as u64,
        )
        .unwrap();
    let RenewalReply::Renewed {
        permit: renewed, ..
    } = &reply.reply
    else {
        panic!("not renewed")
    };
    reply
        .validate_for(
            &q,
            &context,
            reply.delivery.as_ref().unwrap().nonce,
            server_now,
        )
        .unwrap();
    capture(
        "renewal",
        serde_json::json!({"request": body, "response": serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(), "context":context,"server_now_ms":server_now}),
    );
    assert_eq!(reply.revision, 3);
    assert!(renewed.expires_at_ms > q.permit.expires_at_ms);
    assert_eq!(renewed.hard_deadline_ms, q.permit.hard_deadline_ms);
    assert!(renewed.credential.expose() == q.permit.credential.expose());
    let mut reconstructed = state.clone();
    reconstructed.store = f.c.env.env._db.store().await;
    let replay = request(
        &reconstructed,
        "/v1/worker/profiled/renew",
        &h,
        body.clone(),
    )
    .await;
    assert_eq!(replay.status(), 200);
    let bytes = axum::body::to_bytes(replay.into_body(), 16384)
        .await
        .unwrap();
    let SandboxDocument::RunnerRenewalResponse(replay) = serde_json::from_slice(&bytes).unwrap()
    else {
        panic!("wrong replay kind")
    };
    assert_eq!(replay.revision, reply.revision);
    assert!(
        serde_json::to_value(&replay.reply).unwrap() == serde_json::to_value(&reply.reply).unwrap()
    );
    let mut stale = q.clone();
    stale.operation_id = Uuid::new_v4();
    assert_eq!(
        request(
            state,
            "/v1/worker/profiled/renew",
            &h,
            serde_json::to_value(SandboxDocument::RunnerRenewalRequest(stale)).unwrap()
        )
        .await
        .status(),
        409
    );
    let mut changed = q.clone();
    changed.revision += 1;
    assert_eq!(
        request(
            state,
            "/v1/worker/profiled/renew",
            &h,
            serde_json::to_value(SandboxDocument::RunnerRenewalRequest(changed)).unwrap()
        )
        .await
        .status(),
        409
    );
    let snapshot: serde_json::Value = sqlx::query_scalar("SELECT jsonb_build_object('hot',(SELECT state FROM profiled_authority_hot LIMIT 1),'permit',(SELECT state FROM delegated_permit_current LIMIT 1),'ops',(SELECT count(*) FROM delegated_operations))")
        .fetch_one(&f.c.env.env.pool).await.unwrap();
    assert_eq!(
        snapshot["hot"]["grant"]["expires_at_ms"],
        renewed.expires_at_ms
    );
    let retained: String =
        sqlx::query_scalar("SELECT evidence::text FROM delegated_operations WHERE kind='renewal'")
            .fetch_one(&f.c.env.env.pool)
            .await
            .unwrap();
    assert!(!retained.contains(renewed.credential.expose()));
    let stopped = phase_refresh_fixture::stop_request(started);
    let mut stop_headers = headers(renewed);
    stop_headers.insert("aq-phase-terminal-api-version", "1".parse().unwrap());
    let response = request(
        state,
        "/v1/worker/profiled-v2/stopped",
        &stop_headers,
        serde_json::to_value(&stopped).unwrap(),
    )
    .await;
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 16384)
        .await
        .unwrap();
    assert_eq!(
        status,
        200,
        "stopped after renewal: {}",
        String::from_utf8_lossy(&bytes)
    );
    assert_eq!(
        request(state, "/v1/worker/profiled/renew", &h, body)
            .await
            .status(),
        403
    );
    let after: serde_json::Value = sqlx::query_scalar("SELECT jsonb_build_object('hot',(SELECT state FROM profiled_authority_hot LIMIT 1),'permit',(SELECT state FROM delegated_permit_current LIMIT 1),'ops',(SELECT count(*) FROM delegated_operations))")
        .fetch_one(&f.c.env.env.pool).await.unwrap();
    assert_eq!(after["permit"], snapshot["permit"]);
    assert_eq!(after["ops"], snapshot["ops"]);
    assert_eq!(after["hot"]["status"], "finished");
    let retry = request(
        state,
        "/v1/worker/profiled-v2/stopped",
        &stop_headers,
        serde_json::to_value(&stopped).unwrap(),
    )
    .await;
    assert_eq!(retry.status(), 200);
    assert_eq!(
        axum::body::to_bytes(retry.into_body(), 16384)
            .await
            .unwrap(),
        bytes
    );
    let mut conflict = stopped.clone();
    conflict.original.evidence.stop_operation_id = Uuid::new_v4();
    assert_ne!(
        request(
            state,
            "/v1/worker/profiled-v2/stopped",
            &stop_headers,
            serde_json::to_value(conflict).unwrap()
        )
        .await
        .status(),
        200
    );
    let report = aq_admission::transport::ResultReport {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        report_id: Uuid::new_v4(),
        identity: aq_admission::transport::WorkerIdentity {
            node_id: renewed.binding.node.node_id,
            boot_id: renewed.binding.node.boot_id,
        },
        binding: started.original.binding.clone(),
        supersedes: None,
        outcome: aq_admission::transport::ProcessOutcome::Success,
        stop: stopped.original.evidence.clone(),
        source_receipt_ids: vec![],
        pending_effect_ids: vec![],
        conclusion: aq_admission::Label::new(
            "Synthetic delegated receipt fixture; no task settlement",
        )
        .unwrap(),
    };
    let result = request(
        state,
        "/v1/worker/profiled/results",
        &stop_headers,
        serde_json::to_value(&report).unwrap(),
    )
    .await;
    assert_eq!(result.status(), 200);
    let envelope: aq_server::delegated_permits::receipts::Envelope =
        serde_json::from_str(result.headers()["aq-runner-receipts"].to_str().unwrap()).unwrap();
    let result_bytes = axum::body::to_bytes(result.into_body(), 16384)
        .await
        .unwrap();
    let receipt: aq_admission::result_record::Receipt =
        serde_json::from_slice(&result_bytes).unwrap();
    receipt.validate_for(&report).unwrap();
    let central = envelope.result.as_ref().unwrap();
    assert_eq!(central.artifact_digest, receipt.report_digest);
    assert_eq!(central.receipt_id, report.report_id);
    assert_eq!(central.attempt.launch_id, report.stop.launch_id);
    assert_eq!(
        central.attempt.manifest_digest,
        report.binding.launch_manifest_digest
    );
    assert_eq!(envelope.finished.as_ref().unwrap().attempt, central.attempt);
    capture(
        "result-finished",
        serde_json::json!({"request":report,"response":receipt,"receipts":envelope,"stopped_request":stopped,"stopped_response":serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()}),
    );
    let mut restarted = state.clone();
    restarted.store = f.c.env.env._db.store().await;
    let result = request(
        &restarted,
        "/v1/worker/profiled/results",
        &stop_headers,
        serde_json::to_value(&report).unwrap(),
    )
    .await;
    assert_eq!(result.status(), 200);
    assert_eq!(
        axum::body::to_bytes(result.into_body(), 16384)
            .await
            .unwrap(),
        result_bytes
    );
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM profiled_result_reports WHERE report_id=$1")
            .bind(report.report_id)
            .fetch_one(&f.c.env.env.pool)
            .await
            .unwrap();
    assert_eq!(count, 1);
    clock.store(renewed.expires_at_ms as i64, Ordering::SeqCst);
    assert_eq!(
        request(
            state,
            "/v1/worker/profiled/results",
            &stop_headers,
            serde_json::to_value(&report).unwrap()
        )
        .await
        .status(),
        403
    );
}

fn capture(name: &str, value: serde_json::Value) {
    if let Ok(directory) = std::env::var("B2_CAPTURE_CONTRACT") {
        std::fs::create_dir_all(&directory).unwrap();
        let mut bytes = serde_json::to_vec_pretty(&value).unwrap();
        bytes.push(b'\n');
        std::fs::write(
            std::path::Path::new(&directory).join(format!("{name}.json")),
            bytes,
        )
        .unwrap();
    }
}

#[tokio::test]
async fn reconstructed_auth_fences_revocation_and_stale_boot() {
    let (f, p, _) = setup().await;
    let h = headers(&p);
    let pool = f.db.pool().await;
    let mut restarted = f.state.clone();
    restarted.store = f.db.store().await;
    runtime::authenticate_runner(&restarted, &h, RunnerOperation::Observe, None)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO delegated_node_boots(node_id,boot_id,registration) VALUES($1,$2,'{}')",
    )
    .bind(p.binding.node.node_id)
    .bind(Uuid::new_v4())
    .execute(&pool)
    .await
    .unwrap();
    assert!(
        runtime::authenticate_runner(&restarted, &h, RunnerOperation::Observe, None)
            .await
            .is_err()
    );
    sqlx::query("UPDATE delegated_node_boots SET boot_id=$2 WHERE node_id=$1")
        .bind(p.binding.node.node_id)
        .bind(p.binding.node.boot_id)
        .execute(&pool)
        .await
        .unwrap();
    let auth = runtime::authenticate_runner(&restarted, &h, RunnerOperation::Observe, None)
        .await
        .unwrap();
    sqlx::query("UPDATE nodes SET revoked_at=now() WHERE id=$1")
        .bind(p.binding.node.node_id)
        .execute(&pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    assert!(
        aq_server::worker_api::lock_current_node(&mut tx, &auth)
            .await
            .is_err()
    );
    tx.rollback().await.unwrap();
    assert!(
        runtime::authenticate_runner(&restarted, &h, RunnerOperation::Observe, None)
            .await
            .is_err()
    );
}

async fn document(response: axum::response::Response) -> SandboxDocument {
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 131072)
        .await
        .unwrap();
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
    serde_json::from_slice(&bytes).unwrap()
}
async fn exercise_reboot(
    f: &phase_refresh_fixture::Prepared,
    upgraded: &PermitUpgradeResponse,
    started: &aq_phase_terminal_protocol::refresh_v1::StartedRequest,
    revision: u64,
    original_registration: &Registration,
    node_headers: &HeaderMap,
    mode: u8,
) {
    use std::sync::atomic::Ordering;
    let state = &f.c.env.env.state;
    let pool = &f.c.env.env.pool;
    let clock = f.c.controlled_clock.as_ref().unwrap();
    let snapshot_sql = "SELECT jsonb_build_object('node',(SELECT state FROM execution_nodes LIMIT 1),'grants',(SELECT jsonb_agg(to_jsonb(g)) FROM profiled_phase_execution_grants g),'runtime',(SELECT jsonb_agg(to_jsonb(r)) FROM native_registration_receipts r))";
    let history: serde_json::Value = sqlx::query_scalar(snapshot_sql)
        .fetch_one(pool)
        .await
        .unwrap();
    let old_auth = runtime::authenticate_runner(
        state,
        &headers(&upgraded.permit),
        RunnerOperation::Observe,
        None,
    )
    .await
    .unwrap();
    // Mode 7 reproduces the lost-response window: stop commits on the old
    // incarnation, then the node reboots before the sandbox receives its receipt.
    let ended = if mode == 7 {
        let stopped = phase_refresh_fixture::stop_request(started);
        let mut h = headers(&upgraded.permit);
        h.insert("aq-phase-terminal-api-version", "1".parse().unwrap());
        let response = request(
            state,
            "/v1/worker/profiled-v2/stopped",
            &h,
            serde_json::to_value(&stopped).unwrap(),
        )
        .await;
        assert_eq!(response.status(), 200);
        Some((
            stopped,
            axum::body::to_bytes(response.into_body(), 16384)
                .await
                .unwrap(),
        ))
    } else {
        None
    };
    let mut registration = original_registration.clone();
    registration.registration.operation_id = Uuid::new_v4();
    registration.registration.identity.boot_id = Uuid::new_v4();
    match mode {
        2 => registration.backend.kind = BackendKind::CloudflareContainers,
        3 => {
            registration.backend.templates[0].digest =
                aq_admission_sandbox::Digest::of(b"different template")
        }
        4 => registration.backend.capabilities.clear(),
        _ => (),
    }
    let body = serde_json::to_value(SandboxDocument::Register(registration.clone())).unwrap();
    let SandboxDocument::RegistrationResponse(reply) =
        document(request(state, "/v2/worker/register", node_headers, body.clone()).await).await
    else {
        panic!("registration response");
    };
    reply.validate_for(&registration).unwrap();
    assert_eq!(
        request(
            state,
            "/v2/worker/register",
            &headers(&upgraded.permit),
            body.clone()
        )
        .await
        .status(),
        401
    );
    assert_eq!(
        request(state, "/v2/worker/register", node_headers, body)
            .await
            .status(),
        200
    );
    let mut q = RunnerRenewalRequest {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        authority: upgraded.authority.clone(),
        permit: upgraded.permit.clone(),
        revision,
    };
    let before: serde_json::Value =
        sqlx::query_scalar("SELECT state FROM profiled_authority_hot LIMIT 1")
            .fetch_one(pool)
            .await
            .unwrap();
    clock.fetch_add(1, Ordering::SeqCst);
    let renewal = request(
        state,
        "/v1/worker/profiled/renew",
        &headers(&q.permit),
        serde_json::to_value(SandboxDocument::RunnerRenewalRequest(q.clone())).unwrap(),
    )
    .await;
    let awaiting = if mode == 7 {
        assert_eq!(
            renewal.status(),
            403,
            "ended grants cannot renew even before adoption"
        );
        None
    } else {
        let SandboxDocument::RunnerRenewalResponse(awaiting) = document(renewal).await else {
            panic!("renewal response")
        };
        assert!(matches!(
            awaiting.reply,
            RenewalReply::AwaitingNodeReadoption { .. }
        ));
        assert_eq!(awaiting.revision, revision);
        assert!(awaiting.delivery.is_none());
        Some(awaiting)
    };
    let after: serde_json::Value =
        sqlx::query_scalar("SELECT state FROM profiled_authority_hot LIMIT 1")
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(before, after);
    let readopt = ReadoptionRequest {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        previous: upgraded.permit.binding.node.clone(),
        current: registration.registration.identity.clone(),
        sandboxes: vec![LiveSandbox {
            permit_id: upgraded.permit.permit_id,
            attempt_id: upgraded.permit.binding.attempt_id,
            grant_generation: upgraded.permit.binding.grant_generation,
            sandbox_id: upgraded.permit.binding.sandbox_id,
        }],
    };
    let body = serde_json::to_value(SandboxDocument::ReadoptionRequest(readopt.clone())).unwrap();
    assert_eq!(
        request(
            state,
            "/v2/worker/readopt",
            &headers(&q.permit),
            body.clone()
        )
        .await
        .status(),
        401
    );
    if mode == 5 {
        clock.store(upgraded.permit.expires_at_ms as i64, Ordering::SeqCst);
    }
    let SandboxDocument::ReadoptionResponse(adopted) =
        document(request(state, "/v2/worker/readopt", node_headers, body.clone()).await).await
    else {
        panic!("readoption response");
    };
    adopted
        .validate_for(
            &readopt,
            std::slice::from_ref(&upgraded.permit),
            clock.load(Ordering::SeqCst) as u64,
        )
        .unwrap();
    if mode == 1 {
        capture(
            "readoption",
            serde_json::json!({"request":readopt,"response":adopted,"awaiting":awaiting}),
        );
    }
    if (2..=6).contains(&mode) {
        assert!(
            matches!(adopted.results[0], ReadoptionResult::Refused { .. }),
            "mode {mode}"
        );
        assert!(
            runtime::authenticate_runner(
                state,
                &headers(&upgraded.permit),
                RunnerOperation::Observe,
                None
            )
            .await
            .is_err()
        );
        let after: serde_json::Value =
            sqlx::query_scalar("SELECT state FROM profiled_authority_hot LIMIT 1")
                .fetch_one(pool)
                .await
                .unwrap();
        assert_eq!(before, after);
        let unchanged: serde_json::Value = sqlx::query_scalar(snapshot_sql)
            .fetch_one(pool)
            .await
            .unwrap();
        assert_eq!(history, unchanged);
        let mut stale = original_registration.clone();
        stale.registration.operation_id = Uuid::new_v4();
        assert_eq!(
            request(
                state,
                "/v2/worker/register",
                node_headers,
                serde_json::to_value(SandboxDocument::Register(stale)).unwrap()
            )
            .await
            .status(),
            403
        );
        // Refusal leaves ordinary node-authenticated stop/preserve available.
        let mut stop_headers = node_headers.clone();
        stop_headers.insert("aq-phase-terminal-api-version", "1".parse().unwrap());
        assert_eq!(
            request(
                state,
                "/v1/worker/profiled-v2/stopped",
                &stop_headers,
                serde_json::to_value(phase_refresh_fixture::stop_request(started)).unwrap()
            )
            .await
            .status(),
            200
        );
        return;
    }
    let ReadoptionResult::Rebound { permit } = &adopted.results[0] else {
        panic!("not rebound: {:?}", adopted.results);
    };
    assert!(permit.credential.expose() != upgraded.permit.credential.expose());
    assert_eq!(permit.expires_at_ms, upgraded.permit.expires_at_ms);
    let replay = document(request(state, "/v2/worker/readopt", node_headers, body).await).await;
    let SandboxDocument::ReadoptionResponse(replay) = replay else {
        panic!("wrong replay");
    };
    assert!(replay == adopted);
    let mut conflict = readopt.clone();
    conflict.sandboxes.clear();
    assert_eq!(
        request(
            state,
            "/v2/worker/readopt",
            node_headers,
            serde_json::to_value(SandboxDocument::ReadoptionRequest(conflict)).unwrap()
        )
        .await
        .status(),
        409
    );
    assert!(
        runtime::authenticate_runner(
            state,
            &headers(&upgraded.permit),
            RunnerOperation::Observe,
            None
        )
        .await
        .is_err()
    );
    let mut stolen_binding = headers(&upgraded.permit);
    stolen_binding.insert(
        "aq-runner-channel",
        serde_json::to_string(&permit.binding)
            .unwrap()
            .parse()
            .unwrap(),
    );
    assert!(
        runtime::authenticate_runner(state, &stolen_binding, RunnerOperation::Observe, None)
            .await
            .is_err()
    );
    let mut tx = pool.begin().await.unwrap();
    assert!(
        aq_server::worker_api::lock_current_node(&mut tx, &old_auth)
            .await
            .is_err()
    );
    tx.rollback().await.unwrap();
    // A new op cannot revive the original incarnation.
    let mut stale = original_registration.clone();
    stale.registration.operation_id = Uuid::new_v4();
    assert_eq!(
        request(
            state,
            "/v2/worker/register",
            node_headers,
            serde_json::to_value(SandboxDocument::Register(stale)).unwrap()
        )
        .await
        .status(),
        403
    );
    if let Some((stopped, receipt)) = ended {
        exercise_ended_readoption(
            f,
            upgraded,
            permit,
            &readopt,
            node_headers,
            &stopped,
            &receipt,
        )
        .await;
        return;
    }
    q.permit = permit.clone();
    q.operation_id = Uuid::new_v4();
    clock.fetch_add(1, Ordering::SeqCst);
    if mode == 9 {
        restart::exercise(f, started, &q).await;
        return;
    }
    // Reconstruct the store: re-adoption must not depend on an in-memory flag.
    let mut restarted = state.clone();
    restarted.store = f.c.env.env._db.store().await;
    let body = serde_json::to_value(SandboxDocument::RunnerRenewalRequest(q.clone())).unwrap();
    let SandboxDocument::RunnerRenewalResponse(renewed) = document(
        request(
            &restarted,
            "/v1/worker/profiled/renew",
            &headers(&q.permit),
            body.clone(),
        )
        .await,
    )
    .await
    else {
        panic!("renewal response");
    };
    renewed
        .validate_for(
            &q,
            &PermitContext {
                binding: q.permit.binding.clone(),
                phase: GrantPhase::Granted,
                grant_expires_at_ms: q.permit.expires_at_ms,
                hard_deadline_ms: q.permit.hard_deadline_ms,
            },
            renewed.delivery.as_ref().unwrap().nonce,
            clock.load(Ordering::SeqCst) as u64,
        )
        .unwrap();
    let RenewalReply::Renewed { permit: live, .. } = &renewed.reply else {
        panic!("not renewed");
    };
    assert_eq!(live.hard_deadline_ms, upgraded.permit.hard_deadline_ms);
    assert_eq!(
        live.expires_at_ms,
        (clock.load(Ordering::SeqCst) as u64 + 120_000).min(live.hard_deadline_ms)
    );
    assert_eq!(renewed.revision, revision + 1);
    let SandboxDocument::RunnerRenewalResponse(replay) = document(
        request(
            &restarted,
            "/v1/worker/profiled/renew",
            &headers(&q.permit),
            body,
        )
        .await,
    )
    .await
    else {
        panic!("renewal replay");
    };
    assert_eq!(replay.revision, renewed.revision);
    assert!(
        serde_json::to_value(replay.reply).unwrap()
            == serde_json::to_value(&renewed.reply).unwrap()
    );
    let unchanged: serde_json::Value = sqlx::query_scalar(snapshot_sql)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(history, unchanged);
    let stored: String =
        sqlx::query_scalar("SELECT string_agg(evidence::text,'') FROM delegated_operations")
            .fetch_one(pool)
            .await
            .unwrap();
    assert!(!stored.contains(live.credential.expose()));
    assert!(!stored.contains(upgraded.permit.credential.expose()));
    let mut stop_headers = headers(live);
    stop_headers.insert("aq-phase-terminal-api-version", "1".parse().unwrap());
    let stop_request = phase_refresh_fixture::stop_request(started);
    let stopped = request(
        &restarted,
        "/v1/worker/profiled-v2/stopped",
        &stop_headers,
        serde_json::to_value(&stop_request).unwrap(),
    )
    .await;
    let status = stopped.status();
    let bytes = axum::body::to_bytes(stopped.into_body(), 16384)
        .await
        .unwrap();
    assert_eq!(
        status,
        200,
        "stop after reboot: {}",
        String::from_utf8_lossy(&bytes)
    );
    // Authenticate the adopted credential, then replay immutable original-boot
    // history from retained server state. Reconstruct Store before the retry.
    let replay_state_sql = "SELECT jsonb_build_object('stops',(SELECT jsonb_agg(to_jsonb(s)) FROM profiled_stopped_receipts s),'hot',(SELECT state FROM profiled_authority_hot LIMIT 1),'permit',(SELECT state FROM delegated_permit_current LIMIT 1),'ops',(SELECT count(*) FROM delegated_operations))";
    let committed: serde_json::Value = sqlx::query_scalar(replay_state_sql)
        .fetch_one(pool)
        .await
        .unwrap();
    restarted.store = f.c.env.env._db.store().await;
    let retry = request(
        &restarted,
        "/v1/worker/profiled-v2/stopped",
        &stop_headers,
        serde_json::to_value(&stop_request).unwrap(),
    )
    .await;
    assert_eq!(retry.status(), 200);
    let replay_bytes = axum::body::to_bytes(retry.into_body(), 16384)
        .await
        .unwrap();
    assert_eq!(replay_bytes, bytes);
    let native: aq_admission::stopped::Receipt = serde_json::from_value(
        serde_json::from_slice::<serde_json::Value>(&replay_bytes).unwrap()["original"].clone(),
    )
    .unwrap();
    native.validate_for(&stop_request.original).unwrap();
    capture(
        "readoption-stopped-replay-accepted",
        serde_json::json!({
            "request":stop_request,"original_response":serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
            "permit":live,"context":{"binding":live.binding,"phase":"ended","grant_expires_at_ms":live.expires_at_ms,"hard_deadline_ms":live.hard_deadline_ms},
            "server_now_ms":clock.load(Ordering::SeqCst),"http_status":200,
            "replay_response":serde_json::from_slice::<serde_json::Value>(&replay_bytes).unwrap(),
        }),
    );
    // Neither the previous credential nor a forged channel boot can use this
    // exception, even when they supply exact historical bytes.
    let mut old_headers = headers(&upgraded.permit);
    old_headers.insert("aq-phase-terminal-api-version", "1".parse().unwrap());
    let mut forged = live.clone();
    forged.binding.node.boot_id = Uuid::new_v4();
    let mut forged_headers = headers(&forged);
    forged_headers.insert("aq-phase-terminal-api-version", "1".parse().unwrap());
    for rejected in [&old_headers, &forged_headers] {
        assert_eq!(
            request(
                &restarted,
                "/v1/worker/profiled-v2/stopped",
                rejected,
                serde_json::to_value(&stop_request).unwrap()
            )
            .await
            .status(),
            403
        );
    }
    for boot in [live.binding.node.boot_id, Uuid::new_v4()] {
        let mut changed = stop_request.clone();
        let aq_admission::SessionIdentity::Remote { boot_id, .. } =
            &mut changed.original.evidence.binding.session
        else {
            panic!()
        };
        *boot_id = boot;
        changed.original.proof.observation.guardian.intent.session =
            changed.original.evidence.binding.session.clone();
        let aq_admission::transport::StopDisposition::Stopped {
            stop_receipt_digest,
            ..
        } = &mut changed.original.evidence.disposition
        else {
            panic!()
        };
        *stop_receipt_digest = changed.original.proof.digest().unwrap();
        changed.validate().unwrap();
        assert_eq!(
            request(
                &restarted,
                "/v1/worker/profiled-v2/stopped",
                &stop_headers,
                serde_json::to_value(changed).unwrap()
            )
            .await
            .status(),
            409
        );
    }
    let mut changed = stop_request.clone();
    changed.original.evidence.stop_operation_id = Uuid::new_v4();
    assert_eq!(
        request(
            &restarted,
            "/v1/worker/profiled-v2/stopped",
            &stop_headers,
            serde_json::to_value(changed).unwrap()
        )
        .await
        .status(),
        409
    );
    q.permit = live.clone();
    q.revision = renewed.revision;
    q.operation_id = Uuid::new_v4();
    assert_eq!(
        request(
            &restarted,
            "/v1/worker/profiled/renew",
            &headers(&q.permit),
            serde_json::to_value(SandboxDocument::RunnerRenewalRequest(q)).unwrap()
        )
        .await
        .status(),
        403
    );
    clock.store(live.expires_at_ms as i64, Ordering::SeqCst);
    assert_eq!(
        request(
            &restarted,
            "/v1/worker/profiled-v2/stopped",
            &stop_headers,
            serde_json::to_value(&stop_request).unwrap()
        )
        .await
        .status(),
        403
    );
    let after: serde_json::Value = sqlx::query_scalar(replay_state_sql)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(
        after, committed,
        "retries and refusals must preserve native history and authority"
    );
    assert_eq!(after["stops"].as_array().unwrap().len(), 1);
}

async fn exercise_ended_readoption(
    f: &phase_refresh_fixture::Prepared,
    upgraded: &PermitUpgradeResponse,
    permit: &DelegatedPermit,
    readopt: &ReadoptionRequest,
    node_headers: &HeaderMap,
    stopped: &aq_phase_terminal_protocol::refresh_v1::StoppedRequest,
    receipt: &[u8],
) {
    use std::sync::atomic::Ordering;
    let mut restarted = f.c.env.env.state.clone();
    restarted.store = f.c.env.env._db.store().await;
    let pool = &f.c.env.env.pool;
    let snapshot_sql = "SELECT jsonb_build_object('stops',(SELECT jsonb_agg(to_jsonb(s)) FROM profiled_stopped_receipts s),'hot',(SELECT state FROM profiled_authority_hot LIMIT 1),'permit',(SELECT state FROM delegated_permit_current LIMIT 1),'ops',(SELECT count(*) FROM delegated_operations))";
    let before: serde_json::Value = sqlx::query_scalar(snapshot_sql)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(permit.expires_at_ms, upgraded.permit.expires_at_ms);
    assert_eq!(permit.hard_deadline_ms, upgraded.permit.hard_deadline_ms);
    // Replay readoption after reconstructing Store, while authority is Ended.
    document(
        request(
            &restarted,
            "/v2/worker/readopt",
            node_headers,
            serde_json::to_value(SandboxDocument::ReadoptionRequest(readopt.clone())).unwrap(),
        )
        .await,
    )
    .await;
    for operation in [
        RunnerOperation::Renew,
        RunnerOperation::Started,
        RunnerOperation::Observe,
        RunnerOperation::Terminal,
        RunnerOperation::FetchExecutionMaterial,
        RunnerOperation::UploadArtifact,
        RunnerOperation::MailboxPoll,
        RunnerOperation::MailboxAck,
    ] {
        assert!(
            runtime::authenticate_runner(&restarted, &headers(permit), operation, None)
                .await
                .is_err(),
            "ended operation {operation:?}"
        );
    }
    runtime::authenticate_runner(
        &restarted,
        &headers(permit),
        RunnerOperation::ReportResult,
        None,
    )
    .await
    .unwrap();
    let mut h = headers(permit);
    h.insert("aq-phase-terminal-api-version", "1".parse().unwrap());
    let replay = request(
        &restarted,
        "/v1/worker/profiled-v2/stopped",
        &h,
        serde_json::to_value(stopped).unwrap(),
    )
    .await;
    assert_eq!(replay.status(), 200);
    assert_eq!(
        axum::body::to_bytes(replay.into_body(), 16384)
            .await
            .unwrap()
            .as_ref(),
        receipt
    );
    let mut changed = stopped.clone();
    changed.original.evidence.stop_operation_id = Uuid::new_v4();
    assert_eq!(
        request(
            &restarted,
            "/v1/worker/profiled-v2/stopped",
            &h,
            serde_json::to_value(changed).unwrap()
        )
        .await
        .status(),
        409
    );
    let after: serde_json::Value = sqlx::query_scalar(snapshot_sql)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(
        before, after,
        "readoption replay and stopped retry extend no authority"
    );
    let report = aq_admission::transport::ResultReport {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        report_id: Uuid::new_v4(),
        identity: aq_admission::transport::WorkerIdentity {
            node_id: upgraded.permit.binding.node.node_id,
            boot_id: upgraded.permit.binding.node.boot_id,
        },
        binding: stopped.original.evidence.binding.clone(),
        supersedes: None,
        outcome: aq_admission::transport::ProcessOutcome::Success,
        stop: stopped.original.evidence.clone(),
        source_receipt_ids: vec![],
        pending_effect_ids: vec![],
        conclusion: aq_admission::Label::new("Stopped before reboot; retained original history")
            .unwrap(),
    };
    for _ in 0..2 {
        let result = request(
            &restarted,
            "/v1/worker/profiled/results",
            &h,
            serde_json::to_value(&report).unwrap(),
        )
        .await;
        let status = result.status();
        let bytes = axum::body::to_bytes(result.into_body(), 16384)
            .await
            .unwrap();
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
        let result: aq_admission::result_record::Receipt = serde_json::from_slice(&bytes).unwrap();
        result.validate_for(&report).unwrap();
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM profiled_result_reports")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    f.c.controlled_clock
        .as_ref()
        .unwrap()
        .store(permit.expires_at_ms as i64, Ordering::SeqCst);
    assert_eq!(
        request(
            &restarted,
            "/v2/worker/readopt",
            node_headers,
            serde_json::to_value(SandboxDocument::ReadoptionRequest(readopt.clone())).unwrap()
        )
        .await
        .status(),
        403
    );
    assert_eq!(
        request(
            &restarted,
            "/v1/worker/profiled/results",
            &h,
            serde_json::to_value(&report).unwrap()
        )
        .await
        .status(),
        403
    );
}
