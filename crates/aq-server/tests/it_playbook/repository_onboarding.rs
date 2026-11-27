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

<<<<<<< HEAD
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
        // Spec 0016 §5.2: every task's branch is a prefix-scoped target.
        assert_eq!(p["allowed_target_prefixes"], json!(["refs/heads/aq/"]));
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
=======
mod onboard;
>>>>>>> refs/remotes/central/main

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
<<<<<<< HEAD

#[tokio::test]
async fn adopting_task_branch_delivery_needs_no_direct_push_to_the_default_branch() {
    let o = onboarding().await;
    bind(&o.env).await;
    {
        let mut f = o.fake.lock().unwrap();
        f.remote = "https://github.com/acme/widgets.git".into();
        f.show_overrides.insert("allowed_targets".into(), json!([]));
        f.show_overrides
            .insert("allowed_target_prefixes".into(), json!(["refs/heads/aq/"]));
    }
    let (status, body) = adopt(&o, adopt_request(Uuid::new_v4())).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["disposition"], "bound");
}

#[tokio::test]
async fn adopting_a_seeded_project_records_a_replayable_delivery_target() {
    let o = onboarding().await;
    o.fake.lock().unwrap().remote = "https://github.com/acme/widgets.git".into();
    let seeded = bind(&o.env).await;
    assert!(delivery_target(&o, 1).await.is_none());
    let owner2 = "another-owner@localhost";
    o.env
        .state
        .store
        .invite_user(owner2, aq_server::store::Role::Owner, chrono::Utc::now())
        .await
        .unwrap();
    // The viewer is refused before either adapter is called.
    assert_eq!(
        call(
            &o.app,
            "/api/repositories/adopt",
            Some(adopt_request(Uuid::new_v4())),
            "viewer@localhost"
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let op = Uuid::new_v4();
    let (status, v) = adopt(&o, adopt_request(op)).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["disposition"], "bound", "{v}");
    assert_eq!(v["default_branch"], "trunk");
    assert_eq!(v["default_ref"], "refs/heads/trunk");
    assert_eq!(v["repository_identity"], "historical-seed");
    assert_eq!(v["adopted"], true);
    assert!(v["base"].is_null());
    assert!(v["repository_observation_id"].as_str().is_some());
    assert_eq!(v["repository_binding_generation"], 1);
    {
        let target = delivery_target(&o, 1).await.expect("target resolves");
        assert_eq!(
            (target.github.owner.as_str(), target.github.name.as_str()),
            ("acme", "widgets")
        );
        assert_eq!(target.default_branch, "trunk");
        assert_eq!(target.git.repository_id, Uuid::from_u128(2));
        // A rebinding since the adoption makes the recorded target stale.
        assert!(delivery_target(&o, 2).await.is_none());
    }
    // Replay: the same answer, no second row, no registration, same project.
    assert_eq!(adopt(&o, adopt_request(op)).await.1, v);
    assert_eq!(rows(&o).await, 1);
    assert!(o.fake.lock().unwrap().registers.is_empty());
    assert!(o.fake.lock().unwrap().syncs.is_empty());
    let (status, project) = call(&o.app, &path(), None, "owner@localhost").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(project, seeded["record"]);
    // The operation id is bound to its request.
    let mut other = adopt_request(op);
    other["github"]["name"] = "other".into();
    other["remote_url"] = "https://github.com/acme/other.git".into();
    assert_eq!(adopt(&o, other).await.0, StatusCode::CONFLICT);
    assert_eq!(
        call(
            &o.app,
            "/api/repositories/adopt",
            Some(adopt_request(op)),
            owner2
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    // Every successful reply belongs to the caller's exact operation.
    assert_eq!(
        adopt(&o, adopt_request(Uuid::new_v4())).await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(o.fake.lock().unwrap().metadata_auth, vec![None]);
    let (status, read) = call(
        &o.app,
        &format!("/api/repositories/onboard/{op}"),
        None,
        "owner@localhost",
    )
    .await;
    assert_eq!((status, read), (StatusCode::OK, v));
    assert_eq!(rows(&o).await, 1);
}

#[tokio::test]
async fn ordinary_onboarding_keeps_its_target_and_operation_identity() {
    let o = onboarding().await;
    let op = Uuid::new_v4();
    let (_, onboarded) = onboard(&o, request(&o, op)).await;
    assert_eq!(onboarded["disposition"], "bound", "{onboarded}");
    let (status, v) = adopt(&o, adopt_request(Uuid::new_v4())).await;
    assert_eq!(status, StatusCode::CONFLICT, "{v}");
    assert_eq!(adopt(&o, adopt_request(op)).await.0, StatusCode::CONFLICT);
    assert_eq!(rows(&o).await, 1);
    assert_eq!(
        delivery_target(&o, 1).await.unwrap().default_branch,
        "trunk"
    );
    assert!(delivery_target(&o, 2).await.is_none());
}

#[tokio::test]
async fn adoption_refuses_mismatches_and_records_nothing() {
    let o = onboarding().await;
    bind(&o.env).await;
    let remote = "https://github.com/acme/widgets.git";
    let body = |f: &dyn Fn(&mut Value)| {
        let mut b = adopt_request(Uuid::new_v4());
        f(&mut b);
        b
    };
    let cases: Vec<(&str, &str, Value)> = vec![
        ("https://github.com/acme/other.git", "origin", body(&|_| {})),
        (
            "https://github.com/acme/widgets",
            "same repository but different origin spelling",
            body(&|_| {}),
        ),
        (
            remote,
            "invalid URL",
            body(&|b| b["remote_url"] = "https://user@github.com/acme/widgets.git".into()),
        ),
        ("", "no origin reported", body(&|_| {})),
        (
            remote,
            "expected generation",
            body(&|b| b["expected_binding_generation"] = 2.into()),
        ),
        (
            remote,
            "other project",
            body(&|b| b["project_id"] = json!(Uuid::from_u128(9))),
        ),
        (
            remote,
            "other repository",
            body(&|b| b["repository_id"] = json!(Uuid::from_u128(9))),
        ),
        (
            remote,
            "default branch",
            body(&|b| b["default_branch"] = "main".into()),
        ),
        (
            remote,
            "other github repo",
            body(&|b| b["github"]["owner"] = "mallory".into()),
        ),
    ];
    for (origin, why, b) in cases {
        o.fake.lock().unwrap().remote = origin.into();
        let (status, v) = adopt(&o, b.clone()).await;
        // `mallory/widgets` differs from the origin; an unknown project is absent.
        let expected = match why {
            "other project" => StatusCode::NOT_FOUND,
            "other github repo" | "invalid URL" => StatusCode::BAD_REQUEST,
            _ => StatusCode::CONFLICT,
        };
        assert_eq!(status, expected, "{why}: {v}");
    }
    // aq-git moved to another generation after the project binding was written.
    {
        let mut f = o.fake.lock().unwrap();
        f.remote = remote.into();
        f.generation = 2;
    }
    let (status, v) = adopt(&o, adopt_request(Uuid::new_v4())).await;
    assert_eq!(status, StatusCode::CONFLICT, "stale aq-git generation: {v}");
    // GitHub requires signed commits on the default branch.
    {
        let mut f = o.fake.lock().unwrap();
        f.generation = 1;
        f.signing = true;
    }
    let (status, v) = adopt(&o, adopt_request(Uuid::new_v4())).await;
    assert_eq!(status, StatusCode::CONFLICT, "signing: {v}");
    assert_eq!(rows(&o).await, 0);
    assert!(delivery_target(&o, 1).await.is_none());
}

#[tokio::test]
async fn adoption_requires_matching_central_identity_and_an_allowed_existing_default_ref() {
    let o = onboarding().await;
    bind(&o.env).await;
    o.fake.lock().unwrap().remote = "https://github.com/acme/widgets.git".into();
    for (key, value) in [
        ("identity", json!("")),
        ("repository_id", json!(Uuid::new_v4())),
        ("mode", json!("worker")),
        ("object_format", json!("sha256")),
        ("allowed_refs", json!(["refs/heads/other"])),
        ("allowed_targets", json!(["refs/heads/other"])),
    ] {
        o.fake
            .lock()
            .unwrap()
            .show_overrides
            .insert(key.into(), value);
        let (status, answer) = adopt(&o, adopt_request(Uuid::new_v4())).await;
        assert_eq!(status, StatusCode::CONFLICT, "{key}: {answer}");
        o.fake.lock().unwrap().show_overrides.clear();
    }
    o.fake.lock().unwrap().absent_ref = true;
    assert_eq!(
        adopt(&o, adopt_request(Uuid::new_v4())).await.0,
        StatusCode::CONFLICT
    );
    {
        let mut f = o.fake.lock().unwrap();
        f.absent_ref = false;
        f.metadata_override =
            Some(json!({"full_name":"different/widgets","default_branch":"trunk"}));
    }
    assert_eq!(
        adopt(&o, adopt_request(Uuid::new_v4())).await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(rows(&o).await, 0);
}

#[tokio::test]
async fn adoption_fences_project_revision_and_refuses_stale_generations() {
    let o = onboarding().await;
    bind(&o.env).await;
    let arrived = Arc::new(Notify::new());
    let proceed = Arc::new(Notify::new());
    {
        let mut f = o.fake.lock().unwrap();
        f.remote = "https://github.com/acme/widgets.git".into();
        f.rules_gate = Some((arrived.clone(), proceed.clone()));
    }
    let op = Uuid::new_v4();
    let change =
        async {
            tokio::time::timeout(std::time::Duration::from_secs(5), arrived.notified())
                .await
                .expect("adoption reached GitHub");
            let (status, answer) = call(
            &o.app, &path(),
            Some(json!({"operation_id":Uuid::new_v4(),"expected_revision":1,"binding":o.env.body})),
            "owner@localhost",
        ).await;
            proceed.notify_one();
            assert_eq!(status, StatusCode::OK, "{answer}");
        };
    let ((status, answer), ()) = tokio::join!(adopt(&o, adopt_request(op)), change);
    assert_eq!(status, StatusCode::CONFLICT, "{answer}");
    assert_eq!(rows(&o).await, 0);
    assert_eq!(adopt(&o, adopt_request(op)).await.0, StatusCode::OK);

    let mut next = o.env.body.clone();
    next["repository_binding_generation"] = 2.into();
    o.env
        .generation
        .store(2, std::sync::atomic::Ordering::SeqCst);
    let (status, answer) = call(
        &o.env.app,
        &path(),
        Some(json!({"operation_id":Uuid::new_v4(),"expected_revision":2,"binding":next})),
        "owner@localhost",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(adopt(&o, adopt_request(op)).await.0, StatusCode::CONFLICT);
    assert!(delivery_target(&o, 2).await.is_none());

    o.fake.lock().unwrap().generation = 2;
    let mut new = adopt_request(Uuid::new_v4());
    new["expected_binding_generation"] = 2.into();
    let (status, answer) = adopt(&o, new).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(rows(&o).await, 2);
    assert_eq!(
        delivery_target(&o, 2).await.unwrap().default_branch,
        "trunk"
    );
}

#[tokio::test]
async fn ambiguous_historical_rows_cannot_supply_a_delivery_target_or_be_adopted() {
    let o = onboarding().await;
    let op = Uuid::new_v4();
    let (status, answer) = onboard(&o, request(&o, op)).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["disposition"], "bound");
    let duplicate = Uuid::new_v4();
    sqlx::query("INSERT INTO repository_onboardings SELECT $2,actor_id,request_digest,project_id,repository_id,step,state,created_at_ms,updated_at_ms+1 FROM repository_onboardings WHERE operation_id=$1")
        .bind(op).bind(duplicate).execute(&o.env.pool).await.unwrap();
    assert!(delivery_target(&o, 1).await.is_none());
    sqlx::query(
        "UPDATE repository_onboardings SET state=state-'binding_generation' WHERE operation_id=$1",
    )
    .bind(duplicate)
    .execute(&o.env.pool)
    .await
    .unwrap();
    assert!(delivery_target(&o, 1).await.is_none());
    assert_eq!(
        adopt(&o, adopt_request(Uuid::new_v4())).await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(rows(&o).await, 2);
}

fn cli_vault(o: &Onboarding, github: &str) -> tempfile::TempDir {
    let vault = tempfile::tempdir().unwrap();
    let fixture = o._dir.path();
    let staging = fixture.join("staging");
    let git = fixture.join("git");
    let socket = fixture.join("aq-git.sock");
    let config = include_str!("../../../../vault.example/config.toml");
    std::fs::write(
        vault.path().join("config.toml"),
        format!(
            "{config}
[source_retention]
keep_per_task = 2
max_age = 2592000
staging_root = {staging:?}
git_root = {git:?}
git_socket = {socket:?}
max_staging_bytes = 1048576
max_bundle_bytes = 1048576
bulk_concurrency = 1
incomplete_ttl_secs = 60
git_deadline_ms = 30000

{github}"
        ),
    )
    .unwrap();
    vault
}

async fn cli_adopt(
    o: &Onboarding,
    vault: &std::path::Path,
    request: &Value,
) -> std::process::Output {
    let mut db_url = reqwest::Url::parse(&std::env::var("DATABASE_URL").unwrap()).unwrap();
    db_url.set_path(o.env._db.options().get_database().unwrap());
    let mut file = tempfile::NamedTempFile::new_in(vault).unwrap();
    serde_json::to_writer(&mut file, request).unwrap();
    tokio::process::Command::new(env!("CARGO_BIN_EXE_aq-server"))
        .kill_on_drop(true)
        .env_remove("AQ_DEPLOYMENT")
        .env("DATABASE_URL", db_url.as_str())
        .arg("adopt-repository")
        .arg("--vault")
        .arg(vault)
        .args(["--owner-email", "owner@localhost", "--request"])
        .arg(file.path())
        .output()
        .await
        .unwrap()
}

fn cli_json(output: std::process::Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[tokio::test]
async fn concurrent_owner_cli_processes_and_http_replay_the_same_operation() {
    let o = onboarding().await;
    bind(&o.env).await;
    o.fake.lock().unwrap().remote = "https://github.com/acme/widgets.git".into();
    let vault = cli_vault(
        &o,
        &format!("[onboarding]\ngithub_api_url={:?}", o.api_base),
    );
    let op = Uuid::new_v4();
    let request = adopt_request(op);
    let arrived = Arc::new(Notify::new());
    let proceed = Arc::new(Notify::new());
    o.fake.lock().unwrap().rules_gate = Some((arrived.clone(), proceed.clone()));
    let retry = async {
        tokio::time::timeout(std::time::Duration::from_secs(5), arrived.notified())
            .await
            .expect("first CLI reached GitHub");
        let result = cli_adopt(&o, vault.path(), &request).await;
        proceed.notify_one();
        result
    };
    let (left, right) = tokio::join!(cli_adopt(&o, vault.path(), &request), retry);
    let answer = cli_json(left);
    assert_eq!(answer["operation_id"], json!(op));
    assert_eq!(answer, cli_json(right));
    assert_eq!(adopt(&o, request).await, (StatusCode::OK, answer));
    assert_eq!(rows(&o).await, 1);
}

#[tokio::test]
async fn competing_adoption_operations_cannot_both_record_the_same_generation() {
    let o = onboarding().await;
    bind(&o.env).await;
    let arrived = Arc::new(Notify::new());
    let proceed = Arc::new(Notify::new());
    {
        let mut f = o.fake.lock().unwrap();
        f.remote = "https://github.com/acme/widgets.git".into();
        f.rules_gate = Some((arrived.clone(), proceed.clone()));
    }
    let winner = Uuid::new_v4();
    let competing = async {
        tokio::time::timeout(std::time::Duration::from_secs(5), arrived.notified())
            .await
            .expect("adoption reached GitHub");
        let result = adopt(&o, adopt_request(winner)).await;
        proceed.notify_one();
        result
    };
    let (first, second) = tokio::join!(adopt(&o, adopt_request(Uuid::new_v4())), competing);
    assert_eq!(first.0, StatusCode::CONFLICT, "{}", first.1);
    assert_eq!(second.0, StatusCode::OK, "{}", second.1);
    assert_eq!(second.1["operation_id"], json!(winner));
    assert_eq!(rows(&o).await, 1);
}

#[tokio::test]
async fn owner_cli_uses_the_configured_repository_scoped_github_app() {
    use rsa::pkcs1::{EncodeRsaPrivateKey, LineEnding};
    let o = onboarding().await;
    bind(&o.env).await;
    o.fake.lock().unwrap().remote = "https://github.com/acme/widgets.git".into();
    let key = o._dir.path().join("app.pem");
    let rsa = rsa::RsaPrivateKey::new(&mut rand::thread_rng(), 2048).unwrap();
    std::fs::write(&key, rsa.to_pkcs1_pem(LineEnding::LF).unwrap().as_bytes()).unwrap();
    let vault = cli_vault(
        &o,
        &format!(
            r#"
[delivery.github]
app_id = 4242
private_key_file = {key:?}
api_base = {api:?}
[[delivery.github.installations]]
owner = "acme"
installation_id = 42
"#,
            api = o.api_base
        ),
    );
    let answer = cli_json(cli_adopt(&o, vault.path(), &adopt_request(Uuid::new_v4())).await);
    assert_eq!(answer["default_branch"], "trunk");
    assert_eq!(
        o.fake.lock().unwrap().metadata_auth,
        vec![Some(format!("Bearer {APP_TOKEN}"))]
    );
    assert!(!answer.to_string().contains(APP_TOKEN));
}
=======
mod adoption;
mod cli;
>>>>>>> refs/remotes/central/main
