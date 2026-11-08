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

pub(crate) struct AutoEnv {
    pub(crate) f: Fixture,
    pub(crate) report: ResultReport,
    pub(crate) source: Uuid,
    pub(crate) checkpoint: String,
    check_id: String,
    validator: String,
    expected_version: u64,
    writes: Arc<AtomicUsize>,
    bodies: Arc<Mutex<Vec<Value>>>,
    _directory: tempfile::TempDir,
}

async fn auto_env(delivery: &str, graph_status: axum::http::StatusCode) -> AutoEnv {
    auto_env_with(delivery, graph_status, true, None).await
}

pub(crate) async fn auto_env_with(
    delivery: &str,
    graph_status: axum::http::StatusCode,
    keep_checks: bool,
    process_outcome: Option<aq_admission::transport::ProcessOutcome>,
) -> AutoEnv {
    Box::pin(auto_env_profiled(
        delivery,
        graph_status,
        keep_checks,
        process_outcome,
        None,
    ))
    .await
}

async fn auto_env_profiled(
    delivery: &str,
    graph_status: axum::http::StatusCode,
    keep_checks: bool,
    process_outcome: Option<aq_admission::transport::ProcessOutcome>,
    graph_profile: Option<Value>,
) -> AutoEnv {
    let (mut f, _, mut report) = Box::pin(prepare_with_policy_checks(
        true,
        true,
        graph_profile,
        false,
        Some(default_policy(delivery)),
        keep_checks,
    ))
    .await;
    if let Some(outcome) = process_outcome {
        report.outcome = outcome;
    }
    let source = Uuid::new_v4();
    report.source_receipt_ids.push(source);
    let auth = context(&f).await;
    results::record(&f.state, &auth, &report).await.unwrap();
    // The record hook evaluated already: nothing is retained yet, so it holds.
    let first = settlement_row(&f, report.report_id).await;
    if matches!(
        report.outcome,
        aq_admission::transport::ProcessOutcome::Success
    ) {
        assert_eq!(first.0, "held");
        assert_eq!(first.1.as_deref(), Some("source_not_retained"));
    } else {
        // Acceptance precedes every evidence gate: a non-Success report is
        // refused at once, retained source or not.
        assert_eq!(first.0, "rework_required");
        assert_eq!(first.1.as_deref(), Some("not_accepted"));
    }

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
    let (check_id, validator) = recorded
        .outcome
        .required_checks
        .first()
        .map(|check| {
            (
                check.id.as_str().to_owned(),
                check.validator.digest.as_str().to_owned(),
            )
        })
        .unwrap_or_default();
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

/// MS1 review B1: a production-shaped offer (policy actor, no required checks,
/// delivery not required) completes only on a Success report. A failed or an
/// unproven (Unknown) run never completes the task or releases dependents.
#[tokio::test]
async fn failed_or_unknown_report_never_completes_a_task_without_required_checks() {
    use aq_admission::transport::ProcessOutcome;
    for outcome in [ProcessOutcome::Failed, ProcessOutcome::Unknown] {
        let env = auto_env_with(
            "not_required",
            axum::http::StatusCode::OK,
            false,
            Some(outcome),
        )
        .await;
        let s = env.settle().await;
        env.assert_not_completed(&s, AutoDisposition::ReworkRequired, "not_accepted")
            .await;
    }
    let env = auto_env_with(
        "not_required",
        axum::http::StatusCode::OK,
        false,
        Some(ProcessOutcome::Success),
    )
    .await;
    let s = env.settle().await;
    assert_eq!(s.disposition, AutoDisposition::Confirmed, "{s:?}");
    assert_eq!(env.writes.load(Ordering::SeqCst), 1);
    assert!(
        !env.holds_task().await,
        "confirmed receipt releases the claim"
    );
}

/// Synthetic central aq-git peer: answers `candidate.create` and `publish`
/// in order, after checking the intent was committed before the socket call.
pub(crate) async fn git_peer(
    pool: sqlx::PgPool,
    socket: std::path::PathBuf,
    replies: Vec<Value>,
) -> tokio::task::JoinHandle<Vec<Value>> {
    let peer = git_peer_cancelling(pool, socket, replies, None).await;
    tokio::spawn(async move { peer.await.unwrap().0 })
}

/// An owner cancel issued by the peer while it holds request `index` (before
/// replying): the actual B3 cancellation entry point, under its own locks.
type CancelAt = Option<(usize, aq_server::AppState, Uuid)>;

async fn git_peer_cancelling(
    pool: sqlx::PgPool,
    socket: std::path::PathBuf,
    replies: Vec<Value>,
    cancel_at: CancelAt,
) -> tokio::task::JoinHandle<(Vec<Value>, Option<String>)> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let listener = tokio::net::UnixListener::bind(socket).unwrap();
    tokio::spawn(async move {
        let mut seen = Vec::new();
        let mut cancelled = None;
        for (index, mut reply) in replies.into_iter().enumerate() {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let req: Value = serde_json::from_str(&line).unwrap();
            let id = Uuid::parse_str(req["operation_id"].as_str().unwrap()).unwrap();
            let stored: Vec<u8> =
                sqlx::query_scalar("SELECT request FROM delivery_intents WHERE operation_id=$1")
                    .bind(id)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(stored, line.as_bytes(), "intent committed before the call");
            if let Some((at, state, attempt)) = &cancel_at
                && *at == index
            {
                let receipt = aq_server::profiled_execution::cancellation::cancel(
                    state,
                    *attempt,
                    Uuid::new_v4(),
                    "owner cancel while publishing",
                )
                .await
                .unwrap();
                cancelled = Some(json!(receipt.disposition).as_str().unwrap().to_owned());
            }
            if reply["result"]["candidate_id"] == "OP" {
                reply["result"]["candidate_id"] = req["operation_id"].clone();
                reply["result"]["retention_handle"] = req["operation_id"].clone();
            }
            reply["protocol_version"] = json!(1);
            reply["request_id"] = req["request_id"].clone();
            reply["operation_id"] = req["operation_id"].clone();
            reader
                .get_mut()
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .unwrap();
            seen.push(req);
        }
        (seen, cancelled)
    })
}

/// Counts every connection to the central socket; nothing may arrive.
pub(crate) fn silent_git(socket: &std::path::Path) -> Arc<AtomicUsize> {
    let listener = tokio::net::UnixListener::bind(socket).unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let c = count.clone();
    tokio::spawn(async move {
        while listener.accept().await.is_ok() {
            c.fetch_add(1, Ordering::SeqCst);
        }
    });
    count
}

struct PublishEnv {
    env: AutoEnv,
    delivery: aq_server::delivery::DeliveryService,
    socket: std::path::PathBuf,
    _git: tempfile::TempDir,
}
async fn publish_env() -> PublishEnv {
    publish_env_with(None).await
}
async fn publish_env_with(outcome: Option<aq_admission::transport::ProcessOutcome>) -> PublishEnv {
    let env = auto_env_with("required", axum::http::StatusCode::OK, true, outcome).await;
    let (checkpoint, validator) = (env.checkpoint.clone(), env.validator.clone());
    env.check(env.source, &checkpoint, &validator, "passed")
        .await;
    let s = env.settle().await;
    if outcome.is_none() {
        env.assert_not_completed(&s, AutoDisposition::Held, "delivery_evidence_missing")
            .await;
    }
    let pool = env.f.db.pool().await;
    let project = admitted(&env).await.project_id;
    sqlx::query("UPDATE project_execution_bindings SET binding=jsonb_set(binding,'{publication}',$2) WHERE project_id=$1")
        .bind(project)
        .bind(json!({"mode":"fast_forward","target":"refs/heads/main"}))
        .execute(&pool)
        .await
        .unwrap();
    let git = tempfile::tempdir().unwrap();
    let socket = git.path().join("git.sock");
    let config = aq_server::source_retention::SourceRetentionConfig {
        keep_per_task: 2,
        max_age: 30 * 86400,
        staging_root: git.path().join("staging"),
        git_root: git.path().join("git"),
        git_socket: socket.clone(),
        max_staging_bytes: 2097152,
        max_bundle_bytes: 4096,
        bulk_concurrency: 2,
        incomplete_ttl_secs: 60,
    };
    PublishEnv {
        delivery: aq_server::delivery::DeliveryService::new(pool, &config),
        env,
        socket,
        _git: git,
    }
}
pub(crate) async fn admitted(env: &AutoEnv) -> aq_admission::SourceAdmission {
    let pool = env.f.db.pool().await;
    let mut tx = pool.begin().await.unwrap();
    let r = results::load_locked(&env.f.state, &mut tx, env.report.report_id)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    r.spec.source
}
pub(crate) fn ready_candidate(candidate: &str) -> Value {
    json!({"ok":true,"result":{"candidate_id":"OP","state":"ready","candidate":{"algorithm":"sha1","hex":candidate},
        "clean_prefix":{"algorithm":"sha1","hex":candidate},"retention_handle":"OP","conflict_step":null,
        "conflicting_paths":[],"conflict_path_count":0,"conflict_paths_truncated":false}})
}

#[tokio::test]
async fn retained_result_publishes_then_completes_and_advances_source() {
    use aq_server::profiled_execution::settlement::publication;
    let p = publish_env().await;
    let env = &p.env;
    let pool = env.f.db.pool().await;
    let base = admitted(env).await.resolved_object.hex;
    let candidate = "c".repeat(40);
    let peer = git_peer(
        pool.clone(),
        p.socket.clone(),
        vec![
            ready_candidate(&candidate),
            json!({"ok":true,"operation_state":"complete","result":{"outcome":"applied","target":"refs/heads/main",
                "old":{"algorithm":"sha1","hex":base},"new":{"algorithm":"sha1","hex":candidate},
                "acknowledged_at":"2026-11-05T00:00:00Z"}}),
        ],
    )
    .await;
    let s = publication::deliver_then_settle(&env.f.state, &p.delivery, env.report.report_id)
        .await
        .unwrap()
        .unwrap();
    let sent = peer.await.unwrap();
    assert_eq!(s.disposition, AutoDisposition::Confirmed, "{s:?}");
    assert_eq!(s.evidence["delivery"]["state"], "published", "{s:?}");
    assert_eq!(env.writes.load(Ordering::SeqCst), 1, "one Graph completion");
    assert!(
        !env.holds_task().await,
        "dependents released only after publication"
    );
    // Server-owned inputs only: the retained receipt, the admitted base and the policy target.
    assert_eq!(sent[0]["operation"], "candidate.create");
    assert_eq!(sent[0]["params"]["base"]["hex"], json!(base));
    assert_eq!(
        sent[0]["params"]["sources"][0]["oid"]["hex"],
        json!(env.checkpoint)
    );
    assert_eq!(sent[1]["operation"], "publish");
    assert_eq!(sent[1]["params"]["target"], "refs/heads/main");
    assert_eq!(sent[1]["params"]["expected_old"]["hex"], json!(base));
    assert_eq!(sent[1]["params"]["candidate_id"], sent[0]["operation_id"]);
    // The project's source head advanced to the published commit, retained by
    // the candidate handle.
    let source = &admitted(env).await;
    let head = publication::current_head(
        &pool,
        source.project_id,
        source.repository_id,
        source.repository_binding_generation,
        "refs/heads/main",
    )
    .await
    .unwrap()
    .expect("source head");
    assert_eq!(head.oid, candidate);
    assert_eq!(head.previous, base);
    assert_eq!(head.sequence, 1);
    assert_eq!(json!(head.retention_handle), sent[0]["operation_id"]);
    // Replay is idempotent and does no aq-git I/O (no socket listens now).
    std::fs::remove_file(&p.socket).unwrap();
    let again = publication::deliver_then_settle(&env.f.state, &p.delivery, env.report.report_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(again.disposition, AutoDisposition::Confirmed);
    assert_eq!(env.writes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn moved_target_is_unpublished_rework_and_never_advances_source() {
    use aq_server::profiled_execution::settlement::publication;
    let p = publish_env().await;
    let env = &p.env;
    let pool = env.f.db.pool().await;
    let peer = git_peer(
        pool.clone(),
        p.socket.clone(),
        vec![
            ready_candidate(&"c".repeat(40)),
            json!({"ok":true,"operation_state":"complete","result":{"outcome":"rejected","reason":"target_moved",
                "observed_target":{"algorithm":"sha1","hex":"d".repeat(40)},"observed_at":"2026-11-05T00:00:00Z"}}),
        ],
    )
    .await;
    let s = publication::deliver_then_settle(&env.f.state, &p.delivery, env.report.report_id)
        .await
        .unwrap()
        .unwrap();
    peer.await.unwrap();
    env.assert_not_completed(&s, AutoDisposition::ReworkRequired, "delivery_unpublished")
        .await;
    let source = &admitted(env).await;
    assert!(
        publication::current_head(
            &pool,
            source.project_id,
            source.repository_id,
            source.repository_binding_generation,
            "refs/heads/main",
        )
        .await
        .unwrap()
        .is_none()
    );
    // B4 never replaces it from the same immutable export: the conflict would recur.
    let mut tx = pool.begin().await.unwrap();
    let task = results::load_locked(&env.f.state, &mut tx, env.report.report_id)
        .await
        .unwrap()
        .spec
        .task
        .task_id;
    tx.rollback().await.unwrap();
    let assessed = aq_server::profiled_execution::replacement::assess_task(&env.f.state, task, NOW)
        .await
        .unwrap();
    assert!(
        matches!(
            assessed,
            aq_server::profiled_execution::replacement::Assessment::Blocked(
                "replacement_after_unpublished_delivery_needs_new_source"
            )
        ),
        "{assessed:?}"
    );
}

async fn evidence_rows(env: &AutoEnv) -> Vec<(String, Value)> {
    sqlx::query_as("SELECT state,evidence FROM profiled_delivery_evidence WHERE attempt_id=$1")
        .bind(env.report.binding.attempt_id)
        .fetch_all(&env.f.db.pool().await)
        .await
        .unwrap()
}
async fn head_of(
    env: &AutoEnv,
) -> Option<aq_server::profiled_execution::settlement::publication::SourceHead> {
    let source = admitted(env).await;
    aq_server::profiled_execution::settlement::publication::current_head(
        &env.f.db.pool().await,
        source.project_id,
        source.repository_id,
        source.repository_binding_generation,
        "refs/heads/main",
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn non_success_or_cancelled_report_never_reaches_aq_git() {
    use aq_admission::transport::ProcessOutcome;
    use aq_server::profiled_execution::settlement::publication;
    // A non-Success report is refused by D3 before any delivery.
    for outcome in [ProcessOutcome::Failed, ProcessOutcome::Unknown] {
        let p = publish_env_with(Some(outcome)).await;
        let calls = silent_git(&p.socket);
        let s =
            publication::deliver_then_settle(&p.env.f.state, &p.delivery, p.env.report.report_id)
                .await
                .unwrap()
                .unwrap();
        p.env
            .assert_not_completed(&s, AutoDisposition::ReworkRequired, "not_accepted")
            .await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "{outcome:?} reached aq-git"
        );
        assert!(evidence_rows(&p.env).await.is_empty());
    }
    // An owner cancel of an accepted, held result lands before delivery.
    let p = publish_env().await;
    let calls = silent_git(&p.socket);
    let receipt = aq_server::profiled_execution::cancellation::cancel(
        &p.env.f.state,
        p.env.report.binding.attempt_id,
        Uuid::new_v4(),
        "owner cancel before publication",
    )
    .await
    .unwrap();
    assert_eq!(json!(receipt.disposition), "requested");
    let s = publication::deliver_then_settle(&p.env.f.state, &p.delivery, p.env.report.report_id)
        .await
        .unwrap()
        .unwrap();
    p.env
        .assert_not_completed(&s, AutoDisposition::ReworkRequired, "not_accepted")
        .await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "cancelled result reached aq-git"
    );
    assert!(evidence_rows(&p.env).await.is_empty());
}

#[tokio::test]
async fn cancel_between_candidate_and_publish_is_never_published() {
    use aq_server::profiled_execution::settlement::publication;
    let p = publish_env().await;
    let env = &p.env;
    let pool = env.f.db.pool().await;
    // The cancel lands while aq-git is building the candidate.
    let peer = git_peer_cancelling(
        pool.clone(),
        p.socket.clone(),
        vec![ready_candidate(&"c".repeat(40))],
        Some((0, env.f.state.clone(), env.report.binding.attempt_id)),
    )
    .await;
    let s = publication::deliver_then_settle(&env.f.state, &p.delivery, env.report.report_id)
        .await
        .unwrap()
        .unwrap();
    let (sent, cancelled) = peer.await.unwrap();
    assert_eq!(cancelled.as_deref(), Some("requested"));
    assert_eq!(sent.len(), 1, "only candidate.create reached aq-git");
    env.assert_not_completed(&s, AutoDisposition::ReworkRequired, "not_accepted")
        .await;
    let publishes: i64 =
        sqlx::query_scalar("SELECT count(*) FROM delivery_intents WHERE operation='publish'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(publishes, 0, "no publish intent was ever frozen");
    assert!(evidence_rows(env).await.is_empty());
    assert!(head_of(env).await.is_none());
}

#[tokio::test]
async fn cancel_during_publishing_is_recorded_as_already_publishing() {
    use aq_server::profiled_execution::settlement::publication;
    let p = publish_env().await;
    let env = &p.env;
    let pool = env.f.db.pool().await;
    let base = admitted(env).await.resolved_object.hex;
    let candidate = "c".repeat(40);
    // The cancel lands while the publish request is in flight (fence held).
    let peer = git_peer_cancelling(
        pool.clone(),
        p.socket.clone(),
        vec![
            ready_candidate(&candidate),
            json!({"ok":true,"operation_state":"complete","result":{"outcome":"applied","target":"refs/heads/main",
                "old":{"algorithm":"sha1","hex":base},"new":{"algorithm":"sha1","hex":candidate},
                "acknowledged_at":"2026-11-05T00:00:00Z"}}),
        ],
        Some((1, env.f.state.clone(), env.report.binding.attempt_id)),
    )
    .await;
    let s = publication::deliver_then_settle(&env.f.state, &p.delivery, env.report.report_id)
        .await
        .unwrap()
        .unwrap();
    let (_, cancelled) = peer.await.unwrap();
    assert_eq!(cancelled.as_deref(), Some("already_publishing"));
    let cancellations: i64 =
        sqlx::query_scalar("SELECT count(*) FROM profiled_cancellations WHERE attempt_id=$1")
            .bind(env.report.binding.attempt_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(cancellations, 0, "the fenced publication was not cancelled");
    assert_eq!(s.disposition, AutoDisposition::Confirmed, "{s:?}");
    assert_eq!(head_of(env).await.unwrap().oid, candidate);
}

#[tokio::test]
async fn ambiguous_or_foreign_presence_stays_held_without_a_source_head() {
    use aq_server::profiled_execution::settlement::publication;
    let candidate = "c".repeat(40);
    for (case, reply) in [
        (
            "operation_indeterminate",
            json!({"ok":false,"operation_state":"pending","error":{"code":"operation_indeterminate",
            "message":"push outcome unknown","details":{"dispatched":true,"remote_effect_unknown":true},"retry":"reconcile_first"}}),
        ),
        (
            "observed_target_differs",
            json!({"ok":true,"operation_state":"complete","result":{"outcome":"already_present",
            "observed_target":{"algorithm":"sha1","hex":"d".repeat(40)},"observed_at":"2026-11-05T00:00:00Z",
            "caused_by_this_operation":false}}),
        ),
    ] {
        let p = publish_env().await;
        let env = &p.env;
        let peer = git_peer(
            env.f.db.pool().await,
            p.socket.clone(),
            vec![ready_candidate(&candidate), reply],
        )
        .await;
        let s = publication::deliver_then_settle(&env.f.state, &p.delivery, env.report.report_id)
            .await
            .unwrap()
            .unwrap();
        peer.await.unwrap();
        env.assert_not_completed(&s, AutoDisposition::Held, "delivery_indeterminate")
            .await;
        let rows = evidence_rows(env).await;
        assert_eq!(rows.len(), 1, "{case}");
        assert_eq!(rows[0].0, "indeterminate", "{case}");
        assert_eq!(rows[0].1["result"]["reason"], case, "{case}");
        assert!(
            rows[0].1["phase"].is_null(),
            "{case}: final evidence replaced the fence"
        );
        assert!(head_of(env).await.is_none(), "{case}");
        // Held, never re-pushed: a replay does no aq-git I/O.
        std::fs::remove_file(&p.socket).unwrap();
        let calls = silent_git(&p.socket);
        let again =
            publication::deliver_then_settle(&env.f.state, &p.delivery, env.report.report_id)
                .await
                .unwrap()
                .unwrap();
        assert_eq!(
            again.reason.as_deref(),
            Some("delivery_indeterminate"),
            "{case}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0, "{case}");
    }
}

/// MS1 retry (B4): after a final rework, the D3 policy actor records the
/// replacement facts that only owning domains prove (the central retention
/// receipt pinned by the report; a settled read-only AQ effect ledger), so the
/// failed attempt becomes replaceable. It never completes the task, never
/// releases the claim, and never touches a confirmed (Success) settlement.
#[tokio::test]
async fn reworked_failed_attempt_records_replacement_facts_and_becomes_replaceable() {
    use aq_admission::transport::ProcessOutcome;
    use aq_server::{
        execution::profiled::{Effects, Retention},
        profiled_execution::replacement::{self, Assessment},
    };
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../../../protocol/agent-access/v1/fixtures/mutation-delivery.json"
    ))
    .unwrap();
    let mut profile = fixture["proposal_profiles"][0]["profile"].clone();
    profile["capabilities"] = json!(["read.get", "read.context"]);
    let env = Box::pin(auto_env_profiled(
        "not_required",
        axum::http::StatusCode::OK,
        false,
        Some(ProcessOutcome::Failed),
        Some(profile.clone()),
    ))
    .await;
    let pool = env.f.db.pool().await;
    let attempt = env.report.binding.attempt_id;
    let hot = || async {
        let v: Value =
            sqlx::query_scalar("SELECT state FROM profiled_authority_hot WHERE attempt_id=$1")
                .bind(attempt)
                .fetch_one(&pool)
                .await
                .unwrap();
        serde_json::from_value::<aq_server::execution::authority::hot::HotState>(v).unwrap()
    };
    // The rework was recorded before any source receipt or agent binding
    // existed: no fact could be proven, so none was written.
    let before = hot().await;
    assert!(matches!(before.retention, Retention::Pending));
    assert!(matches!(before.effects, Effects::Unknown));
    let facts = settlement::record_replacement_facts(&env.f.state, env.report.report_id)
        .await
        .err();
    assert!(
        facts.is_some(),
        "no sealed agent binding: the ledger is unavailable"
    );
    assert!(
        matches!(hot().await.retention, Retention::Pending),
        "all or nothing per call"
    );

    // Synthetic sealed admission history, as settlement_effects.rs.
    let mut tx = pool.begin().await.unwrap();
    let recorded = results::load_locked(&env.f.state, &mut tx, env.report.report_id)
        .await
        .unwrap();
    let SessionIdentity::Remote {
        node_id,
        boot_id,
        session_id,
        ..
    } = env.report.binding.session
    else {
        unreachable!()
    };
    let binding = json!({"task_id":recorded.spec.task.task_id,"attempt_id":attempt,"session_id":session_id,"generation":env.report.binding.assignment_generation,"node_id":node_id,"boot_id":boot_id,"project_id":recorded.spec.lifecycle.project_id,"workspace_id":recorded.spec.task.workspace_id,"resolved_spec_digest":env.report.binding.execution_spec_digest,"profile_digest":aq_agent_api::digest(&profile).unwrap(),"context_digest":env.f.profiled.graph.context_digest});
    assert!(aq_agent_api::valid("Binding", &binding));
    sqlx::query("INSERT INTO agent_launch_bindings(attempt_id,node_id,binding,profile,context) VALUES($1,$2,$3,$4,'{}')").bind(attempt).bind(node_id).bind(&binding).bind(&profile).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();

    // The replay of the final rework now records both facts; the verdict,
    // the claim and the Graph are unchanged.
    let s = env.settle().await;
    env.assert_not_completed(&s, AutoDisposition::ReworkRequired, "not_accepted")
        .await;
    let after = hot().await;
    let Retention::Retained { receipt } = &after.retention else {
        panic!("retained source fact expected: {:?}", after.retention);
    };
    assert_eq!(
        receipt.id.as_str(),
        format!("aq.git-import-receipt/{}", env.source)
    );
    assert!(matches!(after.effects, Effects::Quiescent { .. }));
    // Idempotent: a second replay changes nothing.
    let again = settlement::record_replacement_facts(&env.f.state, env.report.report_id)
        .await
        .unwrap();
    assert!(again.source_retained && again.effects_quiescent && !again.changed);
    assert_eq!(
        serde_json::to_value(hot().await).unwrap(),
        serde_json::to_value(&after).unwrap()
    );
    // The backfill cursor no longer lists it.
    assert!(
        settlement::reworked_without_facts(&env.f.state, None, 8)
            .await
            .unwrap()
            .is_empty()
    );

    // B4 now admits a replacement of the failed predecessor, which still holds
    // the task until the replacement owner releases it.
    sqlx::query("ALTER TABLE profiled_launches DISABLE TRIGGER profiled_launch_guard")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE profiled_launches SET graph_task_observation=jsonb_set(graph_task_observation,'{native_offer}',jsonb_build_object('offered',jsonb_build_object('clock',jsonb_build_object('server_time_ms',$2::BIGINT)))) WHERE attempt_id=$1")
        .bind(attempt).bind(NOW as i64).execute(&pool).await.unwrap();
    sqlx::query("ALTER TABLE profiled_launches ENABLE TRIGGER profiled_launch_guard")
        .execute(&pool)
        .await
        .unwrap();
    let task = env.f.selection.lineage.task_id;
    match replacement::assess_task(&env.f.state, task, NOW + 1)
        .await
        .unwrap()
    {
        Assessment::Replace(r) => {
            assert_eq!(r.predecessor, attempt);
            assert_eq!(r.attempts_started, 1);
            assert!(r.needs_release);
            assert_eq!(r.retained_source_receipt.id, receipt.id);
        }
        other => panic!("reworked failed attempt must be replaceable: {other:?}"),
    }
    assert!(env.holds_task().await);
}

/// The facts writer never acts on a confirmed (Success) settlement or on a
/// report that is not finally reworked.
#[tokio::test]
async fn replacement_facts_are_never_recorded_for_a_confirmed_result() {
    use aq_admission::transport::ProcessOutcome;
    use aq_server::execution::profiled::{Effects, Retention};
    let env = auto_env_with(
        "not_required",
        axum::http::StatusCode::OK,
        false,
        Some(ProcessOutcome::Success),
    )
    .await;
    let s = env.settle().await;
    assert_eq!(s.disposition, AutoDisposition::Confirmed, "{s:?}");
    let facts = settlement::record_replacement_facts(&env.f.state, env.report.report_id)
        .await
        .unwrap();
    assert!(!facts.changed && !facts.source_retained && !facts.effects_quiescent);
    let v: Value =
        sqlx::query_scalar("SELECT state FROM profiled_authority_hot WHERE attempt_id=$1")
            .bind(env.report.binding.attempt_id)
            .fetch_one(&env.f.db.pool().await)
            .await
            .unwrap();
    let hot: aq_server::execution::authority::hot::HotState = serde_json::from_value(v).unwrap();
    assert!(matches!(hot.retention, Retention::Pending));
    assert!(matches!(hot.effects, Effects::Unknown));
}
