//! D2: one owner operation onboards a GitHub repository into a central aq-git
//! binding with a retained base, a project binding and a publication policy.
mod common;
use aq_server::source_retention::{Runtime as GitRuntime, SourceRetentionConfig};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::get,
};
use common::selection::*;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use uuid::Uuid;

const HEAD: &str = "1111111111111111111111111111111111111111";
const SECRET: &str = "github_pat_synthetic_never_logged_0000";

#[derive(Default)]
struct Fake {
    branch: String,
    tree: Vec<Value>,
    signing: bool,
    registers: Vec<Value>,
    syncs: Vec<Value>,
}
type Shared = Arc<Mutex<Fake>>;

fn reply(r: &Value, outcome: Result<Value, (&str, &str)>) -> Vec<u8> {
    let mut out = json!({"protocol_version":1,"request_id":r["request_id"]});
    match outcome {
        Ok(result) => {
            out["ok"] = true.into();
            out["result"] = result;
        }
        Err((code, message)) => {
            out["ok"] = false.into();
            out["error"] =
                json!({"code":code,"message":message,"details":{},"retry":"refresh_inputs"});
        }
    }
    let mut bytes = serde_json::to_vec(&out).unwrap();
    bytes.push(b'\n');
    bytes
}

fn fake_git(path: &std::path::Path, fake: Shared) -> tokio::task::JoinHandle<()> {
    let listener = tokio::net::UnixListener::bind(path).unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let fake = fake.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stream);
                let mut line = Vec::new();
                reader.read_until(b'\n', &mut line).await.unwrap();
                let r: Value = serde_json::from_slice(&line).unwrap();
                let target = {
                    let f = fake.lock().unwrap();
                    format!("refs/heads/{}", f.branch)
                };
                let outcome = match r["operation"].as_str().unwrap() {
                    "repo.register" => {
                        assert_eq!(r["expected_binding_generation"], 0);
                        fake.lock().unwrap().registers.push(r["params"].clone());
                        if r["params"].get("credential_profile").is_some() {
                            Err((
                                "unsupported_feature",
                                "Named credential profiles are not enabled",
                            ))
                        } else {
                            Ok(
                                json!({"repository_id":r["repository_id"],"binding_generation":1,"mode":"central","object_format":"sha1","allowed_refs":[target],"allowed_targets":[target]}),
                            )
                        }
                    }
                    "repo.sync" => {
                        fake.lock().unwrap().syncs.push(r["params"].clone());
                        Ok(
                            json!({"observation_id":"obs-1","binding_generation":1,"covered_refs":[target],"refs":{target.clone():HEAD},"availability":"available"}),
                        )
                    }
                    "tree" => {
                        assert_eq!(r["params"]["oid"], json!({"algorithm":"sha1","hex":HEAD}));
                        Ok(json!({"entries":fake.lock().unwrap().tree,"truncated":false}))
                    }
                    "repo.show" => Ok(
                        json!({"repository_id":r["repository_id"],"binding_generation":1,"mode":"central","object_format":"sha1","allowed_targets":[target]}),
                    ),
                    other => panic!("unexpected aq-git operation {other}"),
                };
                let bytes = reply(&r, outcome);
                reader.into_inner().write_all(&bytes).await.unwrap();
            });
        }
    })
}

async fn repo(State(f): State<Shared>, Path((_o, _n)): Path<(String, String)>) -> Json<Value> {
    Json(json!({"default_branch":f.lock().unwrap().branch,"private":false}))
}
async fn rules(
    State(f): State<Shared>,
    Path((_o, _n, _b)): Path<(String, String, String)>,
) -> Json<Value> {
    Json(if f.lock().unwrap().signing {
        json!([{"type":"required_signatures"}])
    } else {
        json!([])
    })
}

struct Onboarding {
    env: Env,
    app: Router,
    fake: Shared,
    _tasks: Vec<tokio::task::JoinHandle<()>>,
    _dir: tempfile::TempDir,
}

async fn onboarding() -> Onboarding {
    let env = setup(false).await;
    let dir = tempfile::tempdir().unwrap();
    let fake: Shared = Arc::new(Mutex::new(Fake {
        branch: "trunk".into(),
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
        })
        .unwrap(),
    ));
    let app = aq_server::build_admin_router(state);
    Onboarding {
        env,
        app,
        fake,
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

#[tokio::test]
async fn onboard_github_repo_creates_binding_and_retained_base() {
    let o = onboarding().await;
    let op = Uuid::new_v4();
    let (status, v) = onboard(&o, request(&o, op)).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["disposition"], "bound", "{v}");
    assert_eq!(v["step"], "bound");
    assert_eq!(v["default_branch"], "trunk");
    assert_eq!(v["base"]["ref"], "refs/heads/trunk");
    assert_eq!(v["base"]["oid"], HEAD);
    assert_eq!(v["base"]["observation_id"], "obs-1");
    {
        let f = o.fake.lock().unwrap();
        assert_eq!(f.registers.len(), 1);
        let p = &f.registers[0];
        assert_eq!(p["mode"], "central");
        assert_eq!(p["remote_url"], "ssh://git@github.com/acme/widgets.git");
        assert_eq!(p["allowed_refs"], json!(["refs/heads/trunk"]));
        assert_eq!(p["allowed_targets"], json!(["refs/heads/trunk"]));
        assert!(p.get("credential_profile").is_none());
        assert_eq!(f.syncs[0]["refs"], json!(["refs/heads/trunk"]));
    }
    let (status, project) = call(&o.app, &path(), None, "owner@localhost").await;
    assert_eq!(status, StatusCode::OK, "{project}");
    let binding = &project["binding"];
    assert_eq!(project["revision"], 1, "{project}");
    assert_eq!(binding["repository_id"], Uuid::from_u128(2).to_string());
    assert_eq!(binding["repository_binding_generation"], 1);
    assert_eq!(binding["repository_adapter_reference"], "aq.git.configured");
    assert_eq!(binding["graph_adapter_reference"], "aq.graph.configured");
    assert_eq!(binding["publication"]["mode"], "fast_forward");
    assert_eq!(binding["publication"]["target"], "refs/heads/trunk");
    // Replay of the same operation is the same durable answer, not a second binding.
    let (status, again) = onboard(&o, request(&o, op)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again, v);
    assert_eq!(o.fake.lock().unwrap().registers.len(), 1);
    let (status, shown) = call(
        &o.app,
        &format!("/api/repositories/onboard/{op}"),
        None,
        "owner@localhost",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(shown, v);
}

#[tokio::test]
async fn unsupported_repo_profile_refused_with_reason() {
    let o = onboarding().await;
    let entry = |mode: &str, kind: &str, name: &str| json!({"mode":mode,"type":kind,"oid":{"algorithm":"sha1","hex":HEAD},"path":{"encoding":"utf8","value":name}});
    type Case = (&'static str, &'static str, Box<dyn Fn(&mut Fake)>, bool);
    let cases: Vec<Case> = vec![
        (
            "gitattributes",
            "precheck",
            Box::new(move |f: &mut Fake| f.tree.push(entry("100644", "blob", ".gitattributes"))),
            false,
        ),
        (
            "submodules",
            "precheck",
            Box::new(move |f: &mut Fake| f.tree.push(entry("160000", "commit", "vendor"))),
            false,
        ),
        (
            "lfs",
            "precheck",
            Box::new(move |f: &mut Fake| f.tree.push(entry("100644", "blob", ".lfsconfig"))),
            false,
        ),
        (
            "required_signing",
            "precheck",
            Box::new(|f: &mut Fake| f.signing = true),
            false,
        ),
        (
            "credential_profile_unavailable",
            "aq-git",
            Box::new(|_: &mut Fake| {}),
            true,
        ),
    ];
    for (reason, source, arrange, credential) in cases {
        {
            let mut f = o.fake.lock().unwrap();
            f.tree.truncate(1);
            f.signing = false;
            arrange(&mut f);
        }
        let mut body = request(&o, Uuid::new_v4());
        if credential {
            body["transport"] = "https".into();
            body["credential_profile"] = "acme-token".into();
        }
        let (status, v) = onboard(&o, body.clone()).await;
        assert_eq!(status, StatusCode::OK, "{reason}: {v}");
        assert_eq!(v["disposition"], "refused", "{reason}: {v}");
        assert_eq!(v["step"], "refused");
        assert_eq!(v["refusal"]["reason"], reason, "{v}");
        assert_eq!(v["refusal"]["source"], source, "{v}");
        assert!(!v.to_string().contains(SECRET));
        // A refusal is terminal and replayable.
        assert_eq!(onboard(&o, body).await.1, v);
        let (status, _) = call(&o.app, &path(), None, "owner@localhost").await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{reason}: no half-created project"
        );
    }
    let rows: Vec<String> = sqlx::query_scalar("SELECT state::text FROM repository_onboardings")
        .fetch_all(&o.env.pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 5);
    assert!(rows.iter().all(|r| !r.contains(SECRET)));
    assert!(rows.iter().any(|r| r.contains("acme-token")));
}

#[tokio::test]
async fn onboarding_resumes_after_partial_failure() {
    let o = onboarding().await;
    let op = Uuid::new_v4();
    let mut body = request(&o, op);
    body["default_branch"] = "release".into();
    o.fake.lock().unwrap().branch = "release".into();
    // The graph is unavailable, so only the project-binding step fails.
    o.env
        .graph_status
        .store(3, std::sync::atomic::Ordering::SeqCst);
    let (status, v) = onboard(&o, body.clone()).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["disposition"], "incomplete", "{v}");
    assert_eq!(v["step"], "checked");
    assert!(v["error"]["code"].is_string());
    o.env
        .graph_status
        .store(0, std::sync::atomic::Ordering::SeqCst);
    let (status, v) = onboard(&o, body.clone()).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["disposition"], "bound", "{v}");
    assert_eq!(v["default_branch"], "release");
    assert_eq!(o.fake.lock().unwrap().registers.len(), 1);
    body["github"]["name"] = "other".into();
    let (status, _) = onboard(&o, body).await;
    assert_eq!(status, StatusCode::CONFLICT);
}
