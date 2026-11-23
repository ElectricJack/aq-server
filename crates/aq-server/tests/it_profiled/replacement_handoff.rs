//! Spec 0007 phase 1 (S7, S8): a handed-off attempt's successor starts from
//! the saved commit, and owner/service handoffs are not charged to the lineage
//! budgets. Synthetic SQL owners and a synthetic central aq-git peer; no
//! provider and no real Git export.
use crate::profiled_results::auto_settlement56::{
    CheckedSource, ReportSetup, auto_env_source, evidence_rows, head_of,
};
use crate::profiled_results::auto_settlement56::{git_peer, ready_candidate};
use crate::{
    common::{self, profiled::NOW, source_exports::setup},
    profiled_results::auto_settlement56::{AutoEnv, admitted, auto_env_with, silent_git},
};
use aq_admission::transport::ProcessOutcome;
use aq_server::{
    profiled_execution::{handoff, replacement::Replacement, source, source_exports},
    project_bindings::domain::FrozenProjectBinding,
    source_retention::{Runtime, SourceRetentionConfig},
};
use serde_json::{Value, json};
use sqlx::Row;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use uuid::Uuid;

struct Handed {
    env: AutoEnv,
    socket: std::path::PathBuf,
    _git: tempfile::TempDir,
}
/// A recorded Failed report whose saved checkpoint is centrally retained,
/// with the central aq-git socket configured for delivery.
async fn handed(reason: Option<&str>) -> Handed {
    let mut env = Box::pin(auto_env_with(
        "not_required",
        axum::http::StatusCode::OK,
        false,
        Some(ProcessOutcome::Failed),
    ))
    .await;
    let git = tempfile::tempdir().unwrap();
    let socket = git.path().join("git.sock");
    env.f.state.source_retention = Some(Arc::new(
        Runtime::new(test_config(git.path(), &socket)).unwrap(),
    ));
    if let Some(reason) = reason {
        sqlx::query("INSERT INTO attempt_handoffs(attempt_id,node_id,reason,requested_at) VALUES($1,$2,$3,now())")
            .bind(env.report.binding.attempt_id)
            .bind(env.f.node.node_id)
            .bind(reason)
            .execute(&env.f.db.pool().await)
            .await
            .unwrap();
    }
    Handed {
        env,
        socket,
        _git: git,
    }
}
fn replacement_of(predecessor: Uuid) -> Replacement {
    let artifact = |id: &str| common::profiled::artifact(id, id.as_bytes());
    Replacement {
        predecessor,
        attempts_started: 1,
        exempt_attempts: 0,
        elapsed_ms: 1,
        stop_receipt: artifact("stop"),
        retained_source_receipt: artifact("retained"),
        effect_reconciliation_receipt: artifact("effects"),
        needs_release: false,
    }
}
async fn comments(h: &Handed) -> Vec<aq_task_comments::Comment> {
    let thread = aq_task_comments::Thread {
        project_id: admitted(&h.env).await.project_id,
        task_id: h.env.f.selection.lineage.task_id,
    };
    aq_task_comments::list(&h.env.f.db.pool().await, thread, None, 50)
        .await
        .unwrap()
        .0
}
fn export_request(base: &str, handle: Uuid) -> source_exports::Request {
    serde_json::from_value(json!({"schema_version":1,"operation_id":Uuid::new_v4(),"project_id":Uuid::new_v4(),
        "project_revision":1,"repository_id":Uuid::new_v4(),"logical_identity":"synthetic","binding_generation":1,
        "base":{"algorithm":"sha1","hex":base},"source_retention_handle":handle,
        "source_observation":common::profiled::artifact("observation", b"observation")}))
    .unwrap()
}

mod budgets;
mod publication;
mod successor;

fn test_config(directory: &std::path::Path, socket: &std::path::Path) -> SourceRetentionConfig {
    SourceRetentionConfig {
        keep_per_task: 2,
        max_age: 30 * 86400,
        staging_root: directory.join("staging"),
        git_root: directory.join("git"),
        git_socket: socket.to_owned(),
        max_staging_bytes: 2097152,
        max_bundle_bytes: 4096,
        bulk_concurrency: 2,
        incomplete_ttl_secs: 60,
        git_deadline_ms: 30_000,
    }
}
