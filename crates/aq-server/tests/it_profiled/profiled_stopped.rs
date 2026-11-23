//! Real SQL and HTTP stop owner checks; host observations are synthetic here.
//! Physical proof production is independently qualified against the tmux host.
use crate::common;
use aq_server::profiled_execution::{started, stopped};
use common::{
    native_started::{context, setup, state, stop_request},
    profiled::NOW,
};
use serde_json::json;
use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

async fn observe_blocked_transaction(pool: &sqlx::PgPool, xid: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    loop {
        let blocked: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM pg_locks WHERE transactionid::text=$1 AND NOT granted)",
        )
        .bind(xid)
        .fetch_one(pool)
        .await
        .unwrap();
        if blocked {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "stop report never reached the held node transaction"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn hot_revisions(pool: &sqlx::PgPool, attempt: uuid::Uuid) -> (i64, i64, i64) {
    sqlx::query_as("SELECT record_revision,authority_revision,grant_revision FROM profiled_authority_hot WHERE attempt_id=$1")
        .bind(attempt).fetch_one(pool).await.unwrap()
}

#[tokio::test]
async fn expired_and_cancelled_stop_is_atomic_keeps_claim_and_replays_without_new_authority() {
    let (mut f, started_request) = setup(true).await;
    let auth = context(&f).await;
    started::record(&f.state, &auth, &started_request)
        .await
        .unwrap();
    let pool = f.db.pool().await;
    let mut before = state(&f, &started_request).await;
    before["status"] = json!("cancelling");
    before["cancellation_id"] = json!(uuid::Uuid::new_v4());
    before["revoked"] = json!(true);
    sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
        .bind(started_request.binding.attempt_id)
        .bind(&before)
        .execute(&pool)
        .await
        .unwrap();
    f.time.store((NOW + 40_000) as i64, Ordering::SeqCst);
    // Removing new-launch configuration must not strand an admitted host stop.
    let mut config = (*f.state.config).clone();
    config.profiled_execution = None;
    f.state.config = Arc::new(config);
    let request = stop_request(&started_request);
    let receipt = stopped::record(&f.state, &auth, &request).await.unwrap();
    receipt.validate_for(&request).unwrap();
    assert_eq!(receipt.recorded_at_ms, NOW + 40_000);
    let after = state(&f, &started_request).await;
    assert_eq!(after["status"], "finished");
    assert_eq!(
        after["stop"],
        json!({"kind":"stopped","receipt":receipt.artifact().unwrap()})
    );
    for field in [
        "grant",
        "execution_deadline_ms",
        "cancellation_id",
        "revoked",
        "retention",
        "effects",
    ] {
        assert_eq!(after[field], before[field], "{field} must not change");
    }
    let held: bool =
        sqlx::query_scalar("SELECT holds_task FROM execution_attempts WHERE attempt_id=$1")
            .bind(started_request.binding.attempt_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(held);
    let phase: String =
        sqlx::query_scalar("SELECT phase FROM profiled_owner_journals WHERE attempt_id=$1")
            .bind(started_request.binding.attempt_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(phase, "stopped");
    let revisions = hot_revisions(&pool, started_request.binding.attempt_id).await;
    f.time.store((NOW + 80_000) as i64, Ordering::SeqCst);
    assert_eq!(
        json!(stopped::record(&f.state, &auth, &request).await.unwrap()),
        json!(receipt)
    );
    let again = hot_revisions(&pool, started_request.binding.attempt_id).await;
    assert_eq!(again, revisions);
    assert_eq!(state(&f, &started_request).await, after);
    let mut changed = request.clone();
    changed.evidence.stop_operation_id = uuid::Uuid::new_v4();
    assert!(stopped::record(&f.state, &auth, &changed).await.is_err());
    changed = request.clone();
    changed.evidence.reason = aq_admission::transport::StopReason::Cancellation;
    assert!(stopped::record(&f.state, &auth, &changed).await.is_err());
    assert!(
        sqlx::query("DELETE FROM profiled_stopped_receipts WHERE attempt_id=$1")
            .bind(started_request.binding.attempt_id)
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(sqlx::query("UPDATE profiled_owner_journals SET phase='started',revision=revision+1 WHERE attempt_id=$1").bind(started_request.binding.attempt_id).execute(&pool).await.is_err());
}

#[tokio::test]
async fn missing_started_or_foreign_physical_owner_cannot_settle() {
    let (f, start) = setup(true).await;
    let auth = context(&f).await;
    let request = stop_request(&start);
    assert!(stopped::record(&f.state, &auth, &request).await.is_err());
    started::record(&f.state, &auth, &start).await.unwrap();
    for field in ["nonce", "pane", "configuration", "journal"] {
        let mut changed = request.clone();
        match field {
            "nonce" => changed.proof.observation.guardian.nonce = uuid::Uuid::new_v4(),
            "pane" => changed.proof.observation.pane.pane.start_ticks += 1,
            "configuration" => {
                changed
                    .proof
                    .observation
                    .guardian
                    .intent
                    .configuration_digest = aq_admission::Digest::of(b"other")
            }
            "journal" => {
                changed.evidence.journal = aq_admission::transport::JournalEvidence::Recorded {
                    revision: start.journal_revision,
                    digest: aq_admission::Digest::of(b"regression"),
                }
            }
            _ => unreachable!(),
        }
        changed.evidence.disposition = aq_admission::transport::StopDisposition::Stopped {
            grant_id: start.grant_id,
            stop_receipt_digest: changed.proof.digest().unwrap(),
        };
        changed.validate().unwrap();
        assert!(
            stopped::record(&f.state, &auth, &changed).await.is_err(),
            "{field}"
        );
    }
    assert_eq!(state(&f, &start).await["stop"]["kind"], "running");
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM profiled_stopped_receipts")
            .fetch_one(&f.db.pool().await)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn journal_failure_rolls_back_receipt_and_stop_then_retry_succeeds() {
    let (f, start) = setup(true).await;
    let auth = context(&f).await;
    started::record(&f.state, &auth, &start).await.unwrap();
    let pool = f.db.pool().await;
    let request = stop_request(&start);
    sqlx::raw_sql("CREATE FUNCTION fail_stopped() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.phase='stopped' THEN RAISE EXCEPTION 'synthetic journal fault'; END IF; RETURN NEW; END $$; CREATE TRIGGER fail_stopped BEFORE UPDATE ON profiled_owner_journals FOR EACH ROW EXECUTE FUNCTION fail_stopped();")
        .execute(&pool).await.unwrap();
    assert!(stopped::record(&f.state, &auth, &request).await.is_err());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM profiled_stopped_receipts")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(state(&f, &start).await["stop"]["kind"], "running");
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT phase FROM profiled_owner_journals WHERE attempt_id=$1"
        )
        .bind(start.binding.attempt_id)
        .fetch_one(&pool)
        .await
        .unwrap(),
        "started"
    );
    sqlx::query("DROP TRIGGER fail_stopped ON profiled_owner_journals")
        .execute(&pool)
        .await
        .unwrap();
    stopped::record(&f.state, &auth, &request).await.unwrap();
}

#[tokio::test]
async fn renewed_shorter_permit_is_valid_but_rotated_owner_cannot_replay() {
    let (f, start) = setup(true).await;
    let auth = context(&f).await;
    started::record(&f.state, &auth, &start).await.unwrap();
    let mut request = stop_request(&start);
    request.proof.observation.permit_sequence += 1;
    request.proof.observation.permit_deadline_boot_ms -= 1;
    request.evidence.disposition = aq_admission::transport::StopDisposition::Stopped {
        grant_id: start.grant_id,
        stop_receipt_digest: request.proof.digest().unwrap(),
    };
    stopped::record(&f.state, &auth, &request).await.unwrap();
    let pool = f.db.pool().await;
    let credential = aq_server::tokens::new_node_credential();
    sqlx::query("UPDATE nodes SET credential_hash=$2 WHERE id=$1")
        .bind(f.node.node_id)
        .bind(aq_server::tokens::verifier(&credential))
        .execute(&pool)
        .await
        .unwrap();
    assert!(stopped::record(&f.state, &auth, &request).await.is_err());
}

#[tokio::test]
async fn stop_waiting_for_node_lock_accepts_expiry_and_records_time_after_lock() {
    let (f, start) = setup(true).await;
    let auth = context(&f).await;
    started::record(&f.state, &auth, &start).await.unwrap();
    let pool = f.db.pool().await;
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(f.node.node_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    let blocker_xid: String = sqlx::query_scalar("SELECT txid_current()::text")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    let app = f.state.clone();
    let request = stop_request(&start);
    let call = tokio::spawn(async move { stopped::record(&app, &auth, &request).await });
    observe_blocked_transaction(&pool, &blocker_xid).await;
    f.time.store((NOW + 90_000) as i64, Ordering::SeqCst);
    tx.commit().await.unwrap();
    assert_eq!(call.await.unwrap().unwrap().recorded_at_ms, NOW + 90_000);
}

#[tokio::test]
async fn worker_http_stop_is_bounded_authenticated_and_historical_only() {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;
    let (f, start) = setup(true).await;
    started::record(&f.state, &context(&f).await, &start)
        .await
        .unwrap();
    let r = stop_request(&start);
    let router = aq_server::build_worker_router(f.state.clone());
    let request = |token: &str, body: Vec<u8>| {
        Request::builder()
            .method("POST")
            .uri("/v1/worker/profiled/stopped")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap()
    };
    let bytes = serde_json::to_vec(&r).unwrap();
    assert_eq!(
        router
            .clone()
            .oneshot(request("aqn_wrong", bytes.clone()))
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        router
            .clone()
            .oneshot(request(&f.credential, vec![b' '; 16385]))
            .await
            .unwrap()
            .status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let response = router
        .clone()
        .oneshot(request(&f.credential, bytes.clone()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let response = axum::body::to_bytes(response.into_body(), 16384)
        .await
        .unwrap();
    aq_admission::stopped::decode_receipt(&response, &r).unwrap();
    sqlx::query("UPDATE nodes SET revoked_at=now() WHERE id=$1")
        .bind(f.node.node_id)
        .execute(&f.db.pool().await)
        .await
        .unwrap();
    assert_ne!(
        router
            .oneshot(request(&f.credential, bytes))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        aq_server::admission::classify("/v1/worker/profiled/stopped"),
        aq_server::admission::Class::Control
    );
}

#[tokio::test]
async fn credentials_rotated_during_lock_wait_refuse_first_stop_without_mutation() {
    let (f, start) = setup(true).await;
    let auth = context(&f).await;
    started::record(&f.state, &auth, &start).await.unwrap();
    let pool = f.db.pool().await;
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(f.node.node_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    let blocker_xid: String = sqlx::query_scalar("SELECT txid_current()::text")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    let app = f.state.clone();
    let request = stop_request(&start);
    let call = tokio::spawn(async move { stopped::record(&app, &auth, &request).await });
    observe_blocked_transaction(&pool, &blocker_xid).await;
    sqlx::query("UPDATE nodes SET credential_hash=$2 WHERE id=$1")
        .bind(f.node.node_id)
        .bind(aq_server::tokens::verifier(
            &aq_server::tokens::new_node_credential(),
        ))
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(call.await.unwrap().is_err());
    assert_eq!(state(&f, &start).await["stop"]["kind"], "running");
}

#[tokio::test]
async fn sql_rejects_digest_only_physical_substitution_and_receipt_without_atomic_projection() {
    use sqlx::Row;
    fn bytes(value: &impl serde::Serialize) -> Vec<u8> {
        let mut value = serde_json::to_value(value).unwrap();
        value.sort_all_objects();
        serde_json::to_vec(&value).unwrap()
    }
    let (f, start) = setup(true).await;
    let auth = context(&f).await;
    started::record(&f.state, &auth, &start).await.unwrap();
    let pool = f.db.pool().await;
    let original = stop_request(&start);
    for change in ["nonce", "digest", "atomic"] {
        let mut request = original.clone();
        if change == "nonce" {
            request.proof.observation.guardian.nonce = uuid::Uuid::new_v4();
        }
        request.evidence.disposition = aq_admission::transport::StopDisposition::Stopped {
            grant_id: start.grant_id,
            stop_receipt_digest: request.proof.digest().unwrap(),
        };
        let proof = bytes(&request.proof);
        let mut tx = pool.begin().await.unwrap();
        let row=sqlx::query("SELECT record_revision,authority_revision,grant_revision FROM profiled_authority_hot WHERE attempt_id=$1 FOR UPDATE").bind(start.binding.attempt_id).fetch_one(&mut *tx).await.unwrap();
        let receipt = aq_admission::stopped::Receipt {
            schema_version: 1,
            operation_id: request.evidence.stop_operation_id,
            binding: request.evidence.binding.clone(),
            grant_id: start.grant_id,
            request_digest: request.digest().unwrap(),
            recorded_at_ms: NOW,
            authority_revision_at_record: row.get::<i64, _>("authority_revision") as u64 + 1,
        };
        let proof_digest = if change == "digest" {
            aq_admission::Digest::of(b"invented digest")
        } else {
            aq_admission::Digest::of(&proof)
        };
        let inserted=sqlx::query("INSERT INTO profiled_stopped_receipts(attempt_id,node_id,operation_id,request_bytes,request_digest,proof_bytes,proof_digest,receipt_bytes,before_record_revision,before_grant_revision) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)")
            .bind(start.binding.attempt_id).bind(f.node.node_id).bind(request.evidence.stop_operation_id)
            .bind(bytes(&request)).bind(request.digest().unwrap().as_str()).bind(&proof).bind(proof_digest.as_str()).bind(bytes(&receipt))
            .bind(row.get::<i64,_>("record_revision")).bind(row.get::<i64,_>("grant_revision")).execute(&mut *tx).await;
        if change == "atomic" {
            inserted.unwrap();
            assert!(tx.commit().await.is_err());
        } else {
            assert!(inserted.is_err(), "{change}");
            tx.rollback().await.unwrap();
        }
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM profiled_stopped_receipts")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(state(&f, &start).await["stop"]["kind"], "running");
}

/// A normal stop revokes the attempt's agent credential in the stop
/// transaction (spec 0006 §5.4), so the agent channel's credential is refused
/// afterwards, and a replayed stop changes nothing. Before this the verifier
/// stayed live until its TTL (found by the spec 0006 phase 6 live proof).
#[tokio::test]
async fn normal_stop_revokes_the_agent_credential_and_replays_idempotently() {
    use std::os::unix::fs::PermissionsExt;
    use tower::ServiceExt;
    let (f, start) = setup(true).await;
    let auth = context(&f).await;
    started::record(&f.state, &auth, &start).await.unwrap();
    let pool = f.db.pool().await;
    let attempt = start.binding.attempt_id;
    let aq_admission::SessionIdentity::Remote {
        boot_id,
        assignment_generation,
        session_id,
        ..
    } = start.binding.session
    else {
        panic!("profiled attempts run remotely")
    };
    let task: uuid::Uuid =
        sqlx::query_scalar("SELECT task_id FROM execution_attempts WHERE attempt_id=$1")
            .bind(attempt)
            .fetch_one(&pool)
            .await
            .unwrap();
    let launch = json!({"attempt_id":attempt,"task_id":task,"project_id":f.selection.lineage.project_id,
        "session_id":session_id,"generation":assignment_generation,"boot_id":boot_id});
    let token = format!("aqa_{}", "A".repeat(43));
    sqlx::query("INSERT INTO agent_launch_bindings(attempt_id,node_id,binding,profile,context,credential_version,credential_verifier,credential_expires_at_ms) VALUES($1,$2,$3,'{}','{}',1,$4,$5)")
        .bind(attempt).bind(f.node.node_id).bind(&launch).bind(aq_server::tokens::verifier(&token))
        .bind((NOW + 600_000) as i64).execute(&pool).await.unwrap();
    let binding = || async {
        sqlx::query_as::<_, (bool, Option<String>, Option<i64>, i64)>("SELECT revoked,credential_verifier,credential_expires_at_ms,credential_version FROM agent_launch_bindings WHERE attempt_id=$1")
            .bind(attempt).fetch_one(&pool).await.unwrap()
    };
    // The command surface with an agent-access runtime, as a server with agent
    // access configured has; without one it answers 503 before it reads the
    // credential. Authentication never calls the graph origin.
    let dir = tempfile::tempdir().unwrap();
    let graph_token = dir.path().join("graph-token");
    std::fs::write(&graph_token, "g".repeat(32)).unwrap();
    std::fs::set_permissions(&graph_token, std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut agent_state = f.state.clone();
    agent_state.agent_access = Some(Arc::new(
        aq_server::agent_access::Runtime::new(&aq_server::agent_access::AgentAccessConfig {
            graph_url: "http://127.0.0.1:9/".into(),
            graph_token_file: graph_token,
            credential_ttl_ms: 10_000,
            mutation_actor_id: None,
        })
        .await
        .unwrap(),
    ));
    let commands = aq_server::build_worker_router(agent_state);
    let whoami = || async {
        commands
            .clone()
            .oneshot(
                axum::http::Request::post("/api/v1/commands/whoami")
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(r#"{"params":{}}"#))
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
    };
    // Before the stop the channel's credential is accepted.
    assert_eq!(whoami().await, axum::http::StatusCode::OK);
    let request = stop_request(&start);
    let receipt = stopped::record(&f.state, &auth, &request).await.unwrap();
    assert_eq!(state(&f, &start).await["status"], "finished");
    let revoked = binding().await;
    assert_eq!(
        (revoked.0, revoked.1.as_deref(), revoked.2),
        (true, None, None)
    );
    assert_eq!(
        revoked.3, 2,
        "the credential version moves past the revoked one"
    );
    // After it the same credential is refused: the command surface answers 401.
    assert_eq!(whoami().await, axum::http::StatusCode::UNAUTHORIZED);
    // A replayed stop returns the same receipt and revokes nothing again.
    assert_eq!(
        json!(stopped::record(&f.state, &auth, &request).await.unwrap()),
        json!(receipt)
    );
    assert_eq!(binding().await, revoked);
}
