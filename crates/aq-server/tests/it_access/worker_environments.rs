//! Spec 0017 §6.3, §6.6: builder nodes (the operator's `template-build`
//! worker group) claim and complete environment jobs; every enrolled node
//! reads the admitted environments and reports which it has pulled.
use std::sync::Arc;

use aq_environments::worker::{NewJob, enqueue};
use aq_server::config::Config;
use aq_server::identity::verifier_from_config;
use aq_server::test_support::TestDb;
use aq_server::tokens::verifier;
use aq_server::{AppState, build_worker_router};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

const BUILDER: &str = "aqn_builder";
const SECOND_BUILDER: &str = "aqn_second_builder";
const NODE: &str = "aqn_plain_node";
const DIGEST: &str = "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

struct Harness {
    worker: Router,
    // The git tests serve `/v1/git` from it.
    state: AppState,
    pool: PgPool,
    project: Uuid,
    repository: Uuid,
    nodes: [Uuid; 3],
    _db: TestDb,
}

async fn harness() -> Harness {
    let db = TestDb::create().await;
    let store = db.store().await;
    let pool = db.pool().await;
    let nodes = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
    for (id, token) in nodes.iter().zip([BUILDER, SECOND_BUILDER, NODE]) {
        sqlx::query("INSERT INTO nodes(id,name,credential_hash,worker_version,protocol_version,enrolled_at) VALUES($1,$2,$3,'0.3.3',2,now())")
            .bind(id).bind(token).bind(verifier(token)).execute(&pool).await.unwrap();
    }
    let (project, repository) = (Uuid::new_v4(), Uuid::new_v4());
    sqlx::query("INSERT INTO project_execution_bindings(project_id,revision,binding,validated_at_ms,binding_observation) VALUES($1,1,$2,1,'{}')")
        .bind(project).bind(json!({"name": "example", "repository_id": repository})).execute(&pool).await.unwrap();
    let config: Config = toml::from_str(&format!(
        r#"
format_version = 1
environment = "development"
[server]
admin_bind = "127.0.0.1:0"
worker_bind = "127.0.0.1:0"
public_origin = "http://127.0.0.1:8080"
[auth]
mode = "dev"
dev_email = "owner@localhost"
[bootstrap]
owner_email = "owner@localhost"
enabled = true
[projects.worker_groups]
template-build = ["{}", "{}"]
"#,
        nodes[0], nodes[1]
    ))
    .unwrap();
    let state = AppState {
        internal_work: Default::default(),
        agent_access: None,
        source_retention: None,
        identity: verifier_from_config(&config.auth, reqwest::Client::new()),
        config: Arc::new(config),
        store,
        clock: Arc::new(chrono::Utc::now),
    };
    Harness {
        worker: build_worker_router(state.clone()),
        state,
        pool,
        project,
        repository,
        nodes,
        _db: db,
    }
}

async fn send(h: &Harness, path: &str, bearer: &str, body: Option<Value>) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(if body.is_some() { "POST" } else { "GET" })
        .uri(path)
        .header("authorization", format!("Bearer {bearer}"))
        .header("content-type", "application/json");
    let request = request
        .body(body.map_or(Body::empty(), |b| Body::from(b.to_string())))
        .unwrap();
    let response = h.worker.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn job(h: &Harness, kind: &str) -> Uuid {
    let recipes = vec!["web".to_string()];
    let job = NewJob {
        kind,
        project_id: h.project,
        commit: &"c".repeat(40),
        recipes: &recipes,
        run: (kind == "try").then_some("pnpm test"),
        base_template: &format!("registry.example/aq-template@{DIGEST}"),
    };
    enqueue(&h.pool, &job).await.unwrap()
}

fn built(job: Uuid) -> Value {
    json!({"job_id": job, "outcome": "built", "failure": null, "run": null, "builds": [{
        "name": "web", "digest": DIGEST, "image": format!("registry.example/aq-template@{DIGEST}"),
        "recipe_sha256": "e".repeat(64), "provides": ["node"],
        "toolchain": [{"command": "node --version", "output": "v24.0.0"}],
        "warm": ["pnpm install --frozen-lockfile"],
        "harnesses": [{"harness": "claude", "version": "2.1.0"}], "default": true}]})
}

#[tokio::test]
async fn only_a_template_build_node_can_claim() {
    let h = harness().await;
    let id = job(&h, "build").await;
    let (status, body) = send(&h, "/v1/worker/environments/claim", NODE, Some(json!({}))).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"], "forbidden");
    let complete = send(
        &h,
        "/v1/worker/environments/complete",
        NODE,
        Some(built(id)),
    )
    .await;
    assert_eq!(complete.0, StatusCode::FORBIDDEN, "{}", complete.1);
    let none = send(
        &h,
        "/v1/worker/environments/claim",
        "aqn_unknown",
        Some(json!({})),
    )
    .await;
    assert_eq!(none.0, StatusCode::UNAUTHORIZED);

    let (status, body) = send(
        &h,
        "/v1/worker/environments/claim",
        BUILDER,
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({
            "job_id": id, "kind": "build", "project_id": h.project, "project": "example",
            "repository_id": h.repository, "commit": "c".repeat(40), "recipes": ["web"], "run": null,
            "base_template": format!("registry.example/aq-template@{DIGEST}"),
            "limits": {"max_install_steps": 64, "max_step_args": 32, "max_arg_bytes": 4096,
                "max_prefetch": 16, "max_recipe_bytes": 1048576, "build_wall_ms": 3600000,
                "build_cpu_millis": 4000, "build_memory_bytes": 8589934592u64, "log_bytes": 65536,
                "try_concurrency": 2, "keep_versions": 3},
        })
    );
}

#[tokio::test]
async fn a_job_is_claimed_once() {
    let h = harness().await;
    let id = job(&h, "try").await;
    let claim = |token| send(&h, "/v1/worker/environments/claim", token, Some(json!({})));
    let (status, first) = claim(BUILDER).await;
    assert_eq!((status, &first["job_id"]), (StatusCode::OK, &json!(id)));
    assert_eq!(first["run"], "pnpm test");
    // Nothing else is queued: the second builder gets no job.
    assert_eq!(claim(SECOND_BUILDER).await.0, StatusCode::NO_CONTENT);
    assert_eq!(claim(BUILDER).await.0, StatusCode::NO_CONTENT);

    // The lease (`build_wall_ms`) runs out: another builder may take the job,
    // and the first one's late completion is refused.
    sqlx::query(
        "UPDATE environment_jobs SET lease_until=now()-interval '1 second' WHERE job_id=$1",
    )
    .bind(id)
    .execute(&h.pool)
    .await
    .unwrap();
    let (status, again) = claim(SECOND_BUILDER).await;
    assert_eq!((status, &again["job_id"]), (StatusCode::OK, &json!(id)));
    let late = json!({"job_id": id, "outcome": "built", "builds": [], "failure": null,
        "run": {"exit": 0, "output": "ok"}});
    let (status, body) = send(
        &h,
        "/v1/worker/environments/complete",
        BUILDER,
        Some(late.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let ok = send(
        &h,
        "/v1/worker/environments/complete",
        SECOND_BUILDER,
        Some(late),
    )
    .await;
    assert_eq!(ok.0, StatusCode::OK, "{}", ok.1);
    let claimed: Uuid =
        sqlx::query_scalar("SELECT claimed_by FROM environment_jobs WHERE job_id=$1")
            .bind(id)
            .fetch_one(&h.pool)
            .await
            .unwrap();
    assert_eq!(claimed, h.nodes[1]);
}

#[tokio::test]
async fn complete_records_the_result_once_and_replays_the_same_answer() {
    let h = harness().await;
    let id = job(&h, "build").await;
    let complete = |token, body| send(&h, "/v1/worker/environments/complete", token, Some(body));
    // Not claimed yet: nothing to complete.
    assert_eq!(complete(BUILDER, built(id)).await.0, StatusCode::CONFLICT);
    let unknown = complete(BUILDER, built(Uuid::new_v4())).await;
    assert_eq!(unknown.0, StatusCode::NOT_FOUND);
    send(
        &h,
        "/v1/worker/environments/claim",
        BUILDER,
        Some(json!({})),
    )
    .await;
    // A failed outcome must name the failure.
    let mut malformed = built(id);
    malformed["outcome"] = json!("failed");
    assert_eq!(
        complete(BUILDER, malformed).await.0,
        StatusCode::BAD_REQUEST
    );

    let (status, first) = complete(BUILDER, built(id)).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first, json!({"job_id": id, "recorded": true}));
    let (status, replay) = complete(BUILDER, built(id)).await;
    assert_eq!((status, replay), (StatusCode::OK, first));
    let mut other = built(id);
    other["builds"][0]["name"] = json!("api");
    assert_eq!(complete(BUILDER, other).await.0, StatusCode::CONFLICT);

    let (state, result): (String, Value) =
        sqlx::query_as("SELECT state,result FROM environment_jobs WHERE job_id=$1")
            .bind(id)
            .fetch_one(&h.pool)
            .await
            .unwrap();
    assert_eq!((state.as_str(), result), ("completed", built(id)));
    // A completed job is never handed out again.
    let claim = send(
        &h,
        "/v1/worker/environments/claim",
        BUILDER,
        Some(json!({})),
    )
    .await;
    assert_eq!(claim.0, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn an_admitted_environment_is_offered_only_after_a_node_reports_it_ready() {
    let h = harness().await;
    sqlx::query("INSERT INTO environment_versions(environment_id,version,project_id,template_digest,image,provides,harnesses,warm) \
                 VALUES('example.web','1',$1,$2,'registry.example/aq-template@'||$2,'[\"node\"]','[{\"harness\":\"claude\",\"version\":\"2.1.0\"}]','[\"pnpm install\"]')")
        .bind(h.project).bind(DIGEST).execute(&h.pool).await.unwrap();
    // A node with a live inventory that lists no harness for the environment.
    let expires = chrono::Utc::now().timestamp_millis() + 600_000;
    sqlx::query("INSERT INTO intelligence_node_snapshots(node_id,boot_id,sequence,revision,snapshot) VALUES($1,$2,1,1,$3)")
        .bind(h.nodes[2]).bind(Uuid::new_v4()).bind(json!({"expires_at_ms": expires, "harnesses": []}))
        .execute(&h.pool).await.unwrap();

    // Every enrolled node reads the admitted environments.
    let (status, body) = send(&h, "/v1/worker/environments", NODE, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let entry = &body["environments"][0];
    assert_eq!(body["environments"].as_array().unwrap().len(), 1);
    assert_eq!(
        (&entry["id"], &entry["version"], &entry["digest"]),
        (&json!("example.web"), &json!("1"), &json!(DIGEST))
    );
    assert_eq!(
        entry["image"],
        format!("registry.example/aq-template@{DIGEST}")
    );
    assert_eq!(
        entry["harnesses"],
        json!([{"harness": "claude", "version": "2.1.0"}])
    );
    assert_eq!(entry["warm"], json!(["pnpm install"]));
    // `definition` is the reference a node lists for placement: the catalog
    // resolves exactly it, so the offer path can retain its body.
    let definition: aq_admission::ArtifactRef =
        serde_json::from_value(entry["definition"].clone()).unwrap();
    let boot = aq_environments::BootEnvironments::new([], None);
    let catalog = aq_environments::Catalog::load(&h.pool, &boot)
        .await
        .unwrap();
    assert!(catalog.definition(&definition).is_some());

    let listed = |e| aq_role_settings::environments::listed(&h.pool, e, expires - 1000);
    assert!(!listed("example.web").await, "no node has reported it");
    let ready = |digests: Value| {
        send(
            &h,
            "/v1/worker/environments/ready",
            NODE,
            Some(json!({"ready": digests})),
        )
    };
    let other = format!("sha256:{}", "f".repeat(64));
    let (status, body) = ready(json!([DIGEST, other])).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({"ready": [DIGEST]}),
        "only admitted digests are kept"
    );
    let stored: Value =
        sqlx::query_scalar("SELECT digests FROM environment_ready WHERE node_id=$1")
            .bind(h.nodes[2])
            .fetch_one(&h.pool)
            .await
            .unwrap();
    assert_eq!(stored, json!([DIGEST]));
    // The node lists it from its next inventory on (the inventory writer adds
    // its harness entries: `it_native native_offers::environments`).
    assert!(!listed("example.web").await);
    // Rolling: each report replaces the node's previous set.
    ready(json!([])).await;
    let stored: Value =
        sqlx::query_scalar("SELECT digests FROM environment_ready WHERE node_id=$1")
            .bind(h.nodes[2])
            .fetch_one(&h.pool)
            .await
            .unwrap();
    assert_eq!(stored, json!([]));
    assert_eq!(
        send(&h, "/v1/worker/environments", "aqn_unknown", None)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
}

async fn limits(h: &Harness, limits: Value) {
    let mut all = json!({"max_install_steps": 64, "max_step_args": 32, "max_arg_bytes": 4096,
        "max_prefetch": 16, "max_recipe_bytes": 1048576, "build_wall_ms": 3600000,
        "build_cpu_millis": 4000, "build_memory_bytes": 8589934592u64, "log_bytes": 65536,
        "try_concurrency": 2, "keep_versions": 3, "max_job_claims": 3});
    for (k, v) in limits.as_object().unwrap() {
        all[k] = v.clone();
    }
    sqlx::query("UPDATE role_settings SET environment_limits=$1 WHERE singleton")
        .bind(all)
        .execute(&h.pool)
        .await
        .unwrap();
}
async fn claimed(h: &Harness, kind: &str) -> Uuid {
    let id = job(h, kind).await;
    let (status, body) = send(h, "/v1/worker/environments/claim", BUILDER, Some(json!({}))).await;
    assert_eq!((status, &body["job_id"]), (StatusCode::OK, &json!(id)));
    id
}
fn failed(job: Uuid, log: String) -> Value {
    json!({"job_id": job, "outcome": "failed", "builds": [], "run": null,
        "failure": {"recipe": "web", "step": "install", "log": log}})
}

/// The transport bound on a result comes from the Settings (`log_bytes`,
/// `max_recipe_bytes`), and a result over it is refused in the ApiError shape.
#[tokio::test]
async fn a_result_over_the_bound_from_the_settings_is_refused() {
    let h = harness().await;
    limits(&h, json!({"log_bytes": 1000, "max_recipe_bytes": 1000})).await;
    let complete = |body| send(&h, "/v1/worker/environments/complete", BUILDER, Some(body));
    let id = claimed(&h, "build").await;
    // 6 x (1000 + 1000) bytes on the wire at most.
    let (status, body) = complete(failed(id, "x".repeat(20_000))).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert_eq!(body["error"], "bad_request", "{body}");
    assert!(
        body["message"].as_str().unwrap().contains("12000"),
        "{body}"
    );
    // Under the transport bound but over `log_bytes`: refused as invalid.
    assert_eq!(
        complete(failed(id, "x".repeat(1001))).await.0,
        StatusCode::BAD_REQUEST
    );
    let (status, body) = complete(failed(id, "x".repeat(1000))).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Raised in the Settings, the same large result is accepted.
    limits(&h, json!({"log_bytes": 20_000})).await;
    let id = claimed(&h, "build").await;
    let (status, body) = complete(failed(id, "x".repeat(20_000))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn a_result_must_fit_its_job() {
    let h = harness().await;
    limits(&h, json!({"log_bytes": 100})).await;
    let complete = |body| send(&h, "/v1/worker/environments/complete", BUILDER, Some(body));
    let build = claimed(&h, "build").await;
    // A failed build reports no builds.
    let mut with_builds = failed(build, "boom".into());
    with_builds["builds"] = built(build)["builds"].clone();
    assert_eq!(complete(with_builds).await.0, StatusCode::BAD_REQUEST);
    // Toolchain output counts against `log_bytes`.
    let mut long = built(build);
    long["builds"][0]["toolchain"][0]["output"] = json!("v".repeat(101));
    assert_eq!(complete(long).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(complete(built(build)).await.0, StatusCode::OK);

    // A built try with a command reports its run, and no builds.
    let tried = claimed(&h, "try").await;
    let no_run =
        json!({"job_id": tried, "outcome": "built", "builds": [], "failure": null, "run": null});
    assert_eq!(complete(no_run).await.0, StatusCode::BAD_REQUEST);
    let mut with_builds = built(tried);
    with_builds["run"] = json!({"exit": 0, "output": "ok"});
    assert_eq!(complete(with_builds).await.0, StatusCode::BAD_REQUEST);
    let ran = json!({"job_id": tried, "outcome": "built", "builds": [], "failure": null,
        "run": {"exit": 0, "output": "ok"}});
    assert_eq!(complete(ran).await.0, StatusCode::OK);
}

/// Rolling records: a project keeps its newest `keep_versions` completed
/// jobs of each kind.
#[tokio::test]
async fn completing_more_than_keep_versions_jobs_keeps_only_the_newest() {
    let h = harness().await;
    limits(&h, json!({"keep_versions": 2})).await;
    let tried = claimed(&h, "try").await;
    let ran = json!({"job_id": tried, "outcome": "failed", "builds": [], "run": null,
        "failure": {"recipe": "web", "step": "run", "log": "no"}});
    send(&h, "/v1/worker/environments/complete", BUILDER, Some(ran)).await;
    let mut done = vec![];
    for _ in 0..3 {
        let id = claimed(&h, "build").await;
        let (status, body) = send(
            &h,
            "/v1/worker/environments/complete",
            BUILDER,
            Some(built(id)),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        done.push(id);
    }
    let left: Vec<Uuid> = sqlx::query_scalar(
        "SELECT job_id FROM environment_jobs WHERE kind='build' ORDER BY completed_at",
    )
    .fetch_all(&h.pool)
    .await
    .unwrap();
    assert_eq!(left, done[1..]);
    let tries: i64 = sqlx::query_scalar("SELECT count(*) FROM environment_jobs WHERE kind='try'")
        .fetch_one(&h.pool)
        .await
        .unwrap();
    assert_eq!(tries, 1, "each kind keeps its own");
}

/// The claimed Job names the project's repository as nodes key it: the
/// binding's `repository_id`, stored on the job when it was queued.
#[tokio::test]
async fn the_claim_names_the_jobs_repository() {
    let h = harness().await;
    let id = job(&h, "try").await;
    let stored: Uuid =
        sqlx::query_scalar("SELECT repository_id FROM environment_jobs WHERE job_id=$1")
            .bind(id)
            .fetch_one(&h.pool)
            .await
            .unwrap();
    assert_eq!(stored, h.repository);
    let (status, body) = send(
        &h,
        "/v1/worker/environments/claim",
        BUILDER,
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["repository_id"], json!(h.repository), "{body}");
}

/// The Job's `limits` are exactly the fields the builder's template tool
/// takes (its `LIMIT_FIELDS`): the Setting without the server's own
/// `max_job_claims`, as before that limit existed.
#[tokio::test]
async fn the_jobs_limits_are_the_fields_the_builder_takes() {
    let h = harness().await;
    job(&h, "build").await;
    let (status, body) = send(
        &h,
        "/v1/worker/environments/claim",
        BUILDER,
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let mut sent: Vec<&str> = body["limits"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    sent.sort_unstable();
    assert_eq!(
        sent,
        [
            "build_cpu_millis",
            "build_memory_bytes",
            "build_wall_ms",
            "keep_versions",
            "log_bytes",
            "max_arg_bytes",
            "max_install_steps",
            "max_prefetch",
            "max_recipe_bytes",
            "max_step_args",
            "try_concurrency"
        ]
    );
}

/// A job claimed `max_job_claims` times whose lease ran out each time is
/// completed by the server as failed at `fetch`, never handed out again.
#[tokio::test]
async fn a_job_claimed_more_than_max_job_claims_times_completes_failed() {
    let h = harness().await;
    limits(&h, json!({"max_job_claims": 2})).await;
    let id = job(&h, "try").await;
    let claim = |token| send(&h, "/v1/worker/environments/claim", token, Some(json!({})));
    let expire = || {
        sqlx::query(
            "UPDATE environment_jobs SET lease_until=now()-interval '1 second' WHERE job_id=$1",
        )
        .bind(id)
        .execute(&h.pool)
    };
    for token in [BUILDER, SECOND_BUILDER] {
        let (status, body) = claim(token).await;
        assert_eq!((status, &body["job_id"]), (StatusCode::OK, &json!(id)));
        expire().await.unwrap();
    }
    assert_eq!(claim(BUILDER).await.0, StatusCode::NO_CONTENT);
    let (state, result): (String, Value) =
        sqlx::query_as("SELECT state,result FROM environment_jobs WHERE job_id=$1")
            .bind(id)
            .fetch_one(&h.pool)
            .await
            .unwrap();
    assert_eq!(state, "completed");
    let log = result["failure"]["log"].as_str().unwrap().to_owned();
    assert!(log.contains("claimed 2 times without a result"), "{log}");
    assert_eq!(
        result,
        json!({"job_id": id, "outcome": "failed", "builds": [], "run": null,
            "failure": {"recipe": "web", "step": "fetch", "log": log}})
    );
    // The last claimer's late result is not the recorded one.
    let late = json!({"job_id": id, "outcome": "built", "builds": [], "failure": null,
        "run": {"exit": 0, "output": "ok"}});
    let (status, body) = send(
        &h,
        "/v1/worker/environments/complete",
        SECOND_BUILDER,
        Some(late),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
}

/// The git route on the harness's server, serving aq-git through `fake`.
fn git_router(h: &Harness, fake: &aq_delivery::git_grant::fake::FakeGitHttp) -> Router {
    let mut state = h.state.clone();
    let mut config = (*state.config).clone();
    config.delivery = Some(aq_delivery::github::DeliveryConfig {
        git_http_socket: Some(fake.socket.clone()),
        ..Default::default()
    });
    state.config = Arc::new(config);
    build_worker_router(state)
}
/// A job for `commit` claimed by BUILDER.
async fn claimed_at(h: &Harness, commit: &str) -> Uuid {
    let recipes = vec!["web".to_string()];
    let job = NewJob {
        kind: "build",
        project_id: h.project,
        commit,
        recipes: &recipes,
        run: None,
        base_template: &format!("registry.example/aq-template@{DIGEST}"),
    };
    let id = enqueue(&h.pool, &job).await.unwrap();
    let (status, body) = send(h, "/v1/worker/environments/claim", BUILDER, Some(json!({}))).await;
    assert_eq!((status, &body["job_id"]), (StatusCode::OK, &json!(id)));
    id
}

/// Spec 0017 §6.3: the builder fetches the job's commit through `/v1/git`
/// with its node credential and `AQ-Environment-Job` instead of an attempt.
#[tokio::test]
async fn a_claim_holder_can_fetch_its_jobs_repository() {
    use aq_delivery::git_grant::fake::{FakeGitHttp, git};
    let h = harness().await;
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeGitHttp::start(dir.path(), h.repository, "refs/heads/main").await;
    let id = claimed_at(&h, &fake.base).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/v1/git/{}",
        listener.local_addr().unwrap(),
        h.repository
    );
    let router = git_router(&h, &fake);
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let work = dir.path().join("work");
    std::fs::create_dir(&work).unwrap();
    git(&work, &["init", "-q"]);
    let out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(&work)
        .args([
            "-c",
            &format!("http.extraHeader=Authorization: Bearer {BUILDER}"),
        ])
        .args(["-c", &format!("http.extraHeader=AQ-Environment-Job: {id}")])
        .args(["fetch", "-q", &url, "refs/heads/main"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "fetch: {out:?}");
    assert_eq!(git(&work, &["rev-parse", "FETCH_HEAD"]), fake.base);
    let seen = fake.seen.lock().unwrap().clone();
    assert!(
        seen.len() >= 2,
        "advertisement and upload-pack: {}",
        seen.len()
    );
    for headers in &seen {
        let one = |name: &str| headers.get(name).map(|v| v.to_str().unwrap().to_owned());
        assert_eq!(one("aq-repository"), Some(h.repository.to_string()));
        assert_eq!(one("aq-base"), Some(fake.base.clone()));
        assert!(one("aq-task-ref").unwrap().starts_with("refs/heads/aq/"));
        assert!(one("authorization").is_none() && one("aq-environment-job").is_none());
    }
}

/// The job grant is fetch-only, for the job's repository, for the node that
/// holds the live claim: everything else is refused before aq-git.
#[tokio::test]
async fn the_job_grant_refuses_push_other_repositories_other_nodes_and_expired_claims() {
    use aq_delivery::git_grant::fake::FakeGitHttp;
    let h = harness().await;
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeGitHttp::start(dir.path(), h.repository, "refs/heads/main").await;
    let id = claimed_at(&h, &fake.base).await;
    let router = git_router(&h, &fake);
    let request = |method: &str, repository: Uuid, path: &str, token: &str, job: Uuid| {
        let request = Request::builder()
            .method(method)
            .uri(format!("/v1/git/{repository}/{path}"))
            .header("authorization", format!("Bearer {token}"))
            .header("aq-environment-job", job.to_string())
            .body(Body::empty())
            .unwrap();
        let router = router.clone();
        async move { router.oneshot(request).await.unwrap().status() }
    };
    let fetch = "info/refs?service=git-upload-pack";
    assert_eq!(
        request("GET", h.repository, fetch, BUILDER, id).await,
        StatusCode::OK
    );
    let reached = fake.seen.lock().unwrap().len();
    let other = Uuid::new_v4();
    for (method, repository, path, token, job) in [
        (
            "GET",
            h.repository,
            "info/refs?service=git-receive-pack",
            BUILDER,
            id,
        ),
        ("POST", h.repository, "git-receive-pack", BUILDER, id),
        ("GET", other, fetch, BUILDER, id),
        ("GET", h.repository, fetch, SECOND_BUILDER, id),
        ("GET", h.repository, fetch, NODE, id),
        ("GET", h.repository, fetch, BUILDER, Uuid::new_v4()),
    ] {
        let status = request(method, repository, path, token, job).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{method} {repository}/{path} {token}"
        );
    }
    // An expired claim, and a completed job, grant nothing.
    sqlx::query(
        "UPDATE environment_jobs SET lease_until=now()-interval '1 second' WHERE job_id=$1",
    )
    .bind(id)
    .execute(&h.pool)
    .await
    .unwrap();
    assert_eq!(
        request("GET", h.repository, fetch, BUILDER, id).await,
        StatusCode::FORBIDDEN
    );
    sqlx::query("UPDATE environment_jobs SET lease_until=now()+interval '1 hour',state='completed' WHERE job_id=$1")
        .bind(id)
        .execute(&h.pool)
        .await
        .unwrap();
    assert_eq!(
        request("GET", h.repository, fetch, BUILDER, id).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        fake.seen.lock().unwrap().len(),
        reached,
        "nothing refused reached aq-git"
    );
}
