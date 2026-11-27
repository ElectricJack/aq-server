//! D3 automatic default-policy settlement with a SYNTHETIC Graph peer and a
//! synthetic retained-source owner row. Check and delivery evidence rows are
//! the server-owned D6 and D1a/D1b seams, written here as those owners would.
use super::*;
use aq_server::profiled_execution::settlement::{self, AutoDisposition, AutoSettlement};
use axum::{Json, Router, routing::post};
use serde_json::Value;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicI64, AtomicU16, AtomicUsize},
};

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

struct GraphRecovery {
    write_status: AtomicU16,
    lookup_status: AtomicU16,
    has_receipt: AtomicBool,
    lookups: Mutex<Vec<Value>>,
    active: Mutex<Value>,
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
    recovery: Arc<GraphRecovery>,
    _directory: tempfile::TempDir,
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
    Box::pin(auto_env_source(
        delivery,
        graph_status,
        ReportSetup {
            keep_checks,
            process_outcome,
            ..ReportSetup::default()
        },
    ))
    .await
}

/// The policy inputs and result reported by the synthetic attempt.
pub(crate) struct ReportSetup<'a> {
    pub(crate) keep_checks: bool,
    pub(crate) process_outcome: Option<aq_admission::transport::ProcessOutcome>,
    pub(crate) graph_profile: Option<Value>,
    /// A changed checkpoint is a new commit on the admitted base.
    pub(crate) changed: bool,
    pub(crate) conclusion: Option<&'a str>,
}
impl Default for ReportSetup<'_> {
    fn default() -> Self {
        Self {
            keep_checks: true,
            process_outcome: None,
            graph_profile: None,
            changed: true,
            conclusion: None,
        }
    }
}
pub(crate) async fn auto_env_source(
    delivery: &str,
    graph_status: axum::http::StatusCode,
    setup: ReportSetup<'_>,
) -> AutoEnv {
    let ReportSetup {
        keep_checks,
        process_outcome,
        graph_profile,
        changed,
        conclusion,
    } = setup;
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
    finish_auto_env(f, report, graph_status, ReportSource::Legacy { changed }).await
}

enum ReportSource {
    Legacy {
        changed: bool,
    },
    #[cfg(feature = "delivery_playbook")]
    Pushed {
        changed: bool,
    },
}
impl ReportSource {
    fn changed(&self) -> bool {
        match self {
            Self::Legacy { changed } => *changed,
            #[cfg(feature = "delivery_playbook")]
            Self::Pushed { changed } => *changed,
        }
    }
}
async fn finish_auto_env(
    mut f: Fixture,
    mut report: ResultReport,
    graph_status: axum::http::StatusCode,
    report_source: ReportSource,
) -> AutoEnv {
    let changed = report_source.changed();
    let auth = context(&f).await;
<<<<<<< HEAD
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
=======
    #[cfg(feature = "delivery_playbook")]
    let task_ref = match report_source {
        ReportSource::Pushed { changed } => {
            Some(record_push(&mut f, &mut report, &auth, changed).await)
>>>>>>> refs/remotes/central/main
        }
        ReportSource::Legacy { .. } => None,
    };
    let source = match report_source {
        ReportSource::Legacy { .. } => record_legacy(&f, &mut report, &auth).await,
        #[cfg(feature = "delivery_playbook")]
        ReportSource::Pushed { .. } => report.report_id,
    };

    let checkpoint = retained_checkpoint(&f, &report, source, changed).await;
    let pool = &f.db.pool().await;
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

    let graph = GraphPeer::install(&mut f, expected_version, graph_status).await;
    AutoEnv {
        checkpoint,
        f,
        report,
        source,
        check_id,
        validator,
        expected_version,
        writes: graph.writes,
        bodies: graph.bodies,
        recovery: graph.recovery,
        _directory: graph.directory,
        #[cfg(feature = "delivery_playbook")]
        _task_ref: task_ref,
    }
}

#[cfg(feature = "delivery_playbook")]
async fn record_push(
    f: &mut Fixture,
    report: &mut ResultReport,
    auth: &aq_server::worker_api::AuthenticatedNode,
    changed: bool,
) -> super::pushed_ref::TaskRef {
    let task_ref = super::pushed_ref::task_ref(f, report).await;
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
    results::record_pushed(&f.state, auth, report, &headers)
        .await
        .unwrap();
    assert_eq!(settlement_rows(f, report.report_id).await, 0);
    let facts = result_facts(f, report.report_id)
        .await
        .expect("facts recorded");
    assert_eq!(facts["source"]["changed"], json!(changed));
    task_ref
}
async fn record_legacy(
    f: &Fixture,
    report: &mut ResultReport,
    auth: &aq_server::worker_api::AuthenticatedNode,
) -> Uuid {
    let source = Uuid::new_v4();
    report.source_receipt_ids.push(source);
    results::record(&f.state, auth, report).await.unwrap();
    let first = settlement_row(f, report.report_id).await;
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
}
async fn retained_checkpoint(
    f: &Fixture,
    report: &ResultReport,
    source: Uuid,
    changed: bool,
) -> String {
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

<<<<<<< HEAD
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
        _task_ref: task_ref,
    }
=======
    manifest.checkpoint.hex
>>>>>>> refs/remotes/central/main
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
async fn settlement_rows(f: &Fixture, report: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM profiled_auto_settlements WHERE report_id=$1")
        .bind(report)
        .fetch_one(&f.db.pool().await)
        .await
        .unwrap()
}

/// The facts the result path recorded for the report (spec 0016 section 7).
async fn result_facts(f: &Fixture, report: Uuid) -> Option<Value> {
    sqlx::query_scalar("SELECT facts FROM delivery_result_facts WHERE report_id=$1")
        .bind(report)
        .fetch_optional(&f.db.pool().await)
        .await
        .unwrap()
}

pub(crate) struct CheckedSource<'a> {
    pub(crate) id: Uuid,
    pub(crate) checkpoint: &'a str,
}

impl AutoEnv {
    async fn settle(&self) -> AutoSettlement {
        settlement::settle_after_record(&self.f.state, self.report.report_id)
            .await
            .unwrap()
            .expect("default policy applies")
    }
    pub(crate) async fn check(&self, source: CheckedSource<'_>, validator: &str, verdict: &str) {
        sqlx::query("INSERT INTO profiled_check_evidence(attempt_id,check_id,source_operation_id,checkpoint,validator_digest,verdict,evidence,recorded_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT(attempt_id,check_id,source_operation_id) DO UPDATE SET checkpoint=EXCLUDED.checkpoint,validator_digest=EXCLUDED.validator_digest,verdict=EXCLUDED.verdict,evidence=EXCLUDED.evidence")
            .bind(self.report.binding.attempt_id).bind(&self.check_id).bind(source.id)
            .bind(source.checkpoint).bind(validator).bind(verdict)
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

#[path = "auto_settlement56/default_policy.rs"]
mod default_policy;
#[path = "auto_settlement56/git_peer.rs"]
mod git_peer;
#[path = "auto_settlement56/graph_peer.rs"]
mod graph_peer;
use graph_peer::GraphPeer;
#[path = "auto_settlement56/publication.rs"]
mod publication;
#[path = "auto_settlement56/replacement.rs"]
mod replacement;
pub(crate) use git_peer::{git_peer, silent_git};
pub(crate) use publication::{admitted, evidence_rows, head_of, ready_candidate};

/// Epic 0 (spec 0016 section 6.2): a playbook's `work.complete` and
/// `work.return` on a recorded result, through the same `prepare_locked` /
/// `dispatch` path and its fences. The playbook decides: no policy evidence
/// is asked for, only that the result is the attempt's current report head,
/// that the attempt still holds the task, that its stop is proven and that it
/// was neither revoked nor cancelled.
<<<<<<< HEAD
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
        sqlx::query("INSERT INTO task_holds(comment_id,task_id,attempts,project_id,reason,attempt_id,comment_body,held_at) VALUES($1,$2,1,$3,'lineage_retry_budget_exhausted',$4,'budget exhausted',to_timestamp($5))")
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
            assert!(body.contains("lineage_retry_budget_exhausted"));
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
            &mut pool.acquire().await.unwrap(),
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
                &mut pool.acquire().await.unwrap(),
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

    /// Spec 0021 section 5.2: whatever its source, a hold tells the project's
    /// supervisor once, with the facts and commands it needs.
    fn assert_hold_facts(body: &str, ids: &[Uuid], reason: &str, outcome: &str, attempt: Uuid) {
        for id in ids {
            assert!(body.contains(&id.to_string()), "{id} in {body}");
        }
        assert!(body.contains(reason), "{reason} in {body}");
        assert!(
            body.contains(&format!("attempt {attempt}: outcome {outcome}")),
            "the attempt and its outcome in {body}"
        );
        for command in [
            "task.work",
            "task.history",
            "task.retry",
            "attempt.abandon",
            "task.edit",
            "task.cancel",
        ] {
            assert!(body.contains(command), "{command} in {body}");
        }
    }

    #[tokio::test]
    async fn every_kind_of_task_hold_notifies_its_project_once_and_a_new_hold_notifies_again() {
        use aq_playbook_domain::scheduling_notices::notify;
        let env = Box::pin(auto_env("not_required", axum::http::StatusCode::OK)).await;
        let pool = env.f.db.pool().await;
        let task = env.f.selection.lineage.task_id;
        let project = env.f.selection.lineage.project_id;
        let attempt = env.report.binding.attempt_id;
        sqlx::query("UPDATE supervisor_controls c SET enabled=true,profile='{}',approved_by=$2 FROM supervisor_conversation_ids i WHERE i.conversation_id=c.conversation_id AND i.project_id=$1")
            .bind(project).bind(env.f.actor).execute(&pool).await.unwrap();
        let outcome: String = sqlx::query_scalar("SELECT convert_from(report_bytes,'UTF8')::jsonb->>'outcome' FROM profiled_result_reports WHERE report_id=$1")
            .bind(env.report.report_id).fetch_one(&pool).await.unwrap();
        let bodies = std::sync::Mutex::new(Vec::<String>::new());
        let send = async |_: &mut sqlx::Transaction<'_, sqlx::Postgres>,
                          scope: Option<Uuid>,
                          body: &str,
                          _: u64| {
            assert_eq!(scope, Some(project));
            bodies.lock().unwrap().push(body.to_owned());
            Ok::<_, ()>(Uuid::new_v4())
        };
        let due = NOW as i64 + 600_001;
        // A budget hold, a never-granted hold, a policy hold, and the first
        // reason held again after its release.
        let reasons = [
            "lineage_retry_budget_exhausted",
            "never_granted_retry_bound_exhausted",
            "replacement_project_revision_changed",
            "lineage_retry_budget_exhausted",
        ];
        for (n, reason) in reasons.into_iter().enumerate() {
            sqlx::query("INSERT INTO task_holds(comment_id,task_id,attempts,project_id,reason,attempt_id,comment_body,held_at) VALUES($1,$2,1,$3,$4,$5,'held',to_timestamp($6))")
                .bind(Uuid::new_v4()).bind(task).bind(project).bind(reason).bind(attempt).bind(NOW as f64 / 1000.0)
                .execute(&pool).await.unwrap();
            // Reading the task, and every later sweep, tells nobody again.
            work_reasons(&env).await;
            for _ in 0..3 {
                notify(&pool, due, &send).await.unwrap();
            }
            let sent = bodies.lock().unwrap().clone();
            assert_eq!(sent.len(), n + 1, "one notice per hold: {reason}");
            assert_hold_facts(
                &sent[n],
                &[project, task, attempt],
                reason,
                &outcome,
                attempt,
            );
            sqlx::query("UPDATE task_holds SET cleared_at=now(),cleared_by='claim_released' WHERE task_id=$1 AND cleared_at IS NULL")
                .bind(task).execute(&pool).await.unwrap();
            notify(&pool, due, &send).await.unwrap();
            assert_eq!(
                bodies.lock().unwrap().len(),
                n + 1,
                "a released hold is quiet"
            );
        }
    }

    #[tokio::test]
    async fn a_delivery_hold_of_any_reason_notifies_once_and_never_after_the_playbook_did() {
        use aq_playbook_domain::{delivery_decisions, scheduling_notices::notify};
        let env = Box::pin(auto_env("not_required", axum::http::StatusCode::OK)).await;
        let pool = env.f.db.pool().await;
        let task = env.f.selection.lineage.task_id;
        let project = env.f.selection.lineage.project_id;
        let attempt = env.report.binding.attempt_id;
        let result = env.report.report_id;
        sqlx::query("UPDATE supervisor_controls c SET enabled=true,profile='{}',approved_by=$2 FROM supervisor_conversation_ids i WHERE i.conversation_id=c.conversation_id AND i.project_id=$1")
            .bind(project).bind(env.f.actor).execute(&pool).await.unwrap();
        let outcome: String = sqlx::query_scalar("SELECT convert_from(report_bytes,'UTF8')::jsonb->>'outcome' FROM profiled_result_reports WHERE report_id=$1")
            .bind(result).fetch_one(&pool).await.unwrap();
        let reason = "policy_hold";
        sqlx::query("INSERT INTO delivery_decisions(result_id,task_id,decision,reason,decided_at_ms) VALUES ($1,$2,'held',$3,$4)")
            .bind(result).bind(task).bind(reason).bind(NOW as i64).execute(&pool).await.unwrap();
        let key = delivery_decisions::notify_key(&format!("{result}:{reason}")).unwrap();
        let bodies = std::sync::Mutex::new(Vec::<String>::new());
        let send = async |_: &mut sqlx::Transaction<'_, sqlx::Postgres>,
                          scope: Option<Uuid>,
                          body: &str,
                          _: u64| {
            assert_eq!(scope, Some(project));
            bodies.lock().unwrap().push(body.to_owned());
            Ok::<_, ()>(Uuid::new_v4())
        };
        let due = NOW as i64 + 600_001;
        // The playbook's own notice for this hold is the one notice.
        delivery_decisions::record_told(
            &mut pool.acquire().await.unwrap(),
            project,
            &key,
            Uuid::new_v4(),
            NOW as i64,
        )
        .await
        .unwrap();
        notify(&pool, due, &send).await.unwrap();
        assert!(bodies.lock().unwrap().is_empty(), "no duplicate notice");
        // Without it, the hold still reaches the supervisor, once.
        sqlx::query("DELETE FROM delivery_notifications WHERE project_id=$1")
            .bind(project)
            .execute(&pool)
            .await
            .unwrap();
        for _ in 0..3 {
            notify(&pool, due, &send).await.unwrap();
        }
        let sent = bodies.lock().unwrap().clone();
        assert_eq!(sent.len(), 1);
        assert_hold_facts(
            &sent[0],
            &[project, task, attempt, result],
            reason,
            &outcome,
            attempt,
        );
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
                &mut pool.acquire().await.unwrap(),
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
        let behind = record("pull_request.await_checks", "conflict", result).await;
        sqlx::query("UPDATE playbook_host_operations SET request=request || '{\"answer_note\":\"behind_base\"}'::jsonb WHERE operation_id=$1")
            .bind(behind).execute(&pool).await.unwrap();
        let reasons = work_reasons(&env).await;
        assert!(
            reasons
                .iter()
                .any(|r| r["message"].as_str().is_some_and(|text| text
                    .contains("pull_request.await_checks")
                    && text.contains("behind its base")
                    && text.contains("qualifies for repair")
                    && text.contains(&"4".repeat(40))))
        );
        let port = aq_playbook_domain::delivery_store::PgDelivery(pool.clone());
        let origin = aq_playbook_domain::host10::ResultReady {
            task_id: task,
            result_id: result,
            project_id: lineage.project_id,
            attempt_id: attempt,
            version: 1,
        };
        let notice = json!({"text":"Inspect this held result."});
        let mut tx = pool.begin().await.unwrap();
        let (_, body) = aq_playbook_domain::delivery_decisions::notification(
            &mut tx,
            &port,
            Some(origin),
            lineage.project_id,
            &notice,
        )
        .await
        .unwrap();
        assert!(body.contains("behind its base") && body.contains("qualifies for repair"));
        assert!(
            aq_playbook_domain::delivery_decisions::notification(
                &mut tx,
                &port,
                Some(origin),
                Uuid::new_v4(),
                &notice,
            )
            .await
            .is_err()
        );
        tx.rollback().await.unwrap();

        // A failed required check also carries the redacted tail of its job
        // log, for the exact head it was read at (spec 0021 section 5.3).
        let failed = record("pull_request.await_checks", "failed", result).await;
        sqlx::query("UPDATE playbook_host_operations SET completed_at_ms=$2 WHERE operation_id=$1")
            .bind(failed)
            .bind(NOW as i64 + 1)
            .execute(&pool)
            .await
            .unwrap();
        let log: String = (0..2000)
            .map(|i| format!("== unit ==\nline {i:04} of the failed job\n"))
            .chain(["FAILED test_flaky\n".to_owned()])
            .collect();
        let pull_request = |head: String, log: String| {
            let pool = pool.clone();
            async move {
                sqlx::query("INSERT INTO delivery_pull_requests(task_id,number,url,state,mergeable,checks_state,checks_failed,checks_log_head,checks_log_tail,updated_at_ms) VALUES($1,228,'https://example.test/pr/228','open','mergeable','failed','[\"unit\"]',$2,$3,$4) ON CONFLICT(task_id) DO UPDATE SET checks_log_head=$2,checks_log_tail=$3")
                    .bind(task).bind(head).bind(log).bind(NOW as i64)
                    .execute(&pool).await.unwrap();
            }
        };
        pull_request("4".repeat(40), log.clone()).await;
        let shown = |reasons: &[Value]| {
            reasons.iter().any(|r| {
                r["message"].as_str().is_some_and(|m| {
                    m.contains("recorded failed from pull_request.await_checks")
                        && m.contains("untrusted")
                        && m.contains("FAILED test_flaky")
                })
            })
        };
        assert!(shown(&work_reasons(&env).await), "task.work shows the tail");
        let mut tx = pool.begin().await.unwrap();
        let (_, body) = aq_playbook_domain::delivery_decisions::notification(
            &mut tx,
            &port,
            Some(origin),
            lineage.project_id,
            &notice,
        )
        .await
        .unwrap();
        tx.rollback().await.unwrap();
        assert!(
            body.contains("FAILED test_flaky"),
            "the notice shows the tail"
        );
        assert!(
            body.len() <= aq_playbook_domain::delivery_store::SUPERVISOR_MESSAGE_BYTES,
            "a long tail is cut to fit the notice, not the notice dropped: {}",
            body.len()
        );
        assert!(body.contains("Use task.retry"), "the instructions stay");
        // A tail read for another head is not this result's.
        pull_request("5".repeat(40), log).await;
        assert!(!shown(&work_reasons(&env).await));
        sqlx::query("DELETE FROM delivery_pull_requests WHERE task_id=$1")
            .bind(task)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM playbook_host_operations WHERE operation_id=$1")
            .bind(failed)
            .execute(&pool)
            .await
            .unwrap();

        let merge = record("pull_request.merge", "running", result).await;
        assert_eq!(held_report(&pool, task).await.unwrap(), None);
        assert!(!work_reasons(&env).await.iter().any(|r| {
            r["message"]
                .as_str()
                .is_some_and(|s| s.contains("qualifies for repair"))
        }));
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
        sqlx::query("INSERT INTO task_holds(comment_id,task_id,attempts,project_id,reason,attempt_id,comment_body,held_at) VALUES($1,$2,1,$3,'lineage_cost_budget_exhausted',$4,'returned',to_timestamp($5))")
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
        sqlx::query("UPDATE delivery_decisions SET reason='checks_failed',detail='tests (shard 3/6)',update_from_main=false WHERE result_id=$1")
            .bind(result).execute(&pool).await.unwrap();
        assert!(work_reasons(&env).await.iter().any(|r| {
            r["message"]
                .as_str()
                .is_some_and(|m| m.contains("tests (shard 3/6)"))
        }));
        let calls = AtomicUsize::new(0);
        aq_playbook_domain::scheduling_notices::notify(
            &pool,
            NOW as i64 + 600_001,
            async |_, scope, body: &str, _| {
                assert_eq!(scope, Some(lineage.project_id));
                assert!(
                    body.contains("lineage_cost_budget_exhausted")
                        && body.contains(&"4".repeat(40))
                        && body.contains("conflict")
                        && body.contains(&evidence)
                        && body.contains("tests (shard 3/6)")
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
=======
#[cfg(feature = "delivery_playbook")]
#[path = "auto_settlement56/playbook.rs"]
mod playbook;
>>>>>>> refs/remotes/central/main
