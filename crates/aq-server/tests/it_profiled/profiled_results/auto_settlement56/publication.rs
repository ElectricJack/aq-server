use super::*;

use super::git_peer::git_peer_cancelling;
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
    env.check(
        CheckedSource {
            id: env.source,
            checkpoint: &checkpoint,
        },
        &validator,
        "passed",
    )
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

#[path = "publication/cancellation.rs"]
mod cancellation;
#[path = "publication/retained.rs"]
mod retained;
#[path = "publication/rework.rs"]
mod rework;
