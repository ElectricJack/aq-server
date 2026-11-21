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
    pub(crate) validator: String,
    expected_version: u64,
    pub(crate) writes: Arc<AtomicUsize>,
    bodies: Arc<Mutex<Vec<Value>>>,
    _directory: tempfile::TempDir,
    #[cfg(feature = "delivery_playbook")]
    _task_ref: Option<super::pushed_ref::TaskRef>,
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
    Box::pin(auto_env_source(
        delivery,
        graph_status,
        keep_checks,
        process_outcome,
        graph_profile,
        true,
        None,
    ))
    .await
}

/// `changed`: the retained checkpoint is a new commit on the admitted base;
/// otherwise it is the admitted base itself (the attempt changed no file).
pub(crate) async fn auto_env_source(
    delivery: &str,
    graph_status: axum::http::StatusCode,
    keep_checks: bool,
    process_outcome: Option<aq_admission::transport::ProcessOutcome>,
    graph_profile: Option<Value>,
    changed: bool,
    conclusion: Option<&str>,
) -> AutoEnv {
    let (f, _, mut report) = Box::pin(prepare_with_policy_checks(
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
    if let Some(conclusion) = conclusion {
        report.conclusion = Label::new(conclusion).unwrap();
    }
    finish_auto_env(f, report, graph_status, changed, false).await
}

async fn finish_auto_env(
    mut f: Fixture,
    mut report: ResultReport,
    graph_status: axum::http::StatusCode,
    changed: bool,
    enrolled: bool,
) -> AutoEnv {
    let auth = context(&f).await;
    #[cfg(feature = "delivery_playbook")]
    let task_ref = if enrolled {
        let task_ref = super::pushed_ref::task_ref(&mut f, &mut report).await;
        let head = if changed {
            "4".repeat(40)
        } else {
            f.profiled.source.base.hex.clone()
        };
        *task_ref.at.lock().unwrap() = Some(head.clone());
        let pushed = aq_delivery::git_grant::Pushed::At(aq_delivery::git_grant::PushedRef {
            reference: task_ref.task_ref.clone(),
            head,
        });
        let headers = super::pushed_ref::headers(&pushed);
        results::record_pushed(&f.state, &auth, &report, &headers)
            .await
            .unwrap();
        assert_eq!(settlement_rows(&f, report.report_id).await, 0);
        let facts = result_facts(&f, report.report_id)
            .await
            .expect("facts recorded");
        assert_eq!(facts["source"]["changed"], json!(changed));
        Some(task_ref)
    } else {
        None
    };
    let source = if enrolled {
        report.report_id
    } else {
        let source = Uuid::new_v4();
        report.source_receipt_ids.push(source);
        results::record(&f.state, &auth, &report).await.unwrap();
        let first = settlement_row(&f, report.report_id).await;
        if matches!(
            report.outcome,
            aq_admission::transport::ProcessOutcome::Success
        ) {
            assert_eq!(first.0, "held");
            assert_eq!(first.1.as_deref(), Some("source_not_retained"));
        } else {
            assert_eq!(first.0, "rework_required");
            assert!(matches!(
                first.1.as_deref(),
                Some("not_accepted" | "agent_blocked")
            ));
        }
        source
    };

    // Synthetic retained-source owner row (validation only, not real Git).
    let pool = &f.db.pool().await;
    let transfer_id = Uuid::new_v4();
    let artifact_id = Uuid::new_v4();
    let manifest = aq_worker_protocol::TransferManifest {
        transfer_id,
        role: aq_worker_protocol::SourceRole::Checkpoint,
        checkpoint: if changed {
            aq_worker_protocol::ObjectId {
                algorithm: aq_worker_protocol::ObjectAlgorithm::Sha1,
                hex: "4".repeat(40),
            }
        } else {
            f.profiled.source.base.clone()
        },
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
    // The human work read's Context of the task (spec 0016 section 7's task read).
    let read = graph.clone();
    let app = Router::new().route(
        "/v1/workspaces/{workspace}/nodes/{id}/context",
        axum::routing::get(move || {
            std::future::ready(([("X-AQ-Graph-API-Version", "1")], Json(read.clone())))
        }),
    );
    let app = app.route(
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
        checkpoint: manifest.checkpoint.hex.clone(),
        f,
        report,
        source,
        check_id,
        validator,
        expected_version,
        writes,
        bodies,
        _directory: directory,
        #[cfg(feature = "delivery_playbook")]
        _task_ref: task_ref,
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

/// How many settlement rows the report has (0 or 1).
#[cfg(feature = "delivery_playbook")]
async fn settlement_rows(f: &Fixture, report: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM profiled_auto_settlements WHERE report_id=$1")
        .bind(report)
        .fetch_one(&f.db.pool().await)
        .await
        .unwrap()
}

/// The facts the result path recorded for the report (spec 0016 section 7).
#[cfg(feature = "delivery_playbook")]
async fn result_facts(f: &Fixture, report: Uuid) -> Option<Value> {
    sqlx::query_scalar("SELECT facts FROM delivery_result_facts WHERE report_id=$1")
        .bind(report)
        .fetch_optional(&f.db.pool().await)
        .await
        .unwrap()
}

impl AutoEnv {
    async fn settle(&self) -> AutoSettlement {
        settlement::settle_after_record(&self.f.state, self.report.report_id)
            .await
            .unwrap()
            .expect("default policy applies")
    }
    pub(crate) async fn check(
        &self,
        source: Uuid,
        checkpoint: &str,
        validator: &str,
        verdict: &str,
    ) {
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
    pub(crate) async fn holds_task(&self) -> bool {
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
        git_deadline_ms: 30_000,
    };
    PublishEnv {
        delivery: aq_server::delivery::DeliveryService::new(pool, &config.git_socket),
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

/// Owner decision 2026-11-13: an attempt that finishes successfully without
/// changing a file succeeds. Its retained checkpoint is the admitted base, so
/// required delivery has nothing to publish: no candidate, no push, no
/// delivery evidence, and the task completes.
#[tokio::test]
async fn unchanged_source_completes_without_publication() {
    use aq_server::profiled_execution::settlement::publication;
    let env = Box::pin(auto_env_source(
        "required",
        axum::http::StatusCode::OK,
        true,
        None,
        None,
        false,
        None,
    ))
    .await;
    assert_eq!(env.checkpoint, admitted(&env).await.resolved_object.hex);
    let (checkpoint, validator) = (env.checkpoint.clone(), env.validator.clone());
    env.check(env.source, &checkpoint, &validator, "passed")
        .await;
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
    let calls = silent_git(&socket);
    let config = aq_server::source_retention::SourceRetentionConfig {
        keep_per_task: 2,
        max_age: 30 * 86400,
        staging_root: git.path().join("staging"),
        git_root: git.path().join("git"),
        git_socket: socket,
        max_staging_bytes: 2097152,
        max_bundle_bytes: 4096,
        bulk_concurrency: 2,
        incomplete_ttl_secs: 60,
        git_deadline_ms: 30_000,
    };
    let delivery = aq_server::delivery::DeliveryService::new(pool.clone(), &config.git_socket);
    let settling = env.report.binding.attempt_id;
    common::roles::pin_active(&pool, settling).await;
    let s = publication::deliver_then_settle(&env.f.state, &delivery, env.report.report_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(s.disposition, AutoDisposition::Confirmed, "{s:?}");
    // Spec 0014 §7.1: the snapshot pin is released with the attempt's hold on its task.
    assert!(!common::roles::pinned(&pool, settling).await);
    assert_eq!(s.evidence["delivery"]["state"], "no_change", "{s:?}");
    assert_eq!(s.evidence["source"]["changed"], false, "{s:?}");
    assert_eq!(env.writes.load(Ordering::SeqCst), 1, "one Graph completion");
    assert!(!env.holds_task().await, "dependents are released");
    assert_eq!(calls.load(Ordering::SeqCst), 0, "nothing sent to aq-git");
    assert!(evidence_rows(&env).await.is_empty(), "no delivery evidence");
    assert!(head_of(&env).await.is_none(), "the source head never moves");
}

/// Review R1-M2: a Failed report whose agent said `Outcome: blocked` is reworked
/// with the reason `agent_blocked`; any other failure stays `not_accepted`.
#[tokio::test]
async fn a_blocked_agent_is_reworked_as_agent_blocked() {
    use aq_admission::transport::ProcessOutcome;
    for (conclusion, reason) in [
        (
            "Outcome: blocked; Changes: none; Copied: none; Claude exit 0; Result: no access to the repo",
            "agent_blocked",
        ),
        (
            "Outcome: done; Changes: none; Copied: none; Claude exit 1; Result: crashed",
            "not_accepted",
        ),
    ] {
        let env = Box::pin(auto_env_source(
            "not_required",
            axum::http::StatusCode::OK,
            false,
            Some(ProcessOutcome::Failed),
            None,
            false,
            Some(conclusion),
        ))
        .await;
        let s = env.settle().await;
        env.assert_not_completed(&s, AutoDisposition::ReworkRequired, reason)
            .await;
    }
}

/// Review I-2: a worker report whose agent claimed changes (or copied back
/// only files Git ignores) while the retained source is unchanged is refused
/// for rework, never completed as `no_change`; a claim of none completes.
#[tokio::test]
async fn claimed_changes_that_did_not_land_are_reworked() {
    for (conclusion, expected) in [
        (
            "Changes: src/lib.rs; Copied: none; Claude exit 0; Result: done",
            Some("claimed_changes_missing"),
        ),
        (
            "Changes: target/debug/aq-server; Claude exit 0; Summary: built",
            Some("claimed_changes_missing"),
        ),
        (
            "Changes: none; Copied: target/x; Claude exit 0; Result: CI passes",
            None,
        ),
    ] {
        let env = Box::pin(auto_env_source(
            "not_required",
            axum::http::StatusCode::OK,
            false,
            None,
            None,
            false,
            Some(conclusion),
        ))
        .await;
        let s = env.settle().await;
        match expected {
            Some(reason) => {
                env.assert_not_completed(&s, AutoDisposition::ReworkRequired, reason)
                    .await
            }
            None => assert_eq!(
                s.disposition,
                AutoDisposition::Confirmed,
                "{conclusion}: {s:?}"
            ),
        }
    }
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

pub(crate) async fn evidence_rows(env: &AutoEnv) -> Vec<(String, Value)> {
    sqlx::query_as("SELECT state,evidence FROM profiled_delivery_evidence WHERE attempt_id=$1")
        .bind(env.report.binding.attempt_id)
        .fetch_all(&env.f.db.pool().await)
        .await
        .unwrap()
}
pub(crate) async fn head_of(
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

/// 2026-11-21 (attachment task 8c6ba979, owner retry refused
/// `replacement_effects_unreconciled`): a returned attempt whose profile may write (here the
/// delivery fixture's `write.*` profile; `message.*` is the same verdict) has a final ledger,
/// so B4 records it quiescent and the attempt is replaceable once. Its acceptance verdict is
/// unchanged, `attempt.show`'s owner read names the missing fact before and none after, and
/// a replay changes nothing.
#[tokio::test]
async fn reworked_writer_attempt_with_final_ledger_becomes_replaceable() {
    use aq_admission::transport::ProcessOutcome;
    use aq_profiled_state::replacement_sql::inspect;
    use aq_server::{
        execution::profiled::Effects,
        profiled_execution::replacement::{self, Assessment},
    };
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../../../protocol/agent-access/v1/fixtures/mutation-delivery.json"
    ))
    .unwrap();
    let profile = fixture["proposal_profiles"][0]["profile"].clone();
    assert!(profile["capabilities"].to_string().contains("write."));
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
    // B4 needs the original offer instant (as the read-only test sets it).
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
    let shown = || async {
        let mut conn = pool.acquire().await.unwrap();
        inspect(&mut conn, attempt, NOW + 1).await.unwrap().unwrap()
    };
    let before = shown().await;
    assert_eq!(before["effects"]["kind"], "unknown", "{before}");
    let blockers = before["replacement"]["blockers"].to_string();
    assert!(blockers.contains("effects_unknown"), "{before}");
    seed_agent_binding(&env, &profile).await;
    let s = env.settle().await;
    env.assert_not_completed(&s, AutoDisposition::ReworkRequired, "not_accepted")
        .await;
    let after = shown().await;
    assert_eq!(after["effects"]["kind"], "quiescent", "{after}");
    assert_eq!(after["retention"]["kind"], "retained", "{after}");
    assert_eq!(after["replacement"]["blockers"], json!([]), "{after}");
    let again = settlement::record_replacement_facts(&env.f.state, env.report.report_id)
        .await
        .unwrap();
    assert!(again.effects_quiescent && !again.changed);
    assert_eq!(shown().await["effects"], after["effects"]);
    let v: Value =
        sqlx::query_scalar("SELECT state FROM profiled_authority_hot WHERE attempt_id=$1")
            .bind(attempt)
            .fetch_one(&pool)
            .await
            .unwrap();
    let hot: aq_server::execution::authority::hot::HotState = serde_json::from_value(v).unwrap();
    assert!(matches!(hot.effects, Effects::Quiescent { .. }));
    let task = env.f.selection.lineage.task_id;
    let assessment = replacement::assess_task(&env.f.state, task, NOW + 1).await;
    let Assessment::Replace(r) = assessment.unwrap() else {
        panic!("a returned writer attempt with a final ledger is replaceable");
    };
    assert_eq!((r.predecessor, r.needs_release), (attempt, true));
    assert!(
        env.holds_task().await,
        "only the replacement owner releases"
    );
}

async fn seed_agent_binding(env: &AutoEnv, profile: &Value) {
    let pool = env.f.db.pool().await;
    let attempt = env.report.binding.attempt_id;
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
    let binding = json!({"task_id":recorded.spec.task.task_id,"attempt_id":attempt,"session_id":session_id,"generation":env.report.binding.assignment_generation,"node_id":node_id,"boot_id":boot_id,"project_id":recorded.spec.lifecycle.project_id,"workspace_id":recorded.spec.task.workspace_id,"resolved_spec_digest":env.report.binding.execution_spec_digest,"profile_digest":aq_agent_api::digest(profile).unwrap(),"context_digest":env.f.profiled.graph.context_digest});
    sqlx::query("INSERT INTO agent_launch_bindings(attempt_id,node_id,binding,profile,context) VALUES($1,$2,$3,$4,'{}')").bind(attempt).bind(node_id).bind(&binding).bind(profile).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
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

/// Epic 0 (spec 0016 section 6.2): a playbook's `work.complete` and
/// `work.return` on a recorded result, through the same `prepare_locked` /
/// `dispatch` path and its fences. The playbook decides: no policy evidence
/// is asked for, only that the result is the attempt's current report head,
/// that the attempt still holds the task, that its stop is proven and that it
/// was neither revoked nor cancelled.
#[cfg(feature = "delivery_playbook")]
mod playbook {
    use super::*;
    use aq_server::{
        playbooks::domain::delivery_decisions::Settled,
        profiled_execution::settlement::{complete_by_playbook, return_by_playbook},
    };

    async fn auto_env(delivery: &str, status: axum::http::StatusCode) -> AutoEnv {
        let (f, _, report) = Box::pin(prepare_with_policy_checks(
            true,
            true,
            None,
            false,
            Some(default_policy(delivery)),
            true,
        ))
        .await;
        finish_auto_env(f, report, status, true, true).await
    }

    #[tokio::test]
    async fn persistent_scheduling_hold_notifies_its_project_once_after_settling_time() {
        use aq_playbook_domain::scheduling_notices;
        let env = Box::pin(auto_env("not_required", axum::http::StatusCode::OK)).await;
        let pool = env.f.db.pool().await;
        let task = env.f.selection.lineage.task_id;
        let project = env.f.selection.lineage.project_id;
        let attempt = env.report.binding.attempt_id;
        sqlx::query("UPDATE supervisor_controls c SET enabled=true, profile='{}',approved_by=$2 FROM supervisor_conversation_ids i WHERE i.conversation_id=c.conversation_id AND i.project_id=$1")
            .bind(project).bind(env.f.actor).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO task_holds(comment_id,task_id,attempts,project_id,reason,attempt_id,comment_body,held_at) VALUES($1,$2,1,$3,'lineage_time_budget_exhausted',$4,'budget exhausted',to_timestamp($5))")
            .bind(Uuid::new_v4()).bind(task).bind(project).bind(attempt).bind(NOW as f64/1000.0).execute(&pool).await.unwrap();
        assert_eq!(
            aq_machine_control::holds::unclaimed_count(&pool, &[task, Uuid::new_v4()])
                .await
                .unwrap(),
            1
        );
        let count = AtomicUsize::new(0);
        let send = async |_: &mut sqlx::Transaction<'_, sqlx::Postgres>,
                          scope: Option<Uuid>,
                          body: &str,
                          _: u64| {
            assert_eq!(scope, Some(project));
            for id in [task, attempt, env.report.report_id] {
                assert!(body.contains(&id.to_string()));
            }
            assert!(body.contains("lineage_time_budget_exhausted"));
            count.fetch_add(1, Ordering::SeqCst);
            Ok::<_, ()>(Uuid::new_v4())
        };
        scheduling_notices::notify(&pool, NOW as i64, &send)
            .await
            .unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 0);
        for _ in 0..2 {
            scheduling_notices::notify(&pool, NOW as i64 + 600_001, &send)
                .await
                .unwrap();
        }
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_physically_stopped_task_without_a_result_escalates_once_after_contention_clears() {
        let (f, _, report) = Box::pin(prepare(true, true)).await;
        let pool = f.db.pool().await;
        let project = f.selection.lineage.project_id;
        let attempt = report.binding.attempt_id;
        sqlx::query("UPDATE supervisor_controls SET enabled=true, profile='{}',approved_by=$2 WHERE conversation_id=(SELECT conversation_id FROM supervisor_conversation_ids WHERE project_id=$1)")
            .bind(project).bind(f.actor).execute(&pool).await.unwrap();
        let stopped: i64 = sqlx::query_scalar("SELECT (convert_from(receipt_bytes,'UTF8')::jsonb->>'recorded_at_ms')::bigint FROM profiled_stopped_receipts WHERE attempt_id=$1")
            .bind(attempt).fetch_one(&pool).await.unwrap();
        let count = AtomicUsize::new(0);
        let send = async |_: &mut sqlx::Transaction<'_, sqlx::Postgres>,
                          scope: Option<Uuid>,
                          body: &str,
                          _: u64| {
            assert_eq!(scope, Some(project));
            assert!(body.contains("stopped_without_result") && body.contains(&attempt.to_string()));
            assert!(!body.contains("submit task.retry"));
            count.fetch_add(1, Ordering::SeqCst);
            Ok::<_, ()>(Uuid::new_v4())
        };
        use aq_playbook_domain::scheduling_notices::notify;
        notify(&pool, stopped + 599_999, &send).await.unwrap();
        let mut busy = pool.begin().await.unwrap();
        sqlx::query("SELECT attempt_id FROM execution_attempts WHERE attempt_id=$1 FOR UPDATE")
            .bind(attempt)
            .execute(&mut *busy)
            .await
            .unwrap();
        notify(&pool, stopped + 600_000, &send).await.unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 0);
        busy.rollback().await.unwrap();
        for _ in 0..2 {
            notify(&pool, stopped + 660_000, &send).await.unwrap();
        }
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn repeated_publication_refusals_notify_once_for_the_current_unpublished_head() {
        use aq_playbook_domain::scheduling_notices::notify;
        let env = Box::pin(auto_env("not_required", axum::http::StatusCode::OK)).await;
        let pool = env.f.db.pool().await;
        let lineage = &env.f.selection.lineage;
        let task = lineage.task_id;
        let attempt = env.report.binding.attempt_id;
        let result = env.report.report_id;
        let head = "4".repeat(40);
        sqlx::query("UPDATE supervisor_controls c SET enabled=true,profile='{}',approved_by=$2 FROM supervisor_conversation_ids i WHERE i.conversation_id=c.conversation_id AND i.project_id=$1")
            .bind(lineage.project_id).bind(env.f.actor).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO delivery_pushed_results(report_id,attempt_id,task_ref,head) VALUES($1,$2,$3,$4) ON CONFLICT(report_id) DO UPDATE SET head=EXCLUDED.head")
            .bind(result).bind(attempt).bind(format!("refs/heads/aq/{task}")).bind(&head).execute(&pool).await.unwrap();
        let ns = Uuid::new_v4();
        sqlx::query("INSERT INTO playbook_namespaces(namespace_id,generation,name,workspace_id,project_id,owner_user_id,catalog_digest,source_digest) VALUES($1,1,'publish',$2,$3,$4,repeat('a',64),repeat('b',64))")
            .bind(ns).bind(lineage.workspace_id).bind(lineage.project_id).bind(env.f.actor).execute(&pool).await.unwrap();
        let record = async |sha: &str, report: Uuid, status: &str| {
            let id = Uuid::new_v4();
            let request =
                json!({"result_ready":{"result_id":report,"attempt_id":attempt},"result_head":sha});
            let output = if status == "published" {
                json!({"head":sha})
            } else {
                json!({"reason":"unreachable"})
            };
            sqlx::query("INSERT INTO playbook_host_operations(operation_id,namespace_id,generation,intent_id,command_name,subject_id,subject_revision,request,status,output,created_at_ms,completed_at_ms) VALUES($1,$2,1,$3,'branch.publish',$4,0,$5,$6,$7,$8,$8)")
                .bind(id).bind(ns).bind(id.to_string()).bind(task.to_string()).bind(request).bind(status).bind(output).bind(NOW as i64).execute(&pool).await.unwrap();
            id
        };
        let calls = AtomicUsize::new(0);
        let send =
            async |_: &mut sqlx::Transaction<'_, sqlx::Postgres>, scope, body: &str, _: u64| {
                assert_eq!(scope, Some(lineage.project_id));
                assert!(
                    body.contains("branch_publication_refused")
                        && body.contains(&result.to_string())
                        && body.contains(&attempt.to_string())
                        && body.contains(&head)
                        && body.contains("rejection from the remote")
                );
                calls.fetch_add(1, Ordering::SeqCst);
                Ok::<_, ()>(Uuid::new_v4())
            };
        let due = NOW as i64 + 600_000;
        record(&head, result, "refused").await;
        record(&"5".repeat(40), result, "refused").await;
        record(&head, Uuid::new_v4(), "refused").await;
        notify(&pool, due, &send).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let known_refusal = record(&head, result, "refused").await;
        // Simulate a newer publisher alongside the older, catalog-only receipt.
        sqlx::query("UPDATE playbook_host_operations SET request=request || '{\"answer_note\":\"remote_rejected\"}'::jsonb,completed_at_ms=completed_at_ms+1 WHERE operation_id=$1")
            .bind(known_refusal).execute(&pool).await.unwrap();
        let detail = aq_playbook_domain::delivery_store::publication_refusal(
            &mut *pool.acquire().await.unwrap(),
            result,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(detail.contains(&known_refusal.to_string()));
        notify(&pool, due - 1, &send).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        for _ in 0..2 {
            notify(&pool, due, &send).await.unwrap();
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        sqlx::query("DELETE FROM delivery_notifications WHERE project_id=$1")
            .bind(lineage.project_id)
            .execute(&pool)
            .await
            .unwrap();
        record(&head, result, "published").await;
        notify(&pool, due, &send).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(
            aq_playbook_domain::delivery_store::publication_refusal(
                &mut *pool.acquire().await.unwrap(),
                result
            )
            .await
            .unwrap()
            .is_none()
        );
        assert!(env.holds_task().await);
    }

    #[tokio::test]
    async fn expired_delivery_notice_reaches_the_project_once_for_the_current_held_result() {
        use aq_playbook_domain::{delivery_decisions, scheduling_notices};
        let env = Box::pin(auto_env("not_required", axum::http::StatusCode::OK)).await;
        let pool = env.f.db.pool().await;
        let task = env.f.selection.lineage.task_id;
        let project = env.f.selection.lineage.project_id;
        let result = env.report.report_id;
        sqlx::query("UPDATE supervisor_controls c SET enabled=true,profile='{}',approved_by=$2 FROM supervisor_conversation_ids i WHERE i.conversation_id=c.conversation_id AND i.project_id=$1")
            .bind(project).bind(env.f.actor).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO delivery_decisions(result_id,task_id,decision,reason,decided_at_ms) VALUES ($1,$2,'held','returns_exhausted',$3)")
            .bind(result).bind(task).bind(NOW as i64).execute(&pool).await.unwrap();
        let reasons = work_reasons(&env).await;
        assert!(reasons.iter().any(|r| r["code"] == "task_held"
            && r["hold_reason"] == "returns_exhausted"
            && r["message"].as_str().unwrap().contains(&result.to_string())));
        let key = delivery_decisions::notify_key(&format!("{result}:returns_exhausted")).unwrap();
        let calls = AtomicUsize::new(0);
        let send = async |_: &mut sqlx::Transaction<'_, sqlx::Postgres>,
                          scope: Option<Uuid>,
                          body: &str,
                          _: u64| {
            assert_eq!(scope, Some(project));
            assert!(
                body.contains(&task.to_string())
                    && body.contains(&result.to_string())
                    && body.contains("task.retry")
            );
            calls.fetch_add(1, Ordering::SeqCst);
            Ok::<_, ()>(Uuid::new_v4())
        };
        scheduling_notices::notify(&pool, NOW as i64, &send)
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        for _ in 0..2 {
            scheduling_notices::notify(&pool, NOW as i64 + 600_001, &send)
                .await
                .unwrap();
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(
            delivery_decisions::told(&mut pool.acquire().await.unwrap(), project, &key)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn independent_review_reads_the_assigned_result_revision_and_retained_commit() {
        use aq_playbook_domain::delivery_store::{self, DeliveryPort, PgDelivery};
        let target = Box::pin(super::auto_env("not_required", axum::http::StatusCode::OK)).await;
        let pool = target.f.db.pool().await;
        let id = target.report.report_id;
        let pin: String = sqlx::query_scalar(
            "SELECT report_revision FROM result_revision_pins WHERE report_id=$1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        let lineage = aq_selection::TaskLineage {
            task_id: Uuid::new_v4(),
            ..target.f.selection.lineage.clone()
        };
        let mut f = Box::pin(common::profiled::fixture_on(
            target.f.db.clone(),
            Some((target.f.actor, lineage.clone())),
        ))
        .await;
        f.state.config = target.f.state.config.clone();
        let (f, _, mut report) = Box::pin(prepare_on(
            f,
            true,
            true,
            None,
            false,
            Some(default_policy("not_required")),
            true,
        ))
        .await;
        let signed = format!(
            "Outcome: done; Changes: none; Copied: none; Claude exit 0; Result: Review verdict: changes_requested {id}@{pin} {} Unit checks exposed a defect.",
            target.checkpoint
        );
        report.conclusion = Label::new(signed).unwrap();
        let reviewer = Box::pin(finish_auto_env(
            f,
            report,
            axum::http::StatusCode::OK,
            false,
            true,
        ))
        .await;
        let latest = PgDelivery(pool.clone())
            .latest(lineage.task_id)
            .await
            .unwrap()
            .unwrap();
        let version = reviewer.expected_version;
        let graph = json!({"version":version,"labels":["review-only"],"body":format!("Review target: {id}@{pin}")});
        let observed = |node: Value| {
            let pool = pool.clone();
            let latest = latest.clone();
            let lineage = lineage.clone();
            async move {
                let mut conn = pool.acquire().await.unwrap();
                delivery_store::observed(
                    &mut conn,
                    lineage.task_id,
                    (lineage.workspace_id, lineage.project_id),
                    Some(&latest),
                    &node,
                    (false, 0, 1),
                )
                .await
                .unwrap()["facts"]
                    .clone()
            }
        };
        let facts = observed(graph.clone()).await;
        assert_eq!(facts["kind"], "review-only");
        assert_eq!(facts["outcome"], "review_changes_requested");
        let mut moved = graph.clone();
        moved["version"] = json!(version + 1);
        assert_eq!(observed(moved).await["outcome"], "review_incomplete");
        let mut wrong = graph.clone();
        wrong["body"] = json!(format!("Review target: {id}@{}", "0".repeat(64)));
        assert_eq!(observed(wrong).await["outcome"], "review_incomplete");
        let mut unmarked = graph;
        unmarked["labels"] = json!([]);
        assert_eq!(observed(unmarked).await["outcome"], "success");
    }

    #[tokio::test]
    async fn two_projects_keep_distinct_result_paths_after_enrollment_changes() {
        use aq_playbook_domain::delivery_store::{self, DeliveryPort, PgDelivery};
        let mut legacy = super::auto_env("not_required", axum::http::StatusCode::OK).await;
        let lineage = aq_selection::TaskLineage {
            project_id: Uuid::new_v4(),
            workspace_id: Uuid::new_v4(),
            task_id: Uuid::new_v4(),
        };
        let mut f = common::profiled::fixture_on(
            legacy.f.db.clone(),
            Some((legacy.f.actor, lineage.clone())),
        )
        .await;
        f.state.config = legacy.f.state.config.clone();
        let (f, _, report) = Box::pin(prepare_on(
            f,
            true,
            true,
            None,
            false,
            Some(default_policy("not_required")),
            true,
        ))
        .await;
        let mut enrolled = finish_auto_env(f, report, axum::http::StatusCode::OK, true, true).await;
        // An in-flight legacy attempt's project is now enabled. The project's
        // earlier enrollment for the other attempt was removed.
        let mut config = (*legacy.f.state.config).clone();
        config.delivery = Some(aq_delivery::github::DeliveryConfig {
            project_allowlist: [legacy.f.selection.lineage.project_id].into(),
            git_http_socket: Some(
                enrolled
                    .f
                    .state
                    .source_retention
                    .as_ref()
                    .unwrap()
                    .config
                    .git_socket
                    .clone(),
            ),
            ..Default::default()
        });
        let config = Arc::new(config);
        legacy.f.state.config = config.clone();
        enrolled.f.state.config = config;

        let pool = legacy.f.db.pool().await;
        let port = PgDelivery(pool.clone());
        let pending = port.pending(None, 8).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].project_id, lineage.project_id);
        assert_eq!(pending[0].result_id, enrolled.report.report_id);
        assert!(
            port.latest(legacy.f.selection.lineage.task_id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            delivery_store::undecided(
                &mut *pool.acquire().await.unwrap(),
                legacy.report.binding.attempt_id
            )
            .await
            .unwrap()
            .is_none()
        );
        assert!(
            complete_by_playbook(&legacy.f.state, legacy.report.report_id)
                .await
                .is_err()
        );
        assert!(
            settlement::settle_after_record(&enrolled.f.state, enrolled.report.report_id)
                .await
                .unwrap()
                .is_none()
        );

        legacy
            .check(
                legacy.source,
                &legacy.checkpoint,
                &legacy.validator,
                "passed",
            )
            .await;
        // Replaying the report drives the frozen legacy policy to completion.
        results::record_pushed(
            &legacy.f.state,
            &context(&legacy.f).await,
            &legacy.report,
            &Default::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            settlement_row(&legacy.f, legacy.report.report_id).await.0,
            "confirmed"
        );
        assert_eq!(legacy.writes.load(Ordering::SeqCst), 1);
        assert!(
            result_facts(&legacy.f, legacy.report.report_id)
                .await
                .is_none()
        );

        // The enrolled attempt replays its original pushed facts after the
        // config switch and waits for its own completion decision.
        results::record_pushed(
            &enrolled.f.state,
            &context(&enrolled.f).await,
            &enrolled.report,
            &Default::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            settlement_rows(&enrolled.f, enrolled.report.report_id).await,
            0
        );
        assert_eq!(
            result_facts(&enrolled.f, enrolled.report.report_id)
                .await
                .unwrap()["source"]["checkpoint"],
            enrolled.checkpoint
        );
        assert_eq!(complete(&enrolled).await, Settled::Completed);
        assert_eq!(enrolled.writes.load(Ordering::SeqCst), 1);
        assert!(port.pending(None, 8).await.unwrap().is_empty());
    }

    async fn complete(env: &AutoEnv) -> Settled {
        complete_by_playbook(&env.f.state, env.report.report_id)
            .await
            .unwrap()
    }
    async fn send_back(env: &AutoEnv, reason: &str) -> Settled {
        return_by_playbook(&env.f.state, env.report.report_id, reason)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn work_complete_completes_on_the_graph_receipt_and_releases_the_claim_once() {
        // No check or delivery evidence: only the playbook's decision completes.
        // The release guards (migrations 0059, 0066) accept its receipt.
        let env = auto_env("required", axum::http::StatusCode::OK).await;
        assert!(env.holds_task().await);
        assert_eq!(complete(&env).await, Settled::Completed);
        assert_eq!(env.writes.load(Ordering::SeqCst), 1);
        {
            let bodies = env.bodies.lock().unwrap();
            let operation = &bodies[0]["operations"][0];
            assert_eq!(operation["patch"], json!({"task_state":"completed"}));
            assert_eq!(operation["expected_version"], json!(env.expected_version));
            assert_eq!(
                bodies[0]["reason"],
                format!("Playbook accepted retained result {}", env.report.report_id)
            );
        }
        assert!(
            !env.holds_task().await,
            "the validated receipt releases the claim"
        );
        assert_eq!(
            settlement_row(&env.f, env.report.report_id).await.0,
            "confirmed"
        );
        // A replay answers the same and writes nothing; a return is refused.
        assert_eq!(complete(&env).await, Settled::Completed);
        assert_eq!(send_back(&env, "policy").await, Settled::Stale);
        assert_eq!(env.writes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_graph_version_conflict_is_stale_and_never_dispatched_again() {
        let env = auto_env("not_required", axum::http::StatusCode::CONFLICT).await;
        assert_eq!(complete(&env).await, Settled::Stale);
        let row = settlement_row(&env.f, env.report.report_id).await;
        assert_eq!(
            (row.0.as_str(), row.1.as_deref()),
            ("held", Some("graph_version_conflict"))
        );
        assert_eq!(complete(&env).await, Settled::Stale);
        assert_eq!(env.writes.load(Ordering::SeqCst), 1);
        assert!(env.holds_task().await);
    }

    #[tokio::test]
    async fn a_superseded_report_is_never_completed_or_returned() {
        let env = auto_env("not_required", axum::http::StatusCode::OK).await;
        let auth = context(&env.f).await;
        let mut correction = env.report.clone();
        correction.report_id = Uuid::new_v4();
        correction.operation_id = Uuid::new_v4();
        correction.supersedes = Some(env.report.report_id);
        results::record(&env.f.state, &auth, &correction)
            .await
            .unwrap();
        assert_eq!(complete(&env).await, Settled::Stale);
        assert_eq!(send_back(&env, "policy").await, Settled::Stale);
        assert_eq!(env.writes.load(Ordering::SeqCst), 0);
        assert!(env.holds_task().await);
        // Recording decided nothing and a stale decision writes no row.
        assert_eq!(settlement_rows(&env.f, env.report.report_id).await, 0);
    }

    async fn held_retry_env(outcome: aq_admission::transport::ProcessOutcome) -> AutoEnv {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../../../protocol/agent-access/v1/fixtures/mutation-delivery.json"
        ))
        .unwrap();
        let profile = fixture["proposal_profiles"][0]["profile"].clone();
        let (f, _, mut report) = Box::pin(prepare_with_policy_checks(
            true,
            true,
            Some(profile.clone()),
            false,
            Some(default_policy("not_required")),
            false,
        ))
        .await;
        report.outcome = outcome;
        let env = finish_auto_env(f, report, axum::http::StatusCode::OK, true, true).await;
        seed_agent_binding(&env, &profile).await;
        env
    }

    #[tokio::test]
    async fn owner_retry_proves_the_pushed_source_and_effects_of_a_failed_policy_hold() {
        let env = Box::pin(held_retry_env(
            aq_admission::transport::ProcessOutcome::Failed,
        ))
        .await;
        let task = env.f.selection.lineage.task_id;
        let pool = env.f.db.pool().await;
        let facts = || async {
            sqlx::query_as::<_,(String,String)>("SELECT state#>>'{retention,kind}',state#>>'{effects,kind}' FROM profiled_authority_hot WHERE attempt_id=$1")
                .bind(env.report.binding.attempt_id).fetch_one(&pool).await.unwrap()
        };
        settlement::recover_held_for_retry(&env.f.state, task)
            .await
            .unwrap();
        assert_eq!(facts().await, ("pending".into(), "unknown".into()));
        sqlx::query("INSERT INTO delivery_decisions(result_id,task_id,decision,reason,decided_at_ms) VALUES($1,$2,'held','policy',$3)")
            .bind(env.report.report_id).bind(task).bind(NOW as i64).execute(&pool).await.unwrap();
        for _ in 0..2 {
            settlement::recover_held_for_retry(&env.f.state, task)
                .await
                .unwrap();
            assert_eq!(facts().await, ("retained".into(), "quiescent".into()));
        }
        assert_eq!(env.writes.load(Ordering::SeqCst), 0);
        assert!(env.holds_task().await);
    }

    #[tokio::test]
    async fn exhausted_success_retry_requires_a_final_pr_failure_for_this_exact_result() {
        use aq_delivery::git_grant::held_report;
        let env = Box::pin(held_retry_env(
            aq_admission::transport::ProcessOutcome::Success,
        ))
        .await;
        let pool = env.f.db.pool().await;
        let lineage = &env.f.selection.lineage;
        let (task, attempt, result) = (
            lineage.task_id,
            env.report.binding.attempt_id,
            env.report.report_id,
        );
        let ns = Uuid::new_v4();
        sqlx::query("INSERT INTO playbook_namespaces(namespace_id,generation,name,workspace_id,project_id,owner_user_id,catalog_digest,source_digest) VALUES($1,1,'retry',$2,$3,$4,repeat('a',64),repeat('b',64))")
            .bind(ns).bind(lineage.workspace_id).bind(lineage.project_id).bind(env.f.actor).execute(&pool).await.unwrap();
        let record = async |command: &str, status: &str, report: Uuid| {
            let id = Uuid::new_v4();
            sqlx::query("INSERT INTO playbook_host_operations(operation_id,namespace_id,generation,intent_id,command_name,subject_id,subject_revision,request,status,output,created_at_ms) VALUES($1,$2,1,$3,$4,$5,0,$6,$7,CASE WHEN $7='running' THEN NULL ELSE '{}'::jsonb END,$8)")
                .bind(id).bind(ns).bind(id.to_string()).bind(command).bind(task.to_string())
                .bind(json!({"result_ready":{"result_id":report,"attempt_id":attempt},"result_head":"4".repeat(40),"pull_request":228}))
                .bind(status).bind(NOW as i64).execute(&pool).await.unwrap();
            id
        };
        let check = record("pull_request.await_checks", "running", result).await;
        let reasons = work_reasons(&env).await;
        assert!(reasons.iter().any(|r| {
            r["code"] == "result_ready_no_delivery_policy"
                && r["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("PR #228") && m.contains(&"4".repeat(40)))
        }));
        sqlx::query("UPDATE playbook_host_operations SET status='passed',output='{}',completed_at_ms=$2 WHERE operation_id=$1")
            .bind(check).bind(NOW as i64).execute(&pool).await.unwrap();
        assert!(
            !work_reasons(&env)
                .await
                .iter()
                .any(|r| r["message"].as_str().is_some_and(|m| m.contains("PR #228")))
        );
        sqlx::query("INSERT INTO delivery_decisions(result_id,task_id,decision,reason,decided_at_ms) VALUES($1,$2,'held','returns_exhausted',$3)")
            .bind(result).bind(task).bind(NOW as i64).execute(&pool).await.unwrap();
        record("pull_request.await_checks", "conflict", Uuid::new_v4()).await;
        assert_eq!(held_report(&pool, task).await.unwrap(), None);
        record("pull_request.await_checks", "conflict", result).await;
        let reasons = work_reasons(&env).await;
        assert!(
            reasons.iter().any(|r| r["message"]
                .as_str()
                .is_some_and(|text| text.contains("pull_request.await_checks")
                    && text.contains(&"4".repeat(40))))
        );
        let merge = record("pull_request.merge", "running", result).await;
        assert_eq!(held_report(&pool, task).await.unwrap(), None);
        sqlx::query("UPDATE playbook_host_operations SET status='conflict',output='{}',completed_at_ms=$2 WHERE operation_id=$1")
            .bind(merge).bind(NOW as i64).execute(&pool).await.unwrap();
        assert_eq!(held_report(&pool, task).await.unwrap(), Some(result));
        settlement::recover_held_for_retry(&env.f.state, task)
            .await
            .unwrap();
        let facts: (String,String)=sqlx::query_as("SELECT state#>>'{retention,kind}',state#>>'{effects,kind}' FROM profiled_authority_hot WHERE attempt_id=$1")
            .bind(attempt).fetch_one(&pool).await.unwrap();
        assert_eq!(facts, ("retained".into(), "quiescent".into()));
        assert!(env.holds_task().await);
        assert_eq!(env.writes.load(Ordering::SeqCst), 0);

        // A return followed by a budget hold also carries the original PR
        // evidence, allowing a supervisor to compare independently recorded heads.
        sqlx::query("UPDATE delivery_decisions SET decision='returned',reason='merge_conflict',update_from_main=true WHERE result_id=$1")
            .bind(result).execute(&pool).await.unwrap();
        sqlx::query("UPDATE supervisor_controls c SET enabled=true,profile='{}',approved_by=$2 FROM supervisor_conversation_ids i WHERE i.conversation_id=c.conversation_id AND i.project_id=$1")
            .bind(lineage.project_id).bind(env.f.actor).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO task_holds(comment_id,task_id,attempts,project_id,reason,attempt_id,comment_body,held_at) VALUES($1,$2,1,$3,'task_sandbox_minutes_budget_exhausted',$4,'returned',to_timestamp($5))")
            .bind(Uuid::new_v4()).bind(task).bind(lineage.project_id).bind(attempt).bind(NOW as f64 / 1000.0).execute(&pool).await.unwrap();
        let reasons = work_reasons(&env).await;
        let evidence = format!("returned result {result}. Worker outcome: success.");
        assert!(reasons.iter().any(|r| {
            r["message"]
                .as_str()
                .is_some_and(|text| text.contains(&evidence) && text.contains(&"4".repeat(40)))
        }));
        assert!(
            aq_playbook_domain::delivery_store::returned_result_evidence(
                &mut pool.acquire().await.unwrap(),
                Uuid::new_v4(),
                task,
                attempt
            )
            .await
            .unwrap()
            .is_none()
        );
        let calls = AtomicUsize::new(0);
        aq_playbook_domain::scheduling_notices::notify(
            &pool,
            NOW as i64 + 600_001,
            async |_, scope, body: &str, _| {
                assert_eq!(scope, Some(lineage.project_id));
                assert!(
                    body.contains("task_sandbox_minutes_budget_exhausted")
                        && body.contains(&"4".repeat(40))
                        && body.contains("conflict")
                        && body.contains(&evidence)
                );
                calls.fetch_add(1, Ordering::SeqCst);
                Ok::<_, ()>(Uuid::new_v4())
            },
        )
        .await
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_cancelled_attempt_is_never_completed_or_returned() {
        let env = auto_env("not_required", axum::http::StatusCode::OK).await;
        aq_server::profiled_execution::cancellation::cancel(
            &env.f.state,
            env.report.binding.attempt_id,
            Uuid::new_v4(),
            "owner cancel before the playbook decides",
        )
        .await
        .unwrap();
        assert_eq!(complete(&env).await, Settled::Stale);
        assert_eq!(send_back(&env, "policy").await, Settled::Stale);
        assert_eq!(env.writes.load(Ordering::SeqCst), 0);
        assert!(env.holds_task().await);
    }

    #[tokio::test]
    async fn work_return_reworks_the_result_once_and_leaves_the_claim_to_the_replacement() {
        let env = auto_env("not_required", axum::http::StatusCode::OK).await;
        assert_eq!(
            send_back(&env, "delivery_unpublished").await,
            Settled::Returned
        );
        let row = settlement_row(&env.f, env.report.report_id).await;
        assert_eq!(
            (row.0.as_str(), row.1.as_deref()),
            ("rework_required", Some("delivery_unpublished"))
        );
        let evidence: Value =
            sqlx::query_scalar("SELECT evidence FROM profiled_auto_settlements WHERE report_id=$1")
                .bind(env.report.report_id)
                .fetch_one(&env.f.db.pool().await)
                .await
                .unwrap();
        assert_eq!(
            (&evidence["decided_by"], &evidence["decision"]),
            (&json!("playbook"), &json!("work.return"))
        );
        // No Graph write, and the claim stays until the replacement owner releases it.
        assert_eq!(env.writes.load(Ordering::SeqCst), 0);
        assert!(env.holds_task().await);
        let mut db = env.f.db.pool().await.acquire().await.unwrap();
        let task = env.f.selection.lineage.task_id;
        assert!(
            !aq_profiled_state::replacement_sql::delivery_unpublished(&mut db, task)
                .await
                .unwrap(),
            "a playbook return does not trigger the legacy D1 hold"
        );

        // The replacement facts the first call could not prove are swept later.
        let swept = settlement::reworked_without_facts(&env.f.state, None, 8).await;
        assert!(swept.unwrap().contains(&env.report.report_id));
        // A replay answers the same; the result is no longer completed, and a
        // duplicate record of the report keeps the rework.
        assert_eq!(send_back(&env, "policy").await, Settled::Returned);
        assert_eq!(complete(&env).await, Settled::Stale);
        let auth = context(&env.f).await;
        results::record(&env.f.state, &auth, &env.report)
            .await
            .unwrap();
        let row = settlement_row(&env.f, env.report.report_id).await;
        assert_eq!(
            (row.0.as_str(), row.1.as_deref()),
            ("rework_required", Some("delivery_unpublished"))
        );
        assert_eq!(env.writes.load(Ordering::SeqCst), 0);
    }

    /// Spec 0016 section 7: recording a result stores its facts and decides
    /// nothing. With its source retained and nothing else to wait for, the
    /// result is neither completed, published nor sent back, and the claim
    /// cannot be released without a Graph completion receipt.
    #[tokio::test]
    async fn a_recorded_result_waits_with_its_facts_and_no_verdict() {
        let env = auto_env("required", axum::http::StatusCode::OK).await;
        // The duplicate record, now that the source is retained, replaces the facts.
        let auth = context(&env.f).await;
        results::record(&env.f.state, &auth, &env.report)
            .await
            .unwrap();
        let facts = result_facts(&env.f, env.report.report_id)
            .await
            .expect("facts recorded");
        assert_eq!(facts["source"]["operation_id"], json!(env.source));
        assert_eq!(facts["source"]["checkpoint"], json!(env.checkpoint));
        assert_eq!(facts["source"]["changed"], json!(true));
        assert_eq!(facts["outcome"], json!(env.report.outcome));
        assert_eq!(
            facts["accepted"],
            json!({"current_report_head": true, "stopped": true, "cancelled": false, "revoked": false, "holds_task": true})
        );
        assert_eq!(facts["delivery"]["state"], json!("missing"));
        assert!(facts.get("policy_actor").is_none() && facts.get("verdict").is_none());
        // No verdict, publication or completion.
        assert_eq!(settlement_rows(&env.f, env.report.report_id).await, 0);
        assert!(evidence_rows(&env).await.is_empty(), "nothing published");
        assert_eq!(env.writes.load(Ordering::SeqCst), 0);
        assert!(env.holds_task().await);
        // The release guard still refuses a claim release without a receipt.
        let released =
            sqlx::query("UPDATE execution_attempts SET holds_task=false WHERE attempt_id=$1")
                .bind(env.report.binding.attempt_id)
                .execute(&env.f.db.pool().await)
                .await;
        assert!(released.is_err(), "the claim stays until a Graph receipt");
        // The playbook decides it.
        assert_eq!(complete(&env).await, Settled::Completed);
        assert!(!env.holds_task().await);
    }

    /// The reasons of the task's work read (`GET .../tasks/{task}/work`),
    /// checked against the human-graph-1 schema.
    async fn work_reasons(env: &AutoEnv) -> Vec<Value> {
        use http_body_util::BodyExt;
        use tower::ServiceExt;
        let lineage = &env.f.selection.lineage;
        let app =
            aq_server::agent_access::human::routes(&env.f.state).with_state(env.f.state.clone());
        let uri = format!(
            "/api/human/projects/{}/tasks/{}/work",
            lineage.project_id, lineage.task_id
        );
        let request = axum::http::Request::get(uri)
            .header("x-aq-dev-email", "owner@localhost")
            .body(axum::body::Body::empty())
            .unwrap();
        let res = app.oneshot(request).await.unwrap();
        let status = res.status();
        let body: Value =
            serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(status, 200, "{body}");
        let schema: Value = serde_json::from_str(include_str!(
            "../../../../../protocol/human-graph/v1/schema.json"
        ))
        .unwrap();
        assert!(
            aq_agent_api::check(&schema, &schema, &body),
            "schema: {body}"
        );
        assert_eq!(body["data"]["readiness"]["state"], "held", "{body}");
        body["data"]["readiness"]["reasons"]
            .as_array()
            .unwrap()
            .clone()
    }

    /// Spec 0016 section 7 and acceptance 6: with no delivery playbook active
    /// the result waits, and the task read says "result ready, no delivery
    /// policy"; a decided result is no longer listed as waiting.
    #[tokio::test]
    async fn the_task_read_says_a_result_waits_without_a_delivery_policy() {
        let env = auto_env("required", axum::http::StatusCode::OK).await;
        let reasons = work_reasons(&env).await;
        let waiting = (reasons.iter())
            .find(|r| r["code"] == "result_ready_no_delivery_policy")
            .unwrap_or_else(|| panic!("{reasons:?}"));
        assert_eq!(waiting["result_id"], json!(env.report.report_id));
        assert_eq!(waiting["attempt_id"], json!(env.report.binding.attempt_id));
        let message = waiting["message"].as_str().unwrap();
        assert!(message.starts_with("Result ready, no delivery policy"));
        // A playbook's decision ends the wait (here it sends the result back).
        assert_eq!(send_back(&env, "checks_failed").await, Settled::Returned);
        let reasons = work_reasons(&env).await;
        assert!(
            reasons
                .iter()
                .all(|r| r["code"] != "result_ready_no_delivery_policy"),
            "{reasons:?}"
        );
    }

    /// The conclusion-text heuristics are gone: a failed report whose agent
    /// said it was blocked, or a success that claims changes that did not
    /// land, waits like any other result instead of being reworked.
    #[tokio::test]
    async fn conclusion_text_never_reworks_a_result() {
        use aq_admission::transport::ProcessOutcome;
        for (outcome, changed, conclusion) in [
            (
                ProcessOutcome::Failed,
                false,
                "Outcome: blocked; Changes: none; Copied: none; Claude exit 0; Result: no access to the repo",
            ),
            (
                ProcessOutcome::Success,
                false,
                "Changes: src/lib.rs; Checks: cargo test",
            ),
        ] {
            let (f, _, mut report) = Box::pin(prepare_with_policy_checks(
                true,
                true,
                None,
                false,
                Some(default_policy("not_required")),
                false,
            ))
            .await;
            report.outcome = outcome;
            report.conclusion = Label::new(conclusion).unwrap();
            let env = finish_auto_env(f, report, axum::http::StatusCode::OK, changed, true).await;
            let auth = context(&env.f).await;
            results::record(&env.f.state, &auth, &env.report)
                .await
                .unwrap();
            assert_eq!(settlement_rows(&env.f, env.report.report_id).await, 0);
            let facts = result_facts(&env.f, env.report.report_id).await.unwrap();
            assert_eq!(facts["outcome"], json!(outcome));
            assert_eq!(facts["source"]["changed"], json!(false));
            assert_eq!(env.writes.load(Ordering::SeqCst), 0);
            assert!(env.holds_task().await);
        }
    }
}
