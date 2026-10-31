//! Disposable actual SQL owners; synthetic launch/host fixture, no provider run.
//! Until022/023 integrate,024 is applied explicitly only to this test database.
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
    sqlx::raw_sql(include_str!("../migrations/0024_profiled_results.sql"))
        .execute(&pool)
        .await
        .unwrap();
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
