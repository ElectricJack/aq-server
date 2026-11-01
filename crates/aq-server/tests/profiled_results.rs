//! Disposable actual SQL owners; synthetic launch/host fixture, no provider run.
//! Ordered migrations must create the ledger in the assembled daemon.
mod common;
use aq_admission::{
    execution_spec::ProfiledExecutionSpec,
    transport::{self, ResultReport},
    *,
};
use aq_server::profiled_execution::{artifacts, results, started, stopped};
use common::{
    native_started::{self, context},
    profiled::{Fixture, NOW, artifact, fixture},
};
use serde_json::json;
use std::sync::{Arc, atomic::Ordering};
use uuid::Uuid;
async fn prepare(
    pin: bool,
    stop_now: bool,
) -> (Fixture, aq_admission::started::Request, ResultReport) {
    let mut f = fixture().await;
    let task_context = artifact("synthetic-task-context", b"synthetic task");
    f.profiled
        .offer
        .manifest
        .context
        .references
        .push(aq_session_protocol::ContextRef {
            artifact: task_context.clone(),
            provenance: Text::new("synthetic-test-task").unwrap(),
            delivery: aq_session_protocol::ContextDelivery::Startup,
            bytes: 14,
            tokens: 4,
            required: true,
        });
    let selection_digest = aq_selection::digest(&f.selection).unwrap();
    f.profiled.offer.manifest.selection_explanation = ArtifactRef {
        id: Text::new("sealed-selection").unwrap(),
        version: Text::new("1").unwrap(),
        digest: selection_digest.clone(),
    };
    f.profiled.offer.binding.launch_manifest_digest =
        aq_session_protocol::canonical_digest(&f.profiled.offer.manifest).unwrap();
    let mut spec = ProfiledExecutionSpec {
        schema_version: 1,
        lifecycle: transport::lifecycle_binding(&f.profiled.offer).unwrap(),
        project_revision: 1,
        task: TaskRevision {
            workspace_id: f.selection.lineage.workspace_id,
            task_id: f.selection.lineage.task_id,
            version: f.profiled.offer.manifest.bindings.task_revision,
            content_revision: 1,
        },
        source: SourceAdmission {
            schema_version: 1,
            project_id: f.selection.lineage.project_id,
            project_revision: 1,
            repository_id: f.profiled.source.repository_id,
            repository_binding_generation: 1,
            policy: artifact("synthetic-source-policy", b"immutable-base-policy"),
            selection: SourceSelection::Immutable {
                object: ObjectId {
                    format: ObjectFormat::Sha1,
                    hex: f.profiled.source.base.hex.clone(),
                },
            },
            resolved_object: ObjectId {
                format: ObjectFormat::Sha1,
                hex: f.profiled.source.base.hex.clone(),
            },
            observed_at_ms: NOW,
            expires_at_ms: NOW + 30000,
        },
        source_artifact: f.profiled.offer.manifest.bindings.source.clone(),
        outcome_artifact: f.profiled.offer.manifest.bindings.outcome.clone(),
        graph_access_profile: f
            .profiled
            .offer
            .manifest
            .bindings
            .graph_access_profile
            .clone(),
        task_observation: artifact("synthetic-task-observation", b"graph-context"),
        task_context,
        selection_digest,
    };
    let mut outcome: outcome::ExpectedOutcomeDefinition = serde_json::from_str(include_str!(
        "../../aq-admission/artifacts/outcome.fixture.json"
    ))
    .unwrap();
    outcome.project_id = spec.lifecycle.project_id;
    outcome.project_revision = spec.project_revision;
    outcome.task = spec.task.clone();
    outcome.source = outcome::SourceAnchor {
        repository_id: spec.source.repository_id,
        binding_generation: 1,
        base: spec.source.resolved_object.clone(),
        artifact: spec.source_artifact.clone(),
    };
    outcome.task_observation = spec.task_observation.clone();
    outcome.task_context = spec.task_context.clone();
    spec.outcome_artifact = outcome.artifact().unwrap();
    f.profiled.offer.manifest.bindings.outcome = spec.outcome_artifact.clone();
    f.profiled.offer.binding.launch_manifest_digest =
        aq_session_protocol::canonical_digest(&f.profiled.offer.manifest).unwrap();
    f.profiled.offer.binding.execution_spec_digest = spec.artifact().unwrap().digest;
    outcome.validate_against(&spec).unwrap();
    let (f, start) = native_started::setup_fixture(f, true, true, Some(&spec)).await;
    let pool = f.db.pool().await;
    let installed: bool =
        sqlx::query_scalar("SELECT to_regclass('public.profiled_result_reports') IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(installed, "ordered migration024 must install report owner");
    if pin {
        let id = Uuid::new_v4();
        let bytes = outcome.canonical_bytes().unwrap();
        artifacts::retain(
            &f.state,
            artifacts::Retain {
                artifact_id: id,
                project_id: outcome.project_id,
                owner: artifacts::OwnerKind::Outcome,
                artifact: outcome.artifact().unwrap(),
                kind: ArtifactKind::Outcome,
                bytes: &bytes,
                retain_until_ms: NOW + 60000,
            },
        )
        .await
        .unwrap();
        let mut tx = pool.begin().await.unwrap();
        artifacts::link_locked(
            &mut tx,
            start.binding.attempt_id,
            outcome.project_id,
            &[id],
            NOW,
            NOW + 30000,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }
    let auth = context(&f).await;
    started::record(&f.state, &auth, &start).await.unwrap();
    let stop = native_started::stop_request(&start);
    if stop_now {
        stopped::record(&f.state, &auth, &stop).await.unwrap();
    }
    let report = ResultReport {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: transport::WorkerIdentity {
            node_id: f.node.node_id,
            boot_id: f.node.boot_id,
        },
        binding: start.binding.clone(),
        report_id: Uuid::new_v4(),
        supersedes: None,
        outcome: transport::ProcessOutcome::Success,
        stop: stop.evidence,
        source_receipt_ids: vec![Uuid::new_v4()],
        pending_effect_ids: vec![],
        conclusion: Label::new("worker claims task is done; unverified").unwrap(),
    };
    result_record::validate_report(&report).unwrap();
    (f, start, report)
}
#[tokio::test]
async fn recording_retains_claims_replays_and_serializes_corrections_without_settlement() {
    let (mut f, start, report) = prepare(true, true).await;
    let auth = context(&f).await;
    let pool = f.db.pool().await;
    let before = native_started::state(&f, &start).await;
    f.time.store((NOW + 90000) as i64, Ordering::SeqCst);
    let mut cfg = (*f.state.config).clone();
    cfg.profiled_execution = None;
    f.state.config = Arc::new(cfg);
    let receipt = results::record(&f.state, &auth, &report).await.unwrap();
    receipt.validate_for(&report).unwrap();
    assert_eq!(receipt.disposition, result_record::Disposition::Recorded);
    assert_eq!(receipt.recorded_at_ms, NOW + 90000);
    f.time.store((NOW + 120000) as i64, Ordering::SeqCst);
    assert_eq!(
        json!(results::record(&f.state, &auth, &report).await.unwrap()),
        json!(receipt)
    );
    let mut one = report.clone();
    one.report_id = Uuid::new_v4();
    one.operation_id = Uuid::new_v4();
    one.supersedes = Some(report.report_id);
    one.conclusion = Label::new("first correction").unwrap();
    let mut two = one.clone();
    two.report_id = Uuid::new_v4();
    two.operation_id = Uuid::new_v4();
    let (a, b) = tokio::join!(
        results::record(&f.state, &auth, &one),
        results::record(&f.state, &auth, &two)
    );
    assert_ne!(a.is_ok(), b.is_ok());
    assert_eq!(native_started::state(&f, &start).await, before);
    let held: bool =
        sqlx::query_scalar("SELECT holds_task FROM execution_attempts WHERE attempt_id=$1")
            .bind(report.binding.attempt_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(held);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM profiled_result_reports")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 2);
    let mut changed = report.clone();
    changed.conclusion = Label::new("changed identity reuse").unwrap();
    assert!(results::record(&f.state, &auth, &changed).await.is_err());
    changed = report.clone();
    changed.operation_id = Uuid::new_v4();
    assert!(results::record(&f.state, &auth, &changed).await.is_err());
    changed = report;
    changed.report_id = Uuid::new_v4();
    assert!(results::record(&f.state, &auth, &changed).await.is_err());
}
#[tokio::test]
async fn stopped_and_pinned_typed_outcome_are_actual_required_owners() {
    for (pin, stop) in [(false, true), (true, false)] {
        let (f, _, report) = prepare(pin, stop).await;
        let auth = context(&f).await;
        assert!(results::record(&f.state, &auth, &report).await.is_err());
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM profiled_result_reports")
            .fetch_one(&f.db.pool().await)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }
    let (f, _, mut report) = prepare(true, true).await;
    let auth = context(&f).await;
    report.stop.reason = transport::StopReason::Cancellation;
    assert!(results::record(&f.state, &auth, &report).await.is_err());
}
#[tokio::test]
async fn report_quota_preserves_exact_replay_and_immutable_heads() {
    let (f, _, mut report) = prepare(true, true).await;
    let auth = context(&f).await;
    let pool = f.db.pool().await;
    let first = report.clone();
    let receipt = results::record(&f.state, &auth, &report).await.unwrap();
    for _ in 1..64 {
        let old = report.report_id;
        report.report_id = Uuid::new_v4();
        report.operation_id = Uuid::new_v4();
        report.supersedes = Some(old);
        results::record(&f.state, &auth, &report).await.unwrap();
    }
    report.supersedes = Some(report.report_id);
    report.report_id = Uuid::new_v4();
    report.operation_id = Uuid::new_v4();
    assert!(results::record(&f.state, &auth, &report).await.is_err());
    assert_eq!(
        json!(results::record(&f.state, &auth, &first).await.unwrap()),
        json!(receipt)
    );
    assert!(
        sqlx::query("DELETE FROM profiled_result_heads")
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM profiled_result_reports")
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE profiled_result_reports SET operation_id=$1")
            .bind(Uuid::new_v4())
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE profiled_result_heads SET report_id=$1")
            .bind(first.report_id)
            .execute(&pool)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn rotated_credential_after_observed_lock_wait_cannot_record() {
    let (f, start, report) = prepare(true, true).await;
    let auth = context(&f).await;
    let pool = f.db.pool().await;
    let before = native_started::state(&f, &start).await;
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(f.node.node_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    let xid: String = sqlx::query_scalar("SELECT txid_current()::text")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    let app = f.state.clone();
    let call = tokio::spawn(async move { results::record(&app, &auth, &report).await });
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
    loop {
        let waiting: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM pg_locks WHERE transactionid::text=$1 AND NOT granted)",
        )
        .bind(&xid)
        .fetch_one(&pool)
        .await
        .unwrap();
        if waiting {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
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
    assert_eq!(native_started::state(&f, &start).await, before);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM profiled_result_reports")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}
#[tokio::test]
async fn sql_requires_exact_stopped_outcome_and_atomic_report_head() {
    use sqlx::Row;
    fn bytes(value: &impl serde::Serialize) -> Vec<u8> {
        let mut v = serde_json::to_value(value).unwrap();
        v.sort_all_objects();
        serde_json::to_vec(&v).unwrap()
    }
    let (f, _, first) = prepare(true, true).await;
    let auth = context(&f).await;
    let pool = f.db.pool().await;
    let receipt = results::record(&f.state, &auth, &first).await.unwrap();
    let owner =
        sqlx::query("SELECT outcome_artifact_id FROM profiled_result_reports WHERE report_id=$1")
            .bind(first.report_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    for case in ["stop", "outcome", "accepted", "atomic"] {
        let mut report = first.clone();
        report.report_id = Uuid::new_v4();
        report.operation_id = Uuid::new_v4();
        report.supersedes = Some(first.report_id);
        if case == "stop" {
            report.stop.guardian_id = Uuid::new_v4();
        }
        let mut ack = receipt.clone();
        ack.report_id = report.report_id;
        ack.operation_id = report.operation_id;
        ack.report_digest = result_record::report_digest(&report).unwrap();
        if case == "outcome" {
            ack.expected_outcome.digest = Digest::of(b"foreign");
        }
        let mut ack = json!(ack);
        if case == "accepted" {
            ack["disposition"] = json!("accepted");
        }
        let body = bytes(&report);
        let mut tx = pool.begin().await.unwrap();
        let inserted=sqlx::query("INSERT INTO profiled_result_reports(report_id,attempt_id,node_id,operation_id,supersedes,report_bytes,report_digest,receipt_bytes,outcome_artifact_id) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)").bind(report.report_id).bind(report.binding.attempt_id).bind(f.node.node_id).bind(report.operation_id).bind(report.supersedes).bind(&body).bind(Digest::of(&body).as_str()).bind(bytes(&ack)).bind(owner.get::<Uuid,_>("outcome_artifact_id")).execute(&mut *tx).await;
        if case == "atomic" {
            inserted.unwrap();
            assert!(tx.commit().await.is_err());
        } else {
            assert!(inserted.is_err(), "{case}");
            tx.rollback().await.unwrap();
        }
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM profiled_result_reports")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}
#[tokio::test]
async fn cancellation_is_retained_without_turning_claim_into_success() {
    let (f, start, report) = prepare(true, false).await;
    let auth = context(&f).await;
    let pool = f.db.pool().await;
    let mut hot = native_started::state(&f, &start).await;
    hot["status"] = json!("cancelling");
    hot["revoked"] = json!(true);
    hot["cancellation_id"] = json!(Uuid::new_v4());
    sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
        .bind(start.binding.attempt_id)
        .bind(hot)
        .execute(&pool)
        .await
        .unwrap();
    stopped::record(&f.state, &auth, &native_started::stop_request(&start))
        .await
        .unwrap();
    let before = native_started::state(&f, &start).await;
    results::record(&f.state, &auth, &report).await.unwrap();
    assert_eq!(native_started::state(&f, &start).await, before);
    assert_eq!(before["revoked"], true);
}

#[tokio::test]
async fn result_http_uses_current_machine_auth_and_bounded_work_admission() {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;
    let (f, _, report) = prepare(true, true).await;
    let app = aq_server::build_worker_router(f.state.clone());
    let request = |token: &str, body: Vec<u8>| {
        Request::builder()
            .method("POST")
            .uri("/v1/worker/profiled/results")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::from(body))
            .unwrap()
    };
    let body = serde_json::to_vec(&report).unwrap();
    assert_ne!(
        app.clone()
            .oneshot(request("foreign-token", body.clone()))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let response = app
        .clone()
        .oneshot(request(&f.credential, body.clone()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let bytes = axum::body::to_bytes(response.into_body(), 65536)
        .await
        .unwrap();
    result_record::decode_receipt(&bytes, &report).unwrap();
    assert_eq!(
        app.clone()
            .oneshot(request(&f.credential, vec![b'x'; 65537]))
            .await
            .unwrap()
            .status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let mut unknown = json!(report);
    unknown["accept_result"] = json!(true);
    assert_ne!(
        app.clone()
            .oneshot(request(
                &f.credential,
                serde_json::to_vec(&unknown).unwrap()
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    sqlx::query("UPDATE nodes SET revoked_at=now() WHERE id=$1")
        .bind(f.node.node_id)
        .execute(&f.db.pool().await)
        .await
        .unwrap();
    assert_ne!(
        app.oneshot(request(&f.credential, body))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        aq_server::admission::classify("/v1/worker/profiled/results"),
        aq_server::admission::Class::General
    );
}

#[tokio::test]
async fn internal_owner_read_is_historical_exact_and_marks_a_superseded_head_without_eligibility() {
    let (mut f, _started, report) = prepare(true, true).await;
    let auth = context(&f).await;
    let original = results::record(&f.state, &auth, &report).await.unwrap();
    let mut correction = report.clone();
    correction.report_id = Uuid::new_v4();
    correction.operation_id = Uuid::new_v4();
    correction.supersedes = Some(report.report_id);
    correction.conclusion = Label::new("retained correction, not eligibility").unwrap();
    results::record(&f.state, &auth, &correction).await.unwrap();
    let pool = f.db.pool().await;
    sqlx::query("UPDATE nodes SET revoked_at=clock_timestamp() WHERE id=$1")
        .bind(f.node.node_id)
        .execute(&pool)
        .await
        .unwrap();
    let mut cfg = (*f.state.config).clone();
    cfg.profiled_execution = None;
    f.state.config = Arc::new(cfg);
    f.time.store((NOW + 1_000_000) as i64, Ordering::SeqCst);
    let old = results::test_load_retained(&f.state, report.report_id)
        .await
        .unwrap();
    assert_eq!(old["is_current_report_head"], false);
    assert_eq!(old["head_report_id"], json!(correction.report_id));
    assert_eq!(old["receipt"], json!(original));
    assert_eq!(
        old["spec"]["task"]["content_revision"],
        old["outcome"]["task"]["content_revision"]
    );
    assert!(old.get("eligible").is_none());
    let current = results::test_load_retained(&f.state, correction.report_id)
        .await
        .unwrap();
    assert_eq!(current["is_current_report_head"], true);
    assert_eq!(current["report"]["binding"], json!(report.binding));
}
#[tokio::test]
async fn internal_owner_read_rejects_a_changed_report_even_with_recomputed_stored_digest() {
    let (f, _, report) = prepare(true, true).await;
    let auth = context(&f).await;
    results::record(&f.state, &auth, &report).await.unwrap();
    let pool = f.db.pool().await;
    // Deliberate operator corruption fixture bypasses immutable UPDATE guard,
    // retaining the CHECK digest so the reader must verify the receipt linkage.
    sqlx::query("ALTER TABLE profiled_result_reports DISABLE TRIGGER USER")
        .execute(&pool)
        .await
        .unwrap();
    let mut changed = report.clone();
    changed.conclusion = Label::new("tampered report body").unwrap();
    let mut value = json!(changed);
    value.sort_all_objects();
    let bytes = serde_json::to_vec(&value).unwrap();
    sqlx::query("UPDATE profiled_result_reports SET report_bytes=$2,report_digest=encode(sha256($2::bytea),'hex') WHERE report_id=$1").bind(report.report_id).bind(bytes).execute(&pool).await.unwrap();
    assert!(
        results::test_load_retained(&f.state, report.report_id)
            .await
            .is_err()
    );
}
#[tokio::test]
async fn internal_owner_read_rejects_receipt_outcome_and_stored_spec_corruption() {
    let (f, _, report) = prepare(true, true).await;
    let auth = context(&f).await;
    let receipt = results::record(&f.state, &auth, &report).await.unwrap();
    let pool = f.db.pool().await;
    sqlx::query("ALTER TABLE profiled_result_reports DISABLE TRIGGER USER")
        .execute(&pool)
        .await
        .unwrap();
    let mut bad = json!(receipt);
    bad["expected_outcome"]["digest"] = json!(Digest::of(b"wrong outcome owner"));
    bad.sort_all_objects();
    sqlx::query("UPDATE profiled_result_reports SET receipt_bytes=$2 WHERE report_id=$1")
        .bind(report.report_id)
        .bind(serde_json::to_vec(&bad).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        results::test_load_retained(&f.state, report.report_id)
            .await
            .is_err()
    );
    let mut good = json!(receipt);
    good.sort_all_objects();
    sqlx::query("UPDATE profiled_result_reports SET receipt_bytes=$2 WHERE report_id=$1")
        .bind(report.report_id)
        .bind(serde_json::to_vec(&good).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE profiled_launches DISABLE TRIGGER USER")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE profiled_launches SET execution_spec=jsonb_set(execution_spec,'{task,content_revision}','9999') WHERE attempt_id=$1").bind(report.binding.attempt_id).execute(&pool).await.unwrap();
    assert!(
        results::test_load_retained(&f.state, report.report_id)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn result_source_owner_requires_a_real_retained_transfer_and_exact_native_binding() {
    let ReviewEnv {
        f,
        report,
        operation,
        transfer_id,
        service,
        prepared,
        peer,
        _directory,
        ..
    } = review_env(2).await;
    let pool = &f.db.pool().await;
    let auth = context(&f).await;
    let request = aq_review_protocol::DecisionRequest {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        decision_id: Uuid::new_v4(),
        subject: prepared.subject().artifact().unwrap(),
        eligible_owners: vec![f.actor],
        eligibility_project_revision: 1,
        review_policy: prepared.subject().validation_policy.clone(),
        expires_at_ms: NOW + 30000,
    };
    service.admit(f.actor, &prepared, &request).await.unwrap();
    service.admit(f.actor, &prepared, &request).await.unwrap();
    let mut changed = request.clone();
    changed.expires_at_ms += 1;
    assert!(service.admit(f.actor, &prepared, &changed).await.is_err());
    let answer = aq_review_protocol::ResolveRequest {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        decision_id: request.decision_id,
        expected_decision_revision: 1,
        choice: aq_review_protocol::Choice::Approve,
        comment: Some("Synthetic component review".into()),
    };
    let mut pending = Vec::new();
    for _ in 0..5 {
        let mut next = request.clone();
        next.operation_id = Uuid::new_v4();
        next.decision_id = Uuid::new_v4();
        service.admit(f.actor, &prepared, &next).await.unwrap();
        pending.push(next);
    }
    let cancellation = Uuid::new_v4();
    let cancelled = service
        .cancel(f.actor, cancellation, pending[0].decision_id, 1)
        .await
        .unwrap();
    assert!(matches!(
        cancelled.finality,
        aq_review_protocol::DecisionFinality::Cancelled { .. }
    ));
    assert_eq!(
        json!(
            service
                .cancel(f.actor, cancellation, pending[0].decision_id, 1)
                .await
                .unwrap()
        ),
        json!(cancelled)
    );
    let mut cancelled_answer = answer.clone();
    cancelled_answer.operation_id = Uuid::new_v4();
    cancelled_answer.decision_id = pending[0].decision_id;
    assert!(service.resolve(f.actor, &cancelled_answer).await.is_err());
    let mut one = answer.clone();
    one.operation_id = Uuid::new_v4();
    one.decision_id = pending[1].decision_id;
    let mut two = one.clone();
    two.operation_id = Uuid::new_v4();
    two.choice = aq_review_protocol::Choice::Reject;
    let competing = tokio::join!(
        service.resolve(f.actor, &one),
        service.resolve(f.actor, &two)
    );
    assert_ne!(competing.0.is_ok(), competing.1.is_ok());
    // An outbox write failure rolls back finality and pending-state CAS; retry
    // uses the same retained answer ID, not a second human decision.
    sqlx::raw_sql("CREATE FUNCTION fail_review_outbox() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'synthetic write failure'; END $$; CREATE TRIGGER fail_review_outbox BEFORE INSERT ON review_owner_outbox FOR EACH ROW EXECUTE FUNCTION fail_review_outbox();").execute(pool).await.unwrap();
    assert!(service.resolve(f.actor, &answer).await.is_err());
    let state: String =
        sqlx::query_scalar("SELECT state FROM human_decision_requests WHERE decision_id=$1")
            .bind(request.decision_id)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(state, "pending");
    let receipts: i64 =
        sqlx::query_scalar("SELECT count(*) FROM human_decision_finality WHERE decision_id=$1")
            .bind(request.decision_id)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(receipts, 0);
    sqlx::raw_sql("DROP TRIGGER fail_review_outbox ON review_owner_outbox; DROP FUNCTION fail_review_outbox();").execute(pool).await.unwrap();
    let finality = service.resolve(f.actor, &answer).await.unwrap();
    assert_eq!(
        json!(service.resolve(f.actor, &answer).await.unwrap()),
        json!(finality)
    );
    // Optional real admin router uses current owner identity and the existing
    // CSRF guard; the default builder does not mount these routes.
    use tower::ServiceExt;
    let app = aq_server::build_admin_router_with_policy_owners(
        f.state.clone(),
        None,
        Some(Arc::new(service.clone())),
    );
    let path = format!("/api/reviews/decisions/{}", request.decision_id);
    let get = axum::http::Request::builder()
        .uri(&path)
        .body(axum::body::Body::empty())
        .unwrap();
    assert_eq!(app.clone().oneshot(get).await.unwrap().status(), 200);
    let get = axum::http::Request::builder()
        .uri(&path)
        .body(axum::body::Body::empty())
        .unwrap();
    assert_eq!(
        aq_server::build_admin_router(f.state.clone())
            .oneshot(get)
            .await
            .unwrap()
            .status(),
        404
    );
    let post = axum::http::Request::builder()
        .method("POST")
        .uri(format!("{path}/resolve"))
        .header("content-type", "application/json")
        .body(axum::body::Body::from(serde_json::to_vec(&answer).unwrap()))
        .unwrap();
    assert_eq!(app.clone().oneshot(post).await.unwrap().status(), 403);
    let mut smuggled = json!(answer);
    smuggled["owner_user_id"] = json!(f.actor);
    let post = axum::http::Request::builder()
        .method("POST")
        .uri(format!("{path}/resolve"))
        .header("content-type", "application/json")
        .header("x-aq-request", "1")
        .body(axum::body::Body::from(
            serde_json::to_vec(&smuggled).unwrap(),
        ))
        .unwrap();
    assert_eq!(app.clone().oneshot(post).await.unwrap().status(), 400);
    let mut changed = answer.clone();
    changed.choice = aq_review_protocol::Choice::Reject;
    assert!(service.resolve(f.actor, &changed).await.is_err());
    let outbox: i64 = sqlx::query_scalar("SELECT count(*) FROM review_owner_outbox")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(outbox, 3);
    // Observe a real role-row lock wait before demotion. Authenticated input
    // from before the wait cannot authorize a later answer after revocation.
    let mut blocked = answer.clone();
    blocked.operation_id = Uuid::new_v4();
    blocked.decision_id = pending[2].decision_id;
    let mut lock = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM users WHERE id=$1 FOR UPDATE")
        .bind(f.actor)
        .execute(&mut *lock)
        .await
        .unwrap();
    let xid: String = sqlx::query_scalar("SELECT txid_current()::text")
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    let clone = service.clone();
    let actor = f.actor;
    let call = tokio::spawn(async move { clone.resolve(actor, &blocked).await });
    wait_review_xid(pool, &xid).await;
    sqlx::query("UPDATE users SET role='viewer' WHERE id=$1")
        .bind(f.actor)
        .execute(&mut *lock)
        .await
        .unwrap();
    lock.commit().await.unwrap();
    assert!(call.await.unwrap().is_err());
    sqlx::query("UPDATE users SET role='owner' WHERE id=$1")
        .bind(f.actor)
        .execute(pool)
        .await
        .unwrap();
    // Expiry is sampled after the lock wait, never at handler admission.
    let mut blocked = answer.clone();
    blocked.operation_id = Uuid::new_v4();
    blocked.decision_id = pending[3].decision_id;
    let mut lock = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM users WHERE id=$1 FOR UPDATE")
        .bind(f.actor)
        .execute(&mut *lock)
        .await
        .unwrap();
    let xid: String = sqlx::query_scalar("SELECT txid_current()::text")
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    let clone = service.clone();
    let call = tokio::spawn(async move { clone.resolve(actor, &blocked).await });
    wait_review_xid(pool, &xid).await;
    f.time.store((NOW + 30001) as i64, Ordering::SeqCst);
    lock.commit().await.unwrap();
    assert!(call.await.unwrap().is_err());
    let expired = service.expire(pending[3].decision_id).await.unwrap();
    assert!(matches!(
        expired.finality,
        aq_review_protocol::DecisionFinality::Expired {}
    ));
    assert_eq!(
        json!(service.expire(pending[3].decision_id).await.unwrap()),
        json!(expired)
    );
    assert!(
        service
            .cancel(f.actor, Uuid::new_v4(), pending[4].decision_id, 1)
            .await
            .is_err()
    );
    // Refresh only through another actual current public-owner read. Hold the
    // budget row until this proof expires; admission samples expiry after wait.
    let fresh = service.prepare(report.report_id, operation).await.unwrap();
    peer.await.unwrap();
    let mut late = request.clone();
    late.operation_id = Uuid::new_v4();
    late.decision_id = Uuid::new_v4();
    late.expires_at_ms = NOW + 90000;
    let mut lock = pool.begin().await.unwrap();
    sqlx::query("SELECT singleton FROM review_owner_budget WHERE singleton FOR UPDATE")
        .execute(&mut *lock)
        .await
        .unwrap();
    let xid: String = sqlx::query_scalar("SELECT txid_current()::text")
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    let expiry = async {
        wait_review_xid(pool, &xid).await;
        f.time.store((NOW + 60002) as i64, Ordering::SeqCst);
        lock.commit().await.unwrap();
    };
    let (refused, _) = tokio::join!(service.admit(f.actor, &fresh, &late), expiry);
    assert!(matches!(
        refused,
        Err(aq_server::error::AppError::UpdatesUnavailable(_))
    ));
    service.admit(f.actor, &prepared, &request).await.unwrap(); // historical only
    let mut correction = report.clone();
    correction.report_id = Uuid::new_v4();
    correction.operation_id = Uuid::new_v4();
    correction.supersedes = Some(report.report_id);
    let race = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(
            results::record(&f.state, &auth, &correction),
            service.resolve(f.actor, &answer)
        )
    })
    .await
    .expect("replay/correction must not deadlock");
    race.0.unwrap();
    assert_eq!(json!(race.1.unwrap()), json!(finality));
    sqlx::query("UPDATE source_transfers SET central_receipt=jsonb_set(central_receipt,'{source,hex}',to_jsonb($2::text)) WHERE transfer_id=$1")
        .bind(transfer_id).bind("c".repeat(40)).execute(pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    let actual = results::load_locked(&f.state, &mut tx, report.report_id)
        .await
        .unwrap();
    assert!(
        aq_server::source_retention::load_confirmed_locked(&mut tx, operation, &actual)
            .await
            .is_err()
    );
    tx.rollback().await.unwrap();
}

async fn wait_review_xid(pool: &sqlx::PgPool, xid: &str) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
    loop {
        let waiting: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM pg_locks WHERE transactionid::text=$1 AND NOT granted)",
        )
        .bind(xid)
        .fetch_one(pool)
        .await
        .unwrap();
        if waiting {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "review did not reach expected row lock"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

struct ReviewEnv {
    f: Fixture,
    report: ResultReport,
    operation: Uuid,
    transfer_id: Uuid,
    service: aq_server::reviews::Service,
    prepared: aq_server::reviews::PreparedReview,
    keys: Arc<aq_server::protected_storage::Keyring>,
    _directory: tempfile::TempDir,
    peer: tokio::task::JoinHandle<()>,
}
async fn review_env(peer_count: usize) -> ReviewEnv {
    let (mut f, _, mut report) = prepare(true, true).await;
    let operation = Uuid::new_v4();
    report.source_receipt_ids.push(operation);
    let auth = context(&f).await;
    results::record(&f.state, &auth, &report).await.unwrap();
    let pool = &f.db.pool().await;
    let mut tx = pool.begin().await.unwrap();
    let actual = results::load_locked(&f.state, &mut tx, report.report_id)
        .await
        .unwrap();
    assert!(
        aq_server::source_retention::load_confirmed_locked(&mut tx, operation, &actual)
            .await
            .is_err()
    );
    tx.rollback().await.unwrap();
    // Synthetic SQL owner row tests validation only; this does not qualify a
    // native Git transfer or fabricate public Git retention evidence.
    let transfer_id = Uuid::new_v4();
    let artifact_id = Uuid::new_v4();
    let mut manifest = aq_worker_protocol::TransferManifest {
        transfer_id,
        role: aq_worker_protocol::SourceRole::Checkpoint,
        checkpoint: f.profiled.source.base.clone(),
        attempt: aq_worker_protocol::AttemptRef {
            attempt_id: report.binding.attempt_id,
            session_id: match report.binding.session {
                aq_session_protocol::SessionIdentity::Remote { session_id, .. } => session_id,
                _ => unreachable!(),
            },
            generation: report.binding.assignment_generation,
        },
        resolved_spec_digest: report.binding.execution_spec_digest.as_str().into(),
        source: f.profiled.source.clone(),
        artifact_id,
        manifest_digest: "a".repeat(64),
        bundle_digest: None,
        manifest_bytes: 1,
        bundle_bytes: 0,
    };
    // An otherwise valid legacy/fake session cannot be coerced into the native
    // result's exact session identity simply by copying its receipt UUID.
    manifest.attempt.session_id = Uuid::new_v4();
    let receipt = aq_worker_protocol::CentralRetentionReceipt {
        receipt_id: operation,
        transfer_id,
        repository_id: manifest.source.repository_id,
        binding_generation: manifest.source.binding_generation,
        source: manifest.checkpoint.clone(),
        retention_handle: artifact_id,
        manifest_digest: "b".repeat(64),
    };
    let raw = json!({"repository_id":receipt.repository_id,"binding_generation":receipt.binding_generation,"transfer_id":artifact_id,"retention_handle":artifact_id,"manifest_digest":receipt.manifest_digest,"source":receipt.source,"retention_state":"active","historical":false,"receiver_installation_id":Uuid::new_v4()});
    sqlx::query("INSERT INTO source_transfers(transfer_id,attempt_id,node_id,repository_id,artifact_id,import_operation_id,manifest,state,receipt,central_receipt) VALUES($1,$2,$3,$4,$5,$6,$7,'retained',$8,$9)")
        .bind(transfer_id).bind(manifest.attempt.attempt_id).bind(f.node.node_id)
        .bind(manifest.source.repository_id).bind(artifact_id).bind(operation)
        .bind(json!(manifest)).bind(json!(receipt)).bind(raw).execute(pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    let actual = results::load_locked(&f.state, &mut tx, report.report_id)
        .await
        .unwrap();
    assert!(
        aq_server::source_retention::load_confirmed_locked(&mut tx, operation, &actual)
            .await
            .is_err()
    );
    tx.rollback().await.unwrap();
    manifest.attempt.session_id = match report.binding.session {
        aq_session_protocol::SessionIdentity::Remote { session_id, .. } => session_id,
        _ => unreachable!(),
    };
    sqlx::query("UPDATE source_transfers SET manifest=$2 WHERE transfer_id=$1")
        .bind(transfer_id)
        .bind(json!(manifest))
        .execute(pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    let actual = results::load_locked(&f.state, &mut tx, report.report_id)
        .await
        .unwrap();
    let historical =
        aq_server::source_retention::load_confirmed_locked(&mut tx, operation, &actual)
            .await
            .unwrap();
    assert_eq!(historical.receipt.receipt_id, operation);
    tx.rollback().await.unwrap();
    // Candidate026 component only; ordinary state24 remains unchanged. This
    // fixture uses a synthetic public Git peer, not real Git qualification.
    let installed: bool =
        sqlx::query_scalar("SELECT to_regclass('public.review_owner_budget') IS NOT NULL")
            .fetch_one(pool)
            .await
            .unwrap();
    if aq_server::STATE_FORMAT_VERSION >= 26 {
        assert!(installed, "ordered026 must install review owner tables");
    } else {
        assert!(
            !installed,
            "unexpected candidate026 table before explicit install"
        );
        let before: String =
            sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
                .fetch_one(pool)
                .await
                .unwrap();
        let schema = include_str!("../migrations/0026_review_owners.sql").replace(
            "UPDATE meta SET value='26' WHERE key='state_format_version';",
            "",
        );
        sqlx::raw_sql(&schema).execute(pool).await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, String>(
                "SELECT value FROM meta WHERE key='state_format_version'"
            )
            .fetch_one(pool)
            .await
            .unwrap(),
            before
        );
    }
    for table in [
        "review_owner_budget",
        "review_lineage_budget",
        "review_subjects",
        "human_decision_requests",
        "human_decision_finality",
        "review_owner_outbox",
    ] {
        let present: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
            .bind(format!("public.{table}"))
            .fetch_one(pool)
            .await
            .unwrap();
        assert!(present, "required026 table {table}");
    }
    let column:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM information_schema.columns WHERE table_schema='public' AND table_name='playbook_namespaces' AND column_name='catalog_profile')").fetch_one(pool).await.unwrap();
    assert!(column, "required026 catalog_profile missing");
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("git.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let inspection = json!({"version":1,"id":artifact_id,"repository_id":manifest.source.repository_id,"binding_generation":manifest.source.binding_generation,"generation":1,"state":"active","resource":"transfer","release_operation":null,"release_reason":null,"refs":[{"name":format!("refs/aq-git/transfers/{artifact_id}"),"oid":receipt.source.hex}]});
    let peer = tokio::spawn(async move {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        for _ in 0..peer_count {
            let (socket, _) = listener.accept().await.unwrap();
            let mut reader = tokio::io::BufReader::new(socket);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let request: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["operation"], "retention.inspect");
            assert_eq!(request["params"]["handle_id"], artifact_id.to_string());
            let reply = json!({"protocol_version":1,"request_id":request["request_id"],"ok":true,"result":inspection});
            let mut bytes = serde_json::to_vec(&reply).unwrap();
            bytes.push(b'\n');
            reader.get_mut().write_all(&bytes).await.unwrap();
        }
    });
    f.state.source_retention = Some(Arc::new(
        aq_server::source_retention::Runtime::new(
            aq_server::source_retention::SourceRetentionConfig {
                staging_root: directory.path().join("staging"),
                git_root: directory.path().join("git"),
                git_socket: socket,
                max_staging_bytes: 1048576,
                max_bundle_bytes: 1048576,
                bulk_concurrency: 1,
                incomplete_ttl_secs: 60,
            },
        )
        .unwrap(),
    ));
    let key = directory.path().join("key");
    std::fs::write(&key, [9_u8; 32]).unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    let keys = aq_server::protected_storage::Keyring::load(
        &aq_server::protected_storage::ProtectionConfig {
            active_key_id: "test".into(),
            keys: vec![aq_server::protected_storage::KeyFile {
                id: "test".into(),
                file: key,
            }],
        },
    )
    .unwrap();
    let keys = Arc::new(keys);
    let service = aq_server::reviews::Service::new(f.state.clone(), keys.clone());
    let prepared = service.prepare(report.report_id, operation).await.unwrap();
    ReviewEnv {
        f,
        report,
        operation,
        transfer_id,
        service,
        prepared,
        keys,
        _directory: directory,
        peer,
    }
}

fn review_policy_artifact(input: serde_json::Value) -> aq_playbook_host::model::Artifact {
    use aq_playbook_host::model::*;
    use std::collections::{BTreeMap, BTreeSet};
    let command = aq_server::playbooks::review_domain::decision_contract();
    let header = SourceHeader {
        format_version: 1,
        id: "human-review".into(),
        scope: "project".into(),
        interfaces: BTreeSet::from(["main".into()]),
        definitions: BTreeSet::new(),
    };
    let ret = |outcome: &str| Step {
        op: Operation::Return {
            outcome: outcome.into(),
            output: Expr::literal(json!(null)),
        },
        edges: BTreeMap::new(),
        on_error: None,
    };
    let mut edges = BTreeMap::from([
        ("accepted".into(), "wait".into()),
        ("approved".into(), "approved".into()),
    ]);
    for outcome in ["rejected", "expired", "cancelled"] {
        edges.insert(outcome.into(), "rejected".into());
    }
    let playbook = Playbook {
        id: header.id.clone(),
        purpose: "synthetic protected-engine human-owner integration, no settlement".into(),
        scope: "project".into(),
        entries: BTreeMap::from([(
            "main".into(),
            EntryPoint {
                input: Schema::Null,
                outcomes: BTreeMap::from([
                    ("approved".into(), Schema::Null),
                    ("rejected".into(), Schema::Null),
                ]),
                start: "ask".into(),
            },
        )]),
        steps: BTreeMap::from([
            (
                "ask".into(),
                Step {
                    op: Operation::Invoke {
                        command: ContractRef {
                            name: command.name.clone(),
                            fingerprint: command.fingerprint().unwrap(),
                        },
                        input: Expr::literal(input),
                        bind: "question".into(),
                        retry: None,
                    },
                    edges,
                    on_error: Some("rejected".into()),
                },
            ),
            (
                "wait".into(),
                Step {
                    op: Operation::Join {
                        member_keys: vec![],
                        members: vec![Expr::reference("question")],
                        mode: JoinMode::All,
                        accepted_outcomes: BTreeSet::from(["approved".into()]),
                        deadline_ms: 20000,
                        remaining: RemainingPolicy::Cancel,
                        bind: "answer".into(),
                    },
                    edges: BTreeMap::from([
                        ("completed".into(), "approved".into()),
                        ("no_match".into(), "rejected".into()),
                        ("timeout".into(), "rejected".into()),
                    ]),
                    on_error: Some("rejected".into()),
                },
            ),
            ("approved".into(), ret("approved")),
            ("rejected".into(), ret("rejected")),
        ]),
        triggers: vec![],
        limits: Limits::default(),
        required_capabilities: BTreeSet::from(["decision.request".into()]),
    };
    aq_playbooks_compiler::compile(&aq_playbooks_compiler::CompileInput {
        source: format!(
            "---aq-playbook\n{}\n---\nSynthetic human-owner integration.\n",
            serde_json::to_string(&header).unwrap()
        ),
        proposal: aq_playbooks_compiler::Proposal {
            playbook,
            source_map: BTreeMap::new(),
            questions: vec![],
            unsupported_features: vec![],
        },
        registry: aq_server::playbooks::review_domain::CatalogProfile::HumanReview2.registry(),
        dependencies: BTreeMap::new(),
        definitions: BTreeMap::new(),
        compiler: aq_playbooks_compiler::COMPILER_VERSION.into(),
    })
    .expect("real compiler must validate the exact async owner catalog")
}
#[tokio::test]
async fn protected_engine_question_waits_for_real_owner_and_exact_completion_event() {
    use aq_server::playbooks::{
        domain::{GrantScope, Recipient},
        driver::Driver,
        service::{Namespace, PrepareRequest, Service},
    };
    use std::collections::BTreeSet;
    let ReviewEnv {
        f,
        report,
        operation,
        prepared,
        keys,
        peer,
        _directory,
        ..
    } = review_env(3).await;
    let pool = f.db.pool().await;
    // New runtime deliberately refuses old source-origin namespaces without
    // rewriting them; no in-place/mixed-version upgrade is claimed.
    let legacy = Namespace {
        namespace_id: Uuid::new_v4(),
        generation: 1,
        name: "legacy-mail-v11".into(),
        workspace_id: f.selection.lineage.workspace_id,
        project_id: f.selection.lineage.project_id,
        owner_user_id: f.actor,
    };
    let legacy_source = "48f19b80fc0734c403ef04e7181d570b9fe675597f35caf08f0b94571ca8cc70";
    sqlx::query("INSERT INTO playbook_namespaces(namespace_id,generation,name,workspace_id,project_id,owner_user_id,catalog_digest,source_digest) VALUES($1,1,$2,$3,$4,$5,$6,$7)").bind(legacy.namespace_id).bind(&legacy.name).bind(legacy.workspace_id).bind(legacy.project_id).bind(f.actor).bind(aq_server::playbooks::domain::digest(&aq_server::playbooks::domain::registry()).unwrap()).bind(legacy_source).execute(&pool).await.unwrap();
    let refused = Service::new(&f.state, legacy.clone(), keys.clone())
        .await
        .unwrap();
    assert!(refused.initialize(f.actor).await.is_err());
    assert!(refused.runtime_revision().await.is_err());
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT source_digest FROM playbook_namespaces WHERE namespace_id=$1"
        )
        .bind(legacy.namespace_id)
        .fetch_one(&pool)
        .await
        .unwrap(),
        legacy_source
    );
    let namespace = Namespace {
        namespace_id: Uuid::new_v4(),
        generation: 1,
        name: "human-v2".into(),
        workspace_id: f.selection.lineage.workspace_id,
        project_id: f.selection.lineage.project_id,
        owner_user_id: f.actor,
    };
    let service = Service::new_profile2(&f.state, namespace.clone(), keys)
        .await
        .unwrap();
    service.initialize(f.actor).await.unwrap();
    // Reusing the same namespace with the old catalog refuses rather than
    // restamping catalog bytes or broadening the original owner's grant.
    let directory = tempfile::tempdir().unwrap();
    let key = directory.path().join("other-key");
    std::fs::write(&key, [9u8; 32]).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    let old_keys = aq_server::protected_storage::Keyring::load(
        &aq_server::protected_storage::ProtectionConfig {
            active_key_id: "test".into(),
            keys: vec![aq_server::protected_storage::KeyFile {
                id: "test".into(),
                file: key,
            }],
        },
    )
    .unwrap();
    assert!(
        Service::new(&f.state, namespace, Arc::new(old_keys))
            .await
            .unwrap()
            .initialize(f.actor)
            .await
            .is_err()
    );
    let recipient = Recipient::Task {
        workspace_id: f.selection.lineage.workspace_id,
        project_id: f.selection.lineage.project_id,
        task_id: f.selection.lineage.task_id,
    };
    let grant = Uuid::new_v4();
    service
        .grant(
            f.actor,
            grant,
            GrantScope {
                workspace_id: f.selection.lineage.workspace_id,
                project_id: f.selection.lineage.project_id,
                recipients: vec![recipient.clone()],
                capabilities: BTreeSet::from(["decision.request".into(), "operation.get".into()]),
            },
            (NOW + 60000) as i64,
        )
        .await
        .unwrap();
    let mut url = reqwest::Url::parse(&std::env::var("DATABASE_URL").unwrap()).unwrap();
    assert_eq!(url.host_str(), Some("127.0.0.1"));
    assert_eq!(url.port(), Some(55432));
    url.set_path(f.db.options().get_database().unwrap());
    let url = url.to_string();
    let driver = Driver::start(
        service.clone(),
        aq_playbook_host::Limits::default(),
        review_engine_factory(url.clone()),
    )
    .await
    .unwrap();
    let input = aq_server::playbooks::review_domain::DecisionInput {
        recipient,
        report_id: report.report_id,
        source_operation_id: operation,
        expected_subject: prepared.subject().artifact().unwrap(),
        eligible_owners: vec![f.actor],
        eligibility_project_revision: 1,
        expires_at_ms: NOW + 30000,
    };
    let artifact = review_policy_artifact(json!(input));
    let hash = artifact.digest.clone();
    driver.install(artifact).await.unwrap();
    let binding = driver
        .activate("human".into(), hash, grant, 1)
        .await
        .unwrap();
    let request = PrepareRequest {
        request_id: Uuid::new_v4(),
        grant_id: grant,
        grant_revision: 1,
        binding: "human".into(),
        expected_activation_revision: binding["revision"].as_u64().unwrap(),
        entry: "main".into(),
        input: json!(null),
    };
    service.prepare(f.actor, request.clone()).await.unwrap();
    let admitted = driver
        .admission(f.actor, request.request_id, false)
        .await
        .unwrap();
    let run = admitted.receipt.unwrap().run_id;
    let mut second = request.clone();
    second.request_id = Uuid::new_v4();
    service.prepare(f.actor, second.clone()).await.unwrap();
    let second_run = driver
        .admission(f.actor, second.request_id, false)
        .await
        .unwrap()
        .receipt
        .unwrap()
        .run_id;
    driver.test_lose_next_decision_reply();
    driver.quantum().await.unwrap();
    let unknown = driver.inspect_run(run.clone()).await.unwrap();
    assert_eq!(unknown["items"][0]["status"], "blocked");
    driver.recover().await.unwrap();
    driver.quantum().await.unwrap();
    let pending = driver.inspect_run(run.clone()).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), peer)
        .await
        .unwrap_or_else(|e| panic!("source peer not called: {e}; run={pending}"))
        .unwrap();
    assert_eq!(pending["items"][0]["status"], "waiting");
    assert_ne!(pending["items"][0]["outcome"], "approved");
    driver.cancel_run(second_run.clone()).await.unwrap();
    driver.quantum().await.unwrap();
    let decision: Uuid = sqlx::query_scalar(
        "SELECT (receipt->>'handle')::uuid FROM playbook_effects WHERE run_id=$1",
    )
    .bind(&run)
    .fetch_one(&pool)
    .await
    .unwrap();
    let cancelled: Uuid = sqlx::query_scalar(
        "SELECT (receipt->>'handle')::uuid FROM playbook_effects WHERE run_id=$1",
    )
    .bind(&second_run)
    .fetch_one(&pool)
    .await
    .unwrap();
    let state: String =
        sqlx::query_scalar("SELECT state FROM human_decision_requests WHERE decision_id=$1")
            .bind(cancelled)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state, "cancelled");
    let answer = aq_review_protocol::ResolveRequest {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        decision_id: decision,
        expected_decision_revision: 1,
        choice: aq_review_protocol::Choice::Approve,
        comment: Some("Synthetic actual owner API resolution".into()),
    };
    let receipt = service
        .human_owner()
        .unwrap()
        .resolve(f.actor, &answer)
        .await
        .unwrap();
    assert!(matches!(
        receipt.finality,
        aq_review_protocol::DecisionFinality::HumanAnswer { .. }
    ));
    // Revoke before delivery: retained completion still reconciles, but cannot
    // grant new business authority or a second question.
    service.revoke(f.actor, grant).await.unwrap();
    // Cancellation was copied by its earlier quantum; only the newly answered
    // question remains for this exact copy/engine-ACK loss boundary.
    assert_eq!(driver.test_collect_decision_events().await.unwrap(), 1);
    let event_first = driver.test_commit_event_without_aq_ack().await.unwrap();
    assert!(!event_first.duplicate);
    let event_replay = driver.test_commit_event_without_aq_ack().await.unwrap();
    assert!(event_replay.duplicate);
    assert_eq!(event_first.key, event_replay.key);
    for _ in 0..4 {
        driver.quantum().await.unwrap();
    }
    let finished = driver.inspect_run(run).await.unwrap();
    assert_eq!(finished["items"][0]["outcome"], "approved");
    let questions: i64 = sqlx::query_scalar("SELECT count(*) FROM human_decision_requests")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(questions, 2);
    let copied: bool =
        sqlx::query_scalar("SELECT acknowledged FROM review_owner_outbox WHERE decision_id=$1")
            .bind(decision)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(copied);
    let event: serde_json::Value =
        sqlx::query_scalar("SELECT engine_receipt FROM playbook_outbox WHERE event_id=$1")
            .bind(decision)
            .fetch_one(&pool)
            .await
            .unwrap();
    // Existing asynchronous wait completion is separate from trigger fanout;
    // no new root admission was requested by this completion event.
    assert_eq!(event["destinations"], 0);
    let cancelled_run = driver.inspect_run(second_run.clone()).await.unwrap();
    assert_eq!(cancelled_run["items"][0]["status"], "cancelled");
    driver.stop().await.unwrap();
    let restarted = Driver::start(
        service.clone(),
        aq_playbook_host::Limits::default(),
        review_engine_factory(url),
    )
    .await
    .unwrap();
    restarted.recover().await.unwrap();
    restarted.quantum().await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM human_decision_requests")
            .fetch_one(&pool)
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        restarted.inspect_run(second_run).await.unwrap()["items"][0]["status"],
        "cancelled"
    );
    restarted.stop().await.unwrap();
}

fn review_engine_factory(
    url: String,
) -> impl FnOnce() -> std::result::Result<
    aq_playbook_host::postgres_storage::PostgresStorage,
    aq_playbook_host::Error,
> + Send
+ 'static {
    move || {
        let mut config: aq_playbook_host::postgres_client::Config =
            url.parse().map_err(|_| aq_playbook_host::Error::Invalid)?;
        config.connect_timeout(std::time::Duration::from_secs(2));
        let client = config
            .connect(aq_playbook_host::postgres_client::NoTls)
            .map_err(|_| aq_playbook_host::Error::Unknown)?;
        let keys = aq_playbook_host::postgres_storage::ProtectionKeyring::new(
            "fixture",
            std::collections::BTreeMap::from([("fixture".into(), [11u8; 32])]),
        )
        .unwrap();
        aq_playbook_host::protected::storage(
            client,
            keys,
            std::time::Duration::from_secs(2),
            std::time::Duration::from_millis(100),
        )
    }
}

#[tokio::test]
async fn bounded_owner_quantum_expires_its_accepted_question_and_delivers_exact_finality() {
    use aq_server::playbooks::{
        domain::{GrantScope, Recipient},
        driver::Driver,
        service::{Namespace, PrepareRequest, Service},
    };
    use std::collections::BTreeSet;
    let ReviewEnv {
        f,
        report,
        operation,
        prepared,
        keys,
        peer,
        _directory,
        ..
    } = review_env(2).await;
    let pool = f.db.pool().await;
    let namespace = Namespace {
        namespace_id: Uuid::new_v4(),
        generation: 1,
        name: "expiry-v2".into(),
        workspace_id: f.selection.lineage.workspace_id,
        project_id: f.selection.lineage.project_id,
        owner_user_id: f.actor,
    };
    let service = Service::new_profile2(&f.state, namespace, keys)
        .await
        .unwrap();
    service.initialize(f.actor).await.unwrap();
    let recipient = Recipient::Task {
        workspace_id: f.selection.lineage.workspace_id,
        project_id: f.selection.lineage.project_id,
        task_id: f.selection.lineage.task_id,
    };
    let grant = Uuid::new_v4();
    service
        .grant(
            f.actor,
            grant,
            GrantScope {
                workspace_id: f.selection.lineage.workspace_id,
                project_id: f.selection.lineage.project_id,
                recipients: vec![recipient.clone()],
                capabilities: BTreeSet::from(["decision.request".into()]),
            },
            (NOW + 60000) as i64,
        )
        .await
        .unwrap();
    let mut url = reqwest::Url::parse(&std::env::var("DATABASE_URL").unwrap()).unwrap();
    assert_eq!(url.host_str(), Some("127.0.0.1"));
    assert_eq!(url.port(), Some(55432));
    url.set_path(f.db.options().get_database().unwrap());
    let driver = Driver::start(
        service.clone(),
        aq_playbook_host::Limits::default(),
        review_engine_factory(url.to_string()),
    )
    .await
    .unwrap();
    let input = aq_server::playbooks::review_domain::DecisionInput {
        recipient,
        report_id: report.report_id,
        source_operation_id: operation,
        expected_subject: prepared.subject().artifact().unwrap(),
        eligible_owners: vec![f.actor],
        eligibility_project_revision: 1,
        expires_at_ms: NOW + 100,
    };
    let artifact = review_policy_artifact(json!(input));
    let digest = artifact.digest.clone();
    driver.install(artifact).await.unwrap();
    let binding = driver
        .activate("expiry".into(), digest, grant, 1)
        .await
        .unwrap();
    let request = PrepareRequest {
        request_id: Uuid::new_v4(),
        grant_id: grant,
        grant_revision: 1,
        binding: "expiry".into(),
        expected_activation_revision: binding["revision"].as_u64().unwrap(),
        entry: "main".into(),
        input: serde_json::Value::Null,
    };
    service.prepare(f.actor, request.clone()).await.unwrap();
    let admitted = driver
        .admission(f.actor, request.request_id, false)
        .await
        .unwrap();
    let run = admitted.receipt.unwrap().run_id;
    driver.quantum().await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), peer)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        driver.inspect_run(run.clone()).await.unwrap()["items"][0]["status"],
        "waiting"
    );
    f.time.store((NOW + 100) as i64, Ordering::SeqCst);
    for _ in 0..3 {
        driver.quantum().await.unwrap();
    }
    let decision_state: String = sqlx::query_scalar("SELECT state FROM human_decision_requests")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(decision_state, "expired");
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM human_decision_finality")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM review_owner_outbox")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        driver.inspect_run(run).await.unwrap()["items"][0]["outcome"],
        "rejected"
    );
    driver.stop().await.unwrap();
}

#[path = "profiled_results/review_inbox.rs"]
mod review_inbox;
#[path = "profiled_results/human_results.rs"]
mod human_results;
#[path = "profiled_results/media_owner.rs"]
mod media_owner;
