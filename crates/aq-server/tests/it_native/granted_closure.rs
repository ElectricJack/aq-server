//! SQL/HTTP owner qualification with actual publication→Accept→initial Grant.
//! Synthetic proof policy does not qualify a physical suppression producer.
use crate::common;
use aq_admission::{
    Digest,
    granted_closure::{Evidence, JournalCheckpoint, Receipt, Request, SuppressionRecord},
};
use aq_server::profiled_execution::closure::{
    policy::{EvidenceKind, HistoricalVerifierPolicy, InstalledPolicy, Verifier},
    service,
};
use common::closure::{self, Prepared};
use http_body_util::BodyExt;
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;
async fn request(c: &Prepared) -> Request {
    let granted = closure::grant(c).await.unwrap().original;
    let row:(serde_json::Value,)=sqlx::query_as("SELECT convert_from(metadata_bytes,'UTF8')::JSONB FROM profiled_native_registration_history h JOIN profiled_launch_registrations l USING(registration_id) WHERE l.attempt_id=$1").bind(c.request.binding.attempt_id).fetch_one(&c.case.env.env.pool).await.unwrap();
    let metadata: aq_server::profiled_execution::registration::NativeRegistration =
        serde_json::from_value(row.0).unwrap();
    let operation_id = Uuid::new_v4();
    let previous = JournalCheckpoint {
        revision: c.request.journal_revision,
        digest: c.request.journal_digest.clone(),
    };
    let record = SuppressionRecord {
        schema_version: 1,
        operation_id,
        suppression_id: Uuid::new_v4(),
        binding: c.request.binding.clone(),
        grant_id: granted.grant.grant_id,
        guardian_id: granted.guardian_id,
        launch_id: granted.launch_id,
        reservation_id: granted.reservation_id,
        original_kernel_boot_id: Uuid::new_v4(),
        recording_kernel_boot_id: Uuid::new_v4(),
        previous,
        terminal_revision: c.request.journal_revision + 1,
    };
    let request = Request {
        schema_version: 1,
        operation_id,
        binding: record.binding.clone(),
        grant_id: record.grant_id,
        guardian_id: record.guardian_id,
        launch_id: record.launch_id,
        reservation_id: record.reservation_id,
        runtime: metadata.request.runtime,
        qualification: metadata.qualification,
        journal: JournalCheckpoint {
            revision: record.terminal_revision,
            digest: Digest::of(b"synthetic terminal journal; not physical evidence"),
        },
        evidence: Evidence::Suppressed {
            record_digest: record.digest().unwrap(),
            record,
        },
    };
    request.validate().unwrap();
    request
}
async fn policy(c: &Prepared, r: &Request, revision: u64) -> Arc<InstalledPolicy> {
    HistoricalVerifierPolicy::new(
        revision,
        vec![Verifier {
            runtime: r.runtime.clone(),
            qualification: r.qualification.clone(),
            profile: common::profiled::artifact(
                "synthetic-recovery-profile",
                b"test-only opaque proof policy",
            ),
            kinds: vec![EvidenceKind::Suppressed, EvidenceKind::HostStopped],
        }],
    )
    .unwrap()
    .install(&c.case.env.env.state)
    .await
    .unwrap()
}
async fn post(
    c: &Prepared,
    p: Arc<InstalledPolicy>,
    r: &Request,
) -> (axum::http::StatusCode, Vec<u8>) {
    let response =
        aq_server::build_worker_router_with_closure_policy(c.case.env.env.state.clone(), p)
            .oneshot(
                axum::http::Request::post("/v1/worker/profiled/launch-closure")
                    .header(
                        "authorization",
                        format!("Bearer {}", c.case.env.env.credential),
                    )
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(serde_json::to_vec(r).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, body)
}
#[tokio::test]
async fn actual_grant_closes_once_and_replays_without_release_or_execution_clock() {
    let c = closure::setup().await;
    let r = request(&c).await;
    let p = policy(&c, &r, 1).await;
    let (status, bytes) = post(&c, p.clone(), &r).await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
    let receipt: Receipt = serde_json::from_slice(&bytes).unwrap();
    receipt.validate_for(&r).unwrap();
    let (status, replay) = post(&c, p.clone(), &r).await;
    assert_eq!(status, 200);
    assert_eq!(bytes, replay);
    let state: serde_json::Value =
        sqlx::query_scalar("SELECT state FROM profiled_authority_hot WHERE attempt_id=$1")
            .bind(r.binding.attempt_id)
            .fetch_one(&c.case.env.env.pool)
            .await
            .unwrap();
    assert_eq!(state["status"], "cancelling");
    assert_eq!(state["revoked"], true);
    assert_eq!(state["stop"]["kind"], "unknown");
    assert_eq!(state["grant"]["grant_id"], r.grant_id.to_string());
    assert!(
        sqlx::query_scalar::<_, bool>(
            "SELECT holds_task FROM execution_attempts WHERE attempt_id=$1"
        )
        .bind(r.binding.attempt_id)
        .fetch_one(&c.case.env.env.pool)
        .await
        .unwrap()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM profiled_started_receipts WHERE attempt_id=$1"
        )
        .bind(r.binding.attempt_id)
        .fetch_one(&c.case.env.env.pool)
        .await
        .unwrap(),
        0
    );
    let human = aq_server::build_admin_router(c.case.env.env.state.clone())
        .oneshot(
            axum::http::Request::post("/v1/worker/profiled/launch-closure")
                .header(
                    "authorization",
                    format!("Bearer {}", c.case.env.env.credential),
                )
                .header("x-aq-request", "1")
                .header(
                    "origin",
                    c.case.env.env.state.config.server.public_origin.as_str(),
                )
                .body(axum::body::Body::from(serde_json::to_vec(&r).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(human.status(), 404);
    for statement in [
        "UPDATE profiled_launch_closures SET context_bytes=convert_to('{}','UTF8')",
        "DELETE FROM profiled_launch_closures",
        "DELETE FROM profiled_launch_registrations",
        "DELETE FROM profiled_native_registration_history",
    ] {
        assert!(
            sqlx::query(statement)
                .execute(&c.case.env.env.pool)
                .await
                .is_err()
        );
    }
    let mut changed = r.clone();
    changed.journal.digest = Digest::of(b"changed bytes");
    assert_eq!(post(&c, p.clone(), &changed).await.0, 409);
    let mut replacement = r.clone();
    replacement.operation_id = Uuid::new_v4();
    if let Evidence::Suppressed {
        record,
        record_digest,
    } = &mut replacement.evidence
    {
        record.operation_id = replacement.operation_id;
        *record_digest = record.digest().unwrap();
    }
    assert_eq!(post(&c, p, &replacement).await.0, 409);
}

#[tokio::test]
async fn renewal_counter_fences_old_suppression_and_closure_fences_all_later_authority() {
    let c = closure::setup().await;
    let mut r = request(&c).await;
    let p = policy(&c, &r, 1).await;
    let node = closure::auth(&c).await;
    let mut renew = aq_admission::transport::Renew {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: c.request.identity.clone(),
        binding: r.binding.clone(),
        grant_id: r.grant_id,
        reservation_id: r.reservation_id,
        guardian_id: r.guardian_id,
        launch_id: r.launch_id,
        journal_revision: c.request.journal_revision + 1,
    };
    let renewed = aq_server::profiled_execution::renewal::renew(
        &c.case.env.env.state,
        &node,
        &renew,
        Uuid::new_v4(),
    )
    .await
    .unwrap();
    // The old physical Prepared checkpoint is still retained, but suppression
    // cannot precede an acknowledged continuing-authority journal counter.
    assert_eq!(post(&c, p.clone(), &r).await.0, 409);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM profiled_launch_closures")
            .fetch_one(&c.case.env.env.pool)
            .await
            .unwrap(),
        0
    );
    r.operation_id = Uuid::new_v4();
    r.journal.revision = renew.journal_revision + 1;
    r.journal.digest = Digest::of(b"synthetic terminal journal after acknowledged renewal");
    if let Evidence::Suppressed {
        record,
        record_digest,
    } = &mut r.evidence
    {
        record.operation_id = r.operation_id;
        record.previous = JournalCheckpoint {
            revision: renew.journal_revision,
            digest: Digest::of(b"synthetic durable acknowledgement, not a server-invented digest"),
        };
        record.terminal_revision = r.journal.revision;
        *record_digest = record.digest().unwrap();
    }
    r.validate().unwrap();
    let (status, bytes) = post(&c, p.clone(), &r).await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
    // Both exact old-renew replay and a new operation must now refuse.
    for fresh in [false, true] {
        if fresh {
            renew.operation_id = Uuid::new_v4();
            renew.journal_revision = r.journal.revision + 1;
        }
        assert!(
            aq_server::profiled_execution::renewal::renew(
                &c.case.env.env.state,
                &node,
                &renew,
                Uuid::new_v4(),
            )
            .await
            .is_err()
        );
    }
    assert!(closure::grant(&c).await.is_err());
    let replay = post(&c, p, &r).await;
    assert_eq!(replay.0, 200);
    assert_eq!(replay.1, bytes);
    let hot: serde_json::Value =
        sqlx::query_scalar("SELECT state FROM profiled_authority_hot WHERE attempt_id=$1")
            .bind(r.binding.attempt_id)
            .fetch_one(&c.case.env.env.pool)
            .await
            .unwrap();
    assert_eq!(
        hot["grant"]["expires_at_ms"],
        renewed.delivery.clock.expires_at_ms
    );
    assert_eq!(hot["revoked"], true);
    assert!(
        sqlx::query_scalar::<_, bool>(
            "SELECT holds_task FROM execution_attempts WHERE attempt_id=$1"
        )
        .bind(r.binding.attempt_id)
        .fetch_one(&c.case.env.env.pool)
        .await
        .unwrap()
    );
}
#[tokio::test]
async fn policy_replacement_fences_old_owner_but_exact_history_can_use_new_current_policy() {
    let c = closure::setup().await;
    let r = request(&c).await;
    let old = policy(&c, &r, 1).await;
    let current = policy(&c, &r, 2).await;
    assert!(
        service::record(&c.case.env.env.state, &old, &closure::auth(&c).await, &r)
            .await
            .is_err()
    );
    assert!(
        HistoricalVerifierPolicy::new(1, vec![])
            .unwrap()
            .install(&c.case.env.env.state)
            .await
            .is_err()
    );
    assert!(
        HistoricalVerifierPolicy::new(2, vec![])
            .unwrap()
            .install(&c.case.env.env.state)
            .await
            .is_err()
    );
    let (status, bytes) = post(&c, current, &r).await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
    let retired = HistoricalVerifierPolicy::new(3, vec![])
        .unwrap()
        .install(&c.case.env.env.state)
        .await
        .unwrap();
    let (status, replay) = post(&c, retired, &r).await;
    assert_eq!(status, 200);
    assert_eq!(bytes, replay);
}

#[tokio::test]
async fn failed_receipt_insert_rolls_back_revocation_and_preserves_retry() {
    let c = closure::setup().await;
    let r = request(&c).await;
    let p = policy(&c, &r, 1).await;
    let before: serde_json::Value =
        sqlx::query_scalar("SELECT state FROM profiled_authority_hot WHERE attempt_id=$1")
            .bind(r.binding.attempt_id)
            .fetch_one(&c.case.env.env.pool)
            .await
            .unwrap();
    sqlx::raw_sql("CREATE FUNCTION fixture_refuse_closure() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected closure persistence failure'; END $$; CREATE TRIGGER z_fixture_refuse BEFORE INSERT ON profiled_launch_closures FOR EACH ROW EXECUTE FUNCTION fixture_refuse_closure();").execute(&c.case.env.env.pool).await.unwrap();
    assert!(
        service::record(&c.case.env.env.state, &p, &closure::auth(&c).await, &r)
            .await
            .is_err()
    );
    let after: serde_json::Value =
        sqlx::query_scalar("SELECT state FROM profiled_authority_hot WHERE attempt_id=$1")
            .bind(r.binding.attempt_id)
            .fetch_one(&c.case.env.env.pool)
            .await
            .unwrap();
    assert_eq!(before, after);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM profiled_launch_closures")
            .fetch_one(&c.case.env.env.pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM admission_material_leases WHERE prospective_attempt_id=$1 AND state='admitted'").bind(r.binding.attempt_id).fetch_one(&c.case.env.env.pool).await.unwrap(),c.plan.materials.len() as i64);
    sqlx::query("DROP TRIGGER z_fixture_refuse ON profiled_launch_closures")
        .execute(&c.case.env.env.pool)
        .await
        .unwrap();
    assert_eq!(post(&c, p, &r).await.0, 200);
}
#[tokio::test]
async fn expiry_and_active_policy_retirement_do_not_block_historical_closure() {
    use std::sync::atomic::Ordering;
    let mut c = closure::setup().await;
    let r = request(&c).await;
    let p = policy(&c, &r, 1).await;
    let deadline: i64 = sqlx::query_scalar(
        "SELECT (state->>'execution_deadline_ms')::BIGINT FROM profiled_authority_hot WHERE attempt_id=$1",
    )
    .bind(r.binding.attempt_id)
    .fetch_one(&c.case.env.env.pool)
    .await
    .unwrap();
    c.clock.store(deadline + 1, Ordering::SeqCst);
    Arc::make_mut(&mut c.case.env.env.state.config).profiled_execution = None;
    assert_eq!(post(&c, p, &r).await.0, 200);
}
#[test]
fn closure_path_is_explicit_reserved_control() {
    assert_eq!(
        aq_server::admission::classify("/v1/worker/profiled/launch-closure"),
        aq_server::admission::Class::Control
    );
}
fn started(r: &Request, prepared: u64) -> aq_admission::started::Request {
    let mut value: aq_admission::started::Request = serde_json::from_str(include_str!(
        "../../../aq-admission/artifacts/started.fixture.json"
    ))
    .unwrap();
    value.operation_id = Uuid::new_v4();
    value.binding = r.binding.clone();
    value.grant_id = r.grant_id;
    value.guardian_id = r.guardian_id;
    value.launch_id = r.launch_id;
    value.reservation_id = r.reservation_id;
    value.runtime = r.runtime.clone();
    value.qualification = r.qualification.clone();
    value.journal_revision = prepared + 1;
    value.journal_digest = Digest::of(b"synthetic physical Started checkpoint");
    value.observation.guardian.intent.launch_id = r.launch_id;
    value.observation.guardian.intent.session = r.binding.session.clone();
    value.observation.guardian.intent.manifest_digest = r.binding.launch_manifest_digest.clone();
    value.observation_digest = value.observation.digest().unwrap();
    value.validate().unwrap();
    value
}
#[tokio::test]
async fn started_and_closure_serialize_to_exactly_one_terminal_history() {
    let c = closure::setup().await;
    let mut r = request(&c).await; // reserve a later revision for a possible driver intent
    r.journal.revision += 2;
    if let Evidence::Suppressed {
        record,
        record_digest,
    } = &mut r.evidence
    {
        record.terminal_revision = r.journal.revision;
        *record_digest = record.digest().unwrap();
    }
    let s = started(&r, c.request.journal_revision);
    let p = policy(&c, &r, 1).await;
    let auth = closure::auth(&c).await;
    let (closed, started) = tokio::join!(
        service::record(&c.case.env.env.state, &p, &auth, &r),
        aq_server::profiled_execution::started::record(&c.case.env.env.state, &auth, &s)
    );
    assert_ne!(
        closed.is_ok(),
        started.is_ok(),
        "closed={closed:?} started={started:?}"
    );
    let closures: i64 = sqlx::query_scalar("SELECT count(*) FROM profiled_launch_closures")
        .fetch_one(&c.case.env.env.pool)
        .await
        .unwrap();
    let starts: i64 = sqlx::query_scalar("SELECT count(*) FROM profiled_started_receipts")
        .fetch_one(&c.case.env.env.pool)
        .await
        .unwrap();
    assert_eq!(closures + starts, 1);
}
#[tokio::test]
async fn actual_host_stopped_variant_retains_absent_prestart_pane_without_inventing_started() {
    use aq_admission::granted_closure::{DriverIntent, HostStoppedProof, StoppedObservation};
    let c = closure::setup().await;
    let mut r = request(&c).await;
    let running = started(&r, c.request.journal_revision).observation;
    let intent = DriverIntent {
        binding: r.binding.clone(),
        grant_id: r.grant_id,
        guardian_id: r.guardian_id,
        reservation_id: r.reservation_id,
        host: running.guardian.intent.clone(),
        journal: JournalCheckpoint {
            revision: c.request.journal_revision + 1,
            digest: Digest::of(b"synthetic persisted prehook launch intent"),
        },
    };
    let proof = HostStoppedProof {
        observation: StoppedObservation {
            guardian: running.guardian,
            phase: aq_admission::stopped::StoppedPhase::Stopped,
            pane: None,
            permit_sequence: 1,
            permit_deadline_boot_ms: 100,
            observed_boot_ms: 101,
            reaped_count: 0,
            kernel_wait_echild: true,
            root_status: None,
            stop_reason: None,
        },
    };
    r.journal.revision = c.request.journal_revision + 2;
    r.evidence = Evidence::HostStopped {
        driver_intent: Box::new(intent),
        proof_digest: proof.digest().unwrap(),
        proof,
    };
    r.validate().unwrap();
    let p = policy(&c, &r, 1).await;
    let (status, body) = post(&c, p, &r).await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
}

#[tokio::test]
async fn closure_http_uses_reserved_control_capacity_and_refuses_at_its_shared_bound() {
    let c = closure::setup().await;
    let r = request(&c).await;
    let p = policy(&c, &r, 1).await;
    for (path, slots, expected) in [
        (
            "/fixture-held-general",
            aq_server::admission::GENERAL_LIMIT,
            200,
        ),
        (
            "/v1/worker/profiled/prepare",
            aq_server::admission::CONTROL_LIMIT,
            503,
        ),
    ] {
        let entered = Arc::new(tokio::sync::Semaphore::new(0));
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let start = entered.clone();
        let finish = release.clone();
        // A fixture-only blocked handler exercises the SAME admission instance as
        // the real injected closure router; it claims no authority owner behavior.
        let held = axum::Router::new()
            .route(
                path,
                axum::routing::post(move || {
                    let start = start.clone();
                    let finish = finish.clone();
                    async move {
                        start.add_permits(1);
                        let permit = finish.acquire().await.unwrap();
                        permit.forget();
                        axum::http::StatusCode::OK
                    }
                }),
            )
            .layer(axum::middleware::from_fn_with_state(
                aq_server::test_support::admission(&c.case.env.env.state.store),
                aq_server::admission::gate,
            ));
        let mut tasks = vec![];
        for _ in 0..slots {
            let held = held.clone();
            tasks.push(tokio::spawn(async move {
                held.oneshot(
                    axum::http::Request::post(path)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
            }));
        }
        let started = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            entered.acquire_many(slots as u32),
        )
        .await
        .unwrap()
        .unwrap();
        started.forget();
        let (status, body) = post(&c, p.clone(), &r).await;
        assert_eq!(status, expected, "{}", String::from_utf8_lossy(&body));
        release.add_permits(slots);
        for t in tasks {
            assert_eq!(t.await.unwrap().status(), 200);
        }
    }
}
#[tokio::test]
async fn credential_rotation_during_node_lock_wait_cannot_commit_a_closure() {
    let c = closure::setup().await;
    let r = request(&c).await;
    let p = policy(&c, &r, 1).await;
    let auth = closure::auth(&c).await;
    let mut barrier = c.case.env.env.pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(auth.id())
        .execute(&mut *barrier)
        .await
        .unwrap();
    let state = c.case.env.env.state.clone();
    let request = r.clone();
    let owner = p.clone();
    let node = auth.clone();
    let pending =
        tokio::spawn(async move { service::record(&state, &owner, &node, &request).await });
    tokio::time::timeout(std::time::Duration::from_millis(400),async {
  loop {let waiting:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND wait_event IS NOT NULL AND query LIKE 'SELECT id FROM nodes WHERE id=$1 AND revoked_at IS NULL AND credential_hash=$2 FOR UPDATE%')").fetch_one(&c.case.env.env.pool).await.unwrap();if waiting{break}tokio::time::sleep(std::time::Duration::from_millis(2)).await;}
 }).await.expect("observe current credential lock wait inside500ms SQL bound");
    sqlx::query("UPDATE nodes SET credential_hash=$2 WHERE id=$1")
        .bind(auth.id())
        .bind("f".repeat(64))
        .execute(&mut *barrier)
        .await
        .unwrap();
    barrier.commit().await.unwrap();
    assert!(pending.await.unwrap().is_err());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM profiled_launch_closures")
            .fetch_one(&c.case.env.env.pool)
            .await
            .unwrap(),
        0
    );
    assert!(
        !sqlx::query_scalar::<_, bool>(
            "SELECT (state->>'revoked')::BOOLEAN FROM profiled_authority_hot WHERE attempt_id=$1"
        )
        .bind(r.binding.attempt_id)
        .fetch_one(&c.case.env.env.pool)
        .await
        .unwrap()
    );
}

/// The owner's abandon covers a granted launch closed before the agent started. Such an
/// attempt has no stopped receipt; its stop proof is the launch closure's receipt.
#[tokio::test]
async fn owner_abandons_a_launch_closed_before_it_started() {
    let c = closure::setup().await;
    let r = request(&c).await;
    let p = policy(&c, &r, 1).await;
    let (status, bytes) = post(&c, p, &r).await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
    let attempt = r.binding.attempt_id;
    let pool = &c.case.env.env.pool;
    let held = || async {
        sqlx::query_scalar::<_, bool>(
            "SELECT holds_task FROM execution_attempts WHERE attempt_id=$1",
        )
        .bind(attempt)
        .fetch_one(pool)
        .await
        .unwrap()
    };
    assert!(held().await, "a closed launch keeps its claim");
    let state = &c.case.env.env.state;
    let abandon = |email: &'static str, body: serde_json::Value| async move {
        let response = aq_server::build_admin_router(state.clone())
            .oneshot(
                axum::http::Request::post(format!("/api/execution/attempts/{attempt}/disposition"))
                    .header("content-type", "application/json")
                    .header("x-aq-dev-email", email)
                    .header("X-AQ-Request", "1")
                    .body(axum::body::Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
        )
    };
    let body = serde_json::json!({"operation_id": Uuid::new_v4(), "reason": "launch failed before the agent started"});
    let (status, receipt) = abandon("owner@localhost", body.clone()).await;
    assert_eq!(status, 200, "{receipt}");
    assert_eq!(receipt["disposition"], "abandoned");
    let closure: String = sqlx::query_scalar(
        "SELECT receipt_digest FROM profiled_launch_closures WHERE attempt_id=$1",
    )
    .bind(attempt)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(
        receipt["stop_receipt_digest"], closure,
        "the stop proof is the closure receipt"
    );
    assert!(!held().await, "the claim is released");
    let (status, replay) = abandon("owner@localhost", body).await;
    assert_eq!((status, &replay), (axum::http::StatusCode::OK, &receipt));
}
