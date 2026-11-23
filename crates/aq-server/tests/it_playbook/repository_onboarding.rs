//! D2: one owner operation onboards a GitHub repository into a central aq-git
//! binding with a retained base, a project binding and a publication policy.
use crate::common;
use aq_server::source_retention::{Runtime as GitRuntime, SourceRetentionConfig};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use common::selection::*;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::Notify,
};
use uuid::Uuid;

const HEAD: &str = "1111111111111111111111111111111111111111";
const APP_TOKEN: &str = "fixture-app-installation-token";
const SECRET: &str = "github_pat_synthetic_never_logged_0000";

#[derive(Default)]
struct Fake {
    branch: String,
    tree: Vec<Value>,
    signing: bool,
    registers: Vec<Value>,
    syncs: Vec<Value>,
    /// `repo.show`: the binding generation and the origin aq-git reports.
    generation: u64,
    remote: String,
    show_overrides: serde_json::Map<String, Value>,
    absent_ref: bool,
    metadata_override: Option<Value>,
    metadata_auth: Vec<Option<String>>,
    rules_gate: Option<(Arc<Notify>, Arc<Notify>)>,
}
type Shared = Arc<Mutex<Fake>>;

mod git;
use git::fake_git;

async fn repo(
    State(f): State<Shared>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Json<Value> {
    let mut f = f.lock().unwrap();
    f.metadata_auth.push(
        headers
            .get("authorization")
            .map(|h| h.to_str().unwrap().to_owned()),
    );
    Json(f.metadata_override.clone().unwrap_or_else(
        || json!({"default_branch":f.branch,"full_name":format!("{owner}/{name}"),"private":false}),
    ))
}
async fn rules(
    State(f): State<Shared>,
    Path((_o, _n, branch)): Path<(String, String, String)>,
) -> Json<Value> {
    let (signing, gate) = {
        let mut f = f.lock().unwrap();
        assert!(!branch.is_empty());
        (f.signing, f.rules_gate.take())
    };
    if let Some((arrived, proceed)) = gate {
        arrived.notify_one();
        proceed.notified().await;
    }
    Json(if signing {
        json!([{"type":"required_signatures"}])
    } else {
        json!([])
    })
}

async fn app_token(headers: HeaderMap, Json(scope): Json<Value>) -> (StatusCode, Json<Value>) {
    assert!(
        headers["authorization"]
            .to_str()
            .unwrap()
            .starts_with("Bearer ey")
    );
    assert_eq!(scope, json!({"repositories":["widgets"]}));
    (
        StatusCode::CREATED,
        Json(json!({
            "token":APP_TOKEN, "expires_at":(chrono::Utc::now()+chrono::Duration::hours(1)).to_rfc3339()
        })),
    )
}

struct Onboarding {
    env: Env,
    app: Router,
    fake: Shared,
    api_base: String,
    _tasks: Vec<tokio::task::JoinHandle<()>>,
    _dir: tempfile::TempDir,
}

async fn onboarding() -> Onboarding {
    let env = setup(false).await;
    let dir = tempfile::tempdir().unwrap();
    let fake: Shared = Arc::new(Mutex::new(Fake {
        branch: "trunk".into(),
        generation: 1,
        tree: vec![
            json!({"mode":"100644","type":"blob","oid":{"algorithm":"sha1","hex":HEAD},"path":{"encoding":"utf8","value":"README.md"}}),
        ],
        ..Default::default()
    }));
    let socket = dir.path().join("aq-git.sock");
    let mut tasks = vec![fake_git(&socket, fake.clone())];
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let github = Router::new()
        .route("/app/installations/42/access_tokens", post(app_token))
        .route("/repos/{owner}/{name}", get(repo))
        .route("/repos/{owner}/{name}/rules/branches/{branch}", get(rules))
        .with_state(fake.clone());
    tasks.push(tokio::spawn(async move {
        axum::serve(listener, github).await.unwrap()
    }));
    let secret = dir.path().join("token");
    std::fs::write(&secret, SECRET).unwrap();
    let mut config = (*env.state.config).clone();
    config.onboarding = Some(
        toml::from_str(&format!(
            "github_api_url=\"http://{addr}\"\n[credential_profiles.acme-token]\nkind=\"https_fine_grained_token\"\nsecret_file={:?}\n",
            secret
        ))
        .unwrap(),
    );
    let mut state = env.state.clone();
    state.config = Arc::new(config);
    state.source_retention = Some(Arc::new(
        GitRuntime::new(SourceRetentionConfig {
            keep_per_task: 2,
            max_age: 30 * 86400,
            staging_root: dir.path().join("staging"),
            git_root: dir.path().join("git"),
            git_socket: socket,
            max_staging_bytes: 1024 * 1024,
            max_bundle_bytes: 1024 * 1024,
            bulk_concurrency: 1,
            incomplete_ttl_secs: 60,
            git_deadline_ms: 30_000,
        })
        .unwrap(),
    ));
    let app = aq_server::build_admin_router(state);
    Onboarding {
        env,
        app,
        fake,
        api_base: format!("http://{addr}"),
        _tasks: tasks,
        _dir: dir,
    }
}

fn request(o: &Onboarding, operation_id: Uuid) -> Value {
    let mut template = o.env.body.clone();
    for key in [
        "project_id",
        "repository_id",
        "repository_binding_generation",
        "repository_adapter_reference",
    ] {
        template.as_object_mut().unwrap().remove(key);
    }
    json!({"operation_id":operation_id,"project_id":Uuid::from_u128(1),"repository_id":Uuid::from_u128(2),
        "github":{"owner":"acme","name":"widgets"},"transport":"ssh",
        "publication":{"mode":"fast_forward"},"template":template})
}

async fn onboard(o: &Onboarding, body: Value) -> (StatusCode, Value) {
    call(
        &o.app,
        "/api/repositories/onboard",
        Some(body),
        "owner@localhost",
    )
    .await
}

mod onboard;

fn adopt_request(op: Uuid) -> Value {
    json!({"operation_id":op,"project_id":Uuid::from_u128(1),"repository_id":Uuid::from_u128(2),
        "expected_binding_generation":1,"github":{"owner":"acme","name":"widgets"},
        "remote_url":"https://github.com/acme/widgets.git"})
}
async fn adopt(o: &Onboarding, body: Value) -> (StatusCode, Value) {
    call(
        &o.app,
        "/api/repositories/adopt",
        Some(body),
        "owner@localhost",
    )
    .await
}
async fn rows(o: &Onboarding) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM repository_onboardings")
        .fetch_one(&o.env.pool)
        .await
        .unwrap()
}
#[cfg(feature = "delivery_playbook")]
async fn delivery_target(
    o: &Onboarding,
    generation: u64,
) -> Option<aq_delivery::commands::DeliveryTarget> {
    let git = aq_delivery::commands::GitTarget {
        repository_id: Uuid::from_u128(2),
        binding_generation: generation,
    };
    aq_delivery::commands::onboarded_target(&o.env.pool, Uuid::from_u128(1), git)
        .await
        .unwrap()
}
mod adoption;
mod cli;
