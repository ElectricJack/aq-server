//! D3 automatic default-policy settlement with a SYNTHETIC Graph peer and a
//! synthetic retained-source owner row. Check and delivery evidence rows are
//! the server-owned D6 and D1a/D1b seams, written here as those owners would.
use super::*;
use aq_server::profiled_execution::settlement::{self, AutoDisposition, AutoSettlement};
use axum::{Json, Router, routing::post};
use serde_json::Value;
use std::sync::{Mutex, atomic::AtomicUsize};

fn default_policy(delivery: &str) -> aq_server::profiled_execution::definitions::Definition {
    let body = format!(
        r#"{{"schema_version":1,"kind":"aq.default-completion.v1","delivery":"{delivery}"}}"#
    );
    aq_server::profiled_execution::definitions::Definition::new(
        artifact("default-completion-policy", body.as_bytes()),
        ArtifactKind::Definition,
        body,
    )
    .unwrap()
}

struct AutoEnv {
    f: Fixture,
    report: ResultReport,
    source: Uuid,
    checkpoint: String,
    check_id: String,
    validator: String,
    expected_version: u64,
    writes: Arc<AtomicUsize>,
    bodies: Arc<Mutex<Vec<Value>>>,
    _directory: tempfile::TempDir,
}

async fn auto_env(delivery: &str, graph_status: axum::http::StatusCode) -> AutoEnv {
    let (mut f, _, mut report) = Box::pin(prepare_with_policy(
        true,
        true,
        None,
        false,
        Some(default_policy(delivery)),
    ))
    .await;
    let source = Uuid::new_v4();
    report.source_receipt_ids.push(source);
    let auth = context(&f).await;
    results::record(&f.state, &auth, &report).await.unwrap();
    // The record hook evaluated already: nothing is retained yet, so it holds.
    let first = settlement_row(&f, report.report_id).await;
    assert_eq!(first.0, "held");
    assert_eq!(first.1.as_deref(), Some("source_not_retained"));

    // Synthetic retained-source owner row (validation only, not real Git).
    let pool = &f.db.pool().await;
    let transfer_id = Uuid::new_v4();
    let artifact_id = Uuid::new_v4();
    let manifest = aq_worker_protocol::TransferManifest {
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
    let receipt = aq_worker_protocol::CentralRetentionReceipt {
        receipt_id: source,
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
        .bind(manifest.source.repository_id).bind(artifact_id).bind(source)
        .bind(json!(manifest)).bind(json!(receipt)).bind(raw).execute(pool).await.unwrap();

    let mut tx = pool.begin().await.unwrap();
    let recorded = results::load_locked(&f.state, &mut tx, report.report_id)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    let check = recorded.outcome.required_checks.first().unwrap();
    let check_id = check.id.as_str().to_owned();
    let validator = check.validator.digest.as_str().to_owned();
    let expected_version = recorded.spec.task.version;

    // Synthetic Graph peer: counts mutation writes and keeps each body.
    let graph = settlement_owner39::graph_context(&f);
    assert_eq!(graph["node"]["version"], json!(expected_version));
    let writes = Arc::new(AtomicUsize::new(0));
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let (wc, bc) = (writes.clone(), bodies.clone());
    let app = Router::new().route(
        "/v1/workspaces/{workspace}/mutations",
        post(move |Json(body): Json<Value>| {
            let (g, wc, bc) = (graph.clone(), wc.clone(), bc.clone());
            async move {
                wc.fetch_add(1, Ordering::SeqCst);
                bc.lock().unwrap().push(body);
                let mut node = g["node"].clone();
                node["task_state"] = json!("completed");
                node["version"] = json!(node["version"].as_u64().unwrap() + 1);
                node["content_revision"] = json!(node["content_revision"].as_u64().unwrap() + 1);
                let reply = if graph_status.is_success() {
                    json!({"change_set_id":"00000000-0000-0000-0000-000000000052","results":[node]})
                } else {
                    json!({"error":"version_conflict"})
                };
                (graph_status, [("X-AQ-Graph-API-Version", "1")], Json(reply))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph-key");
    std::fs::write(&path, "x".repeat(32)).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    f.state.agent_access = Some(Arc::new(
        aq_server::agent_access::Runtime::new(&aq_server::agent_access::AgentAccessConfig {
            graph_url: format!("http://{address}/"),
            graph_token_file: path,
            mutation_actor_id: Some("synthetic-settlement-service".into()),
            credential_ttl_ms: 120000,
        })
        .await
        .unwrap(),
    ));
    AutoEnv {
        checkpoint: f.profiled.source.base.hex.clone(),
        f,
        report,
        source,
        check_id,
        validator,
        expected_version,
        writes,
        bodies,
        _directory: directory,
    }
}

async fn settlement_row(f: &Fixture, report: Uuid) -> (String, Option<String>) {
    let row: (String, Option<String>) = sqlx::query_as(
        "SELECT disposition,reason FROM profiled_auto_settlements WHERE report_id=$1",
    )
    .bind(report)
    .fetch_one(&f.db.pool().await)
    .await
    .unwrap();
    row
}

impl AutoEnv {
    async fn settle(&self) -> AutoSettlement {
        settlement::settle_after_record(&self.f.state, self.report.report_id)
            .await
            .unwrap()
            .expect("default policy applies")
    }
    async fn check(&self, source: Uuid, checkpoint: &str, validator: &str, verdict: &str) {
        sqlx::query("INSERT INTO profiled_check_evidence(attempt_id,check_id,source_operation_id,checkpoint,validator_digest,verdict,evidence,recorded_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT(attempt_id,check_id,source_operation_id) DO UPDATE SET checkpoint=EXCLUDED.checkpoint,validator_digest=EXCLUDED.validator_digest,verdict=EXCLUDED.verdict,evidence=EXCLUDED.evidence")
            .bind(self.report.binding.attempt_id).bind(&self.check_id).bind(source)
            .bind(checkpoint).bind(validator).bind(verdict)
            .bind(json!({"runner":"synthetic-server-check-owner"})).bind(NOW as i64)
            .execute(&self.f.db.pool().await).await.unwrap();
    }
    async fn delivery(&self, state: &str) {
        sqlx::query("INSERT INTO profiled_delivery_evidence(attempt_id,source_operation_id,state,evidence,recorded_at_ms) VALUES($1,$2,$3,$4,$5) ON CONFLICT(attempt_id,source_operation_id) DO UPDATE SET state=EXCLUDED.state")
            .bind(self.report.binding.attempt_id).bind(self.source).bind(state)
            .bind(json!({"publisher":"synthetic-delivery-owner"})).bind(NOW as i64)
            .execute(&self.f.db.pool().await).await.unwrap();
    }
    async fn holds_task(&self) -> bool {
        sqlx::query_scalar("SELECT holds_task FROM execution_attempts WHERE attempt_id=$1")
            .bind(self.report.binding.attempt_id)
            .fetch_one(&self.f.db.pool().await)
            .await
            .unwrap()
    }
    async fn assert_not_completed(
        &self,
        s: &AutoSettlement,
        disposition: AutoDisposition,
        reason: &str,
    ) {
        assert_eq!(s.disposition, disposition, "{s:?}");
        assert_eq!(s.reason.as_deref(), Some(reason), "{s:?}");
        assert!(s.operation_id.is_none());
        assert_eq!(self.writes.load(Ordering::SeqCst), 0, "no Graph completion");
        assert!(self.holds_task().await, "dependents stay blocked");
    }
}

#[tokio::test]
async fn false_pass_result_does_not_complete_task() {
    let env = auto_env("not_required", axum::http::StatusCode::OK).await;
    // The worker claims success; with no server-owned evidence it only holds.
    let s = env.settle().await;
    env.assert_not_completed(&s, AutoDisposition::Held, "required_check_evidence_missing")
        .await;
    // Passing evidence for another source, checkpoint or validator never counts.
    let validator = env.validator.clone();
    let checkpoint = env.checkpoint.clone();
    env.check(Uuid::new_v4(), &checkpoint, &validator, "passed")
        .await;
    env.check(env.source, &"1".repeat(40), &validator, "passed")
        .await;
    let s = env.settle().await;
    env.assert_not_completed(&s, AutoDisposition::Held, "required_check_evidence_missing")
        .await;
    env.check(env.source, &checkpoint, &"c".repeat(64), "passed")
        .await;
    let s = env.settle().await;
    env.assert_not_completed(&s, AutoDisposition::Held, "required_check_evidence_missing")
        .await;
    // Exactly bound evidence that FAILED refuses the result for rework.
    env.check(env.source, &checkpoint, &validator, "failed")
        .await;
    let s = env.settle().await;
    env.assert_not_completed(&s, AutoDisposition::ReworkRequired, "required_check_failed")
        .await;
    assert_eq!(s.evidence["checks"][0]["verdict"], "failed", "{s:?}");
    // Rework is terminal for this report: later evidence cannot flip it.
    env.check(env.source, &checkpoint, &validator, "passed")
        .await;
    let s = env.settle().await;
    env.assert_not_completed(&s, AutoDisposition::ReworkRequired, "required_check_failed")
        .await;
}

#[tokio::test]
async fn false_pass_result_does_not_complete_task_without_published_delivery() {
    let env = auto_env("required", axum::http::StatusCode::OK).await;
    let (checkpoint, validator) = (env.checkpoint.clone(), env.validator.clone());
    env.check(env.source, &checkpoint, &validator, "passed")
        .await;
    let s = env.settle().await;
    env.assert_not_completed(&s, AutoDisposition::Held, "delivery_evidence_missing")
        .await;
    env.delivery("indeterminate").await;
    let s = env.settle().await;
    env.assert_not_completed(&s, AutoDisposition::Held, "delivery_indeterminate")
        .await;
    // Evidence that required delivery did not publish refuses the result.
    env.delivery("unpublished").await;
    let s = env.settle().await;
    env.assert_not_completed(&s, AutoDisposition::ReworkRequired, "delivery_unpublished")
        .await;
    assert_eq!(s.evidence["delivery"]["state"], "unpublished", "{s:?}");
    assert_eq!(s.evidence["checks"][0]["verdict"], "passed", "{s:?}");
}

#[tokio::test]
async fn accepted_result_completes_task_with_expected_version() {
    let env = auto_env("required", axum::http::StatusCode::OK).await;
    let (checkpoint, validator) = (env.checkpoint.clone(), env.validator.clone());
    env.check(env.source, &checkpoint, &validator, "passed")
        .await;
    env.delivery("published").await;
    let s = env.settle().await;
    assert_eq!(s.disposition, AutoDisposition::Confirmed, "{s:?}");
    let op = s.operation_id.unwrap();
    assert_eq!(s.dispatch_count, 1);
    assert_eq!(env.writes.load(Ordering::SeqCst), 1);
    {
        let bodies = env.bodies.lock().unwrap();
        let body = &bodies[0];
        assert_eq!(
            body["idempotency_key"],
            format!("aq-auto-settlement-v1:{op}")
        );
        assert_eq!(body["operations"][0]["op"], "update_node");
        assert_eq!(
            body["operations"][0]["id"],
            json!(env.f.selection.lineage.task_id)
        );
        assert_eq!(
            body["operations"][0]["expected_version"],
            json!(env.expected_version)
        );
        assert_eq!(
            body["operations"][0]["patch"],
            json!({"task_state":"completed"})
        );
    }
    assert!(
        !env.holds_task().await,
        "confirmed receipt releases the claim"
    );
    // Replay and a duplicate worker re-POST keep the same operation, no new write.
    let again = env.settle().await;
    assert_eq!(again.disposition, AutoDisposition::Confirmed);
    assert_eq!(again.operation_id, Some(op));
    let auth = context(&env.f).await;
    results::record(&env.f.state, &auth, &env.report)
        .await
        .unwrap();
    assert_eq!(env.writes.load(Ordering::SeqCst), 1);
    assert_eq!(
        settlement_row(&env.f, env.report.report_id).await.0,
        "confirmed"
    );
}

#[tokio::test]
async fn version_conflict_holds_and_never_overwrites() {
    let env = auto_env("not_required", axum::http::StatusCode::CONFLICT).await;
    let (checkpoint, validator) = (env.checkpoint.clone(), env.validator.clone());
    env.check(env.source, &checkpoint, &validator, "passed")
        .await;
    let s = env.settle().await;
    assert_eq!(s.disposition, AutoDisposition::Held, "{s:?}");
    assert_eq!(s.reason.as_deref(), Some("graph_version_conflict"));
    assert_eq!(env.writes.load(Ordering::SeqCst), 1);
    // Re-evaluation never re-dispatches the stale expected version.
    let s = env.settle().await;
    assert_eq!(s.reason.as_deref(), Some("graph_version_conflict"));
    assert_eq!(env.writes.load(Ordering::SeqCst), 1);
    assert!(env.holds_task().await);
}

#[tokio::test]
async fn parked_suspension_holds_as_suspended_until_successor_ack() {
    let env = auto_env("required", axum::http::StatusCode::OK).await;
    let (checkpoint, validator) = (env.checkpoint.clone(), env.validator.clone());
    env.check(env.source, &checkpoint, &validator, "passed")
        .await;
    env.delivery("published").await;
    // H5: a recorded RetainedSuspended/Held parking report is neither success
    // nor failure, even with otherwise complete evidence.
    let pool = env.f.db.pool().await;
    let continuation = Uuid::new_v4();
    sqlx::query("INSERT INTO hibernation_suspensions(continuation_id,wait_id,predecessor_attempt_id,predecessor_node_id,intent,directive_packet,phase,parking,created_at_ms,updated_at_ms) VALUES($1,$2,$3,$4,'{}','{}','parked_retained','{}',$5,$5)")
        .bind(continuation).bind(Uuid::new_v4()).bind(env.report.binding.attempt_id)
        .bind(env.f.node.node_id).bind(NOW as i64)
        .execute(&pool).await.unwrap();
    let s = env.settle().await;
    env.assert_not_completed(&s, AutoDisposition::Held, "suspended")
        .await;
    assert_eq!(s.evidence["suspension"]["phase"], "parked_retained");
    for phase in ["parked_held", "continued"] {
        sqlx::query("UPDATE hibernation_suspensions SET phase=$2 WHERE continuation_id=$1")
            .bind(continuation)
            .bind(phase)
            .execute(&pool)
            .await
            .unwrap();
        let s = env.settle().await;
        env.assert_not_completed(&s, AutoDisposition::Held, "suspended")
            .await;
    }
    // After the successor's exact BootstrapAck the successor owns the task;
    // the predecessor's report still never settles it.
    sqlx::query(
        "UPDATE hibernation_suspensions SET phase='acknowledged',ack='{}' WHERE continuation_id=$1",
    )
    .bind(continuation)
    .execute(&pool)
    .await
    .unwrap();
    let s = env.settle().await;
    env.assert_not_completed(&s, AutoDisposition::Held, "continued_by_successor")
        .await;
}
