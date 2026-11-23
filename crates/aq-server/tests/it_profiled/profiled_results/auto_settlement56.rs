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
    #[cfg(feature = "delivery_playbook")]
    let task_ref = match report_source {
        ReportSource::Pushed { changed } => {
            Some(record_push(&mut f, &mut report, &auth, changed).await)
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

    manifest.checkpoint.hex
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
#[cfg(feature = "delivery_playbook")]
#[path = "auto_settlement56/playbook.rs"]
mod playbook;
