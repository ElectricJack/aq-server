//! Spec 0017 §6.2 (feature `project_environments`): `environment.try` queues a
//! `try` job for the commit the attempt's node pushed and attested. The node's
//! channel proxy commits and pushes the workspace to `refs/heads/aq/<task>`
//! and adds `"commit"` to the agent's body; without it the request is refused
//! with `workspace_not_pushed`.
use crate::messaging::{Env, send, setup_with};
use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

const TRY: &str = "/v1/agent/environments/try";
const BASE: &str = "registry.example/aq-base@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn granted() -> Option<Value> {
    Some(json!([{"operation":"environment.try","scope":"project:self"}]))
}
fn project(e: &Env) -> Uuid {
    e.binding["project_id"].as_str().unwrap().parse().unwrap()
}
async fn post(e: &Env, body: Value) -> (StatusCode, Value) {
    send(&e.app, TRY, Some(&e.token), Some(body)).await
}
async fn job(e: &Env, id: &Value) -> (StatusCode, Value) {
    let path = format!("{TRY}/{}", id.as_str().unwrap());
    send(&e.app, &path, Some(&e.token), None).await
}
fn attested(name: &str) -> Value {
    json!({"api_version":1,"name":name,"run":"pnpm test","commit":"c".repeat(40)})
}
/// The AQ base template: an admitted version with no project. The task's
/// project gets its execution binding, whose repository a job fetches from.
async fn base(e: &Env) {
    let digest = BASE.split_once('@').unwrap().1;
    sqlx::query("INSERT INTO environment_versions(environment_id,version,template_digest,image) VALUES('aq-base','1',$1,$2)")
        .bind(digest).bind(BASE).execute(&e.pool).await.unwrap();
    bound(e, project(e)).await;
}
/// A project execution binding of `project`, with repository `project`.
async fn bound(e: &Env, project: Uuid) {
    sqlx::query("INSERT INTO project_execution_bindings(project_id,revision,binding,validated_at_ms,binding_observation) VALUES($1,1,$2,1,'{}')")
        .bind(project).bind(json!({"name": "example", "repository_id": project})).execute(&e.pool).await.unwrap();
}
async fn limits(e: &Env, try_concurrency: u64) {
    let limits = json!({"max_install_steps": 64, "max_step_args": 32, "max_arg_bytes": 4096,
        "max_prefetch": 16, "max_recipe_bytes": 1048576, "build_wall_ms": 3600000,
        "build_cpu_millis": 4000, "build_memory_bytes": 8589934592u64, "log_bytes": 65536,
        "try_concurrency": try_concurrency, "keep_versions": 3, "max_job_claims": 3});
    sqlx::query("UPDATE role_settings SET environment_limits=$1 WHERE singleton")
        .bind(limits)
        .execute(&e.pool)
        .await
        .unwrap();
}
async fn queued(e: &Env) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM environment_jobs")
        .fetch_one(&e.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn environment_try_queues_a_try_job_for_the_tasks_head() {
    let e = setup_with(granted()).await;
    // No AQ base template admitted yet: nothing to build on.
    let (status, body) = post(&e, attested("web")).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "no_base_template", "{body}");
    base(&e).await;
    let (status, body) = post(&e, attested("web")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let id = body["job_id"].clone();
    type Row = (
        String,
        Uuid,
        Uuid,
        String,
        Value,
        Option<String>,
        String,
        String,
    );
    let row: Row = sqlx::query_as(
        "SELECT kind,project_id,repository_id,commit,recipes,run,base_template,state FROM environment_jobs WHERE job_id=$1",
    )
    .bind(Uuid::parse_str(id.as_str().unwrap()).unwrap())
    .fetch_one(&e.pool)
    .await
    .unwrap();
    let expected = (
        "try".into(),
        project(&e),
        project(&e),
        "c".repeat(40),
        json!(["web"]),
        Some("pnpm test".into()),
        BASE.into(),
        "queued".into(),
    );
    assert_eq!(row, expected);
    assert_eq!(
        job(&e, &id).await,
        (
            StatusCode::OK,
            json!({"job_id": id, "state": "queued", "result": null})
        )
    );
    // The builder's result is what the agent reads.
    let result = json!({"job_id": id, "outcome": "built", "builds": [], "failure": null,
        "run": {"exit": 0, "output": "ok"}});
    sqlx::query("UPDATE environment_jobs SET state='completed',completed_at=now(),result=$2 WHERE job_id=$1")
        .bind(Uuid::parse_str(id.as_str().unwrap()).unwrap())
        .bind(&result)
        .execute(&e.pool)
        .await
        .unwrap();
    let (status, body) = job(&e, &id).await;
    assert_eq!(
        (status, &body["state"], &body["result"]),
        (StatusCode::OK, &json!("completed"), &result)
    );
    // A try without a run command builds the image only.
    let (status, body) = post(
        &e,
        json!({"api_version":1,"name":"web","commit":"d".repeat(40)}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // The node attests the pushed commit; nothing else does.
    let (status, body) = post(&e, json!({"api_version":1,"name":"web"})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "workspace_not_pushed", "{body}");
    let surface = json!({"params":{"name":"web"}});
    let (status, body) = send(
        &e.app,
        "/api/v1/commands/environment.try",
        Some(&e.token),
        Some(surface),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "workspace_not_pushed", "{body}");
    // Through the surface the agent wrote the body, so it cannot attest one.
    let surface = json!({"params":{"name":"web","commit":"e".repeat(40)}});
    let (status, body) = send(
        &e.app,
        "/api/v1/commands/environment.try",
        Some(&e.token),
        Some(surface),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "invalid_request", "{body}");
    assert!(body["message"].as_str().unwrap().contains("node"), "{body}");
    for bad in [
        json!({"api_version":1,"name":"web","commit":"C".repeat(40)}),
        json!({"api_version":1,"name":"web","commit":"c".repeat(39)}),
        json!({"api_version":1,"name":"Web","commit":"c".repeat(40)}),
        json!({"api_version":1,"name":"-web","commit":"c".repeat(40)}),
        json!({"api_version":1,"name":"w".repeat(65),"commit":"c".repeat(40)}),
        json!({"api_version":1,"name":"web","commit":"c".repeat(40),"recipes":["x"]}),
        json!({"api_version":1,"name":"web","run":"x".repeat(4097),"commit":"c".repeat(40)}),
    ] {
        let (status, body) = post(&e, bad.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad} {body}");
        assert_eq!(body["code"], "invalid_request", "{body}");
    }
    let (status, _) = post(
        &e,
        json!({"api_version":2,"name":"web","commit":"c".repeat(40)}),
    )
    .await;
    assert_eq!(status, StatusCode::UPGRADE_REQUIRED);
    assert_eq!(queued(&e).await, 2);
    assert_eq!(
        job(&e, &json!(Uuid::new_v4())).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        job(&e, &json!("not-a-uuid")).await.0,
        StatusCode::BAD_REQUEST
    );
    let (status, _) = send(&e.app, TRY, None, Some(attested("web"))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn environment_try_needs_the_grant() {
    let e = setup_with(Some(
        json!([{"operation":"comment.read","scope":"project:self"}]),
    ))
    .await;
    base(&e).await;
    let (status, body) = post(&e, attested("web")).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("environment.try"),
        "{body}"
    );
    let (status, body) = job(&e, &json!(Uuid::new_v4())).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let surface = json!({"params":{"name":"web"}});
    let (status, body) = send(
        &e.app,
        "/api/v1/commands/environment.try",
        Some(&e.token),
        Some(surface),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(
        (&body["code"], &body["grant"]),
        (&json!("grant_missing"), &json!("environment.try"))
    );
    assert_eq!(queued(&e).await, 0);
    // Below the project, the grant does not reach the project's builds.
    let e = setup_with(Some(
        json!([{"operation":"environment.try","scope":"task:self"}]),
    ))
    .await;
    base(&e).await;
    assert_eq!(post(&e, attested("web")).await.0, StatusCode::FORBIDDEN);
    assert_eq!(queued(&e).await, 0);
    // The coding default holds it.
    let coding = aq_agent_grants::resolve::default_coding();
    let held = coding.get("environment.try").map(|g| g.scope.to_string());
    assert_eq!(held.as_deref(), Some("project:self"));
}

#[tokio::test]
async fn environment_try_beyond_the_concurrency_limit_is_refused() {
    let e = setup_with(granted()).await;
    base(&e).await;
    limits(&e, 2).await;
    let first = post(&e, attested("web")).await.1["job_id"].clone();
    // A claimed job still counts.
    sqlx::query("UPDATE environment_jobs SET state='claimed',claimed_at=now(),lease_until=now() WHERE job_id=$1")
        .bind(Uuid::parse_str(first.as_str().unwrap()).unwrap())
        .execute(&e.pool)
        .await
        .unwrap();
    assert_eq!(post(&e, attested("api")).await.0, StatusCode::OK);
    let (status, body) = post(&e, attested("web")).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["code"], "try_concurrency_reached", "{body}");
    assert_eq!(body["retryable"], true);
    assert_eq!(queued(&e).await, 2, "a refused try is not queued");
    // Another project's tries do not count against this one.
    sqlx::query("UPDATE environment_jobs SET project_id=$1")
        .bind(Uuid::new_v4())
        .execute(&e.pool)
        .await
        .unwrap();
    assert_eq!(post(&e, attested("web")).await.0, StatusCode::OK);
    // The limit is the Setting's: a completed job frees its place.
    limits(&e, 1).await;
    assert_eq!(
        post(&e, attested("web")).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
    sqlx::query("UPDATE environment_jobs SET state='completed',completed_at=now(),result='{}' WHERE project_id=$1")
        .bind(project(&e))
        .execute(&e.pool)
        .await
        .unwrap();
    assert_eq!(post(&e, attested("web")).await.0, StatusCode::OK);
}

#[tokio::test]
async fn environment_try_for_another_project_is_refused() {
    let e = setup_with(granted()).await;
    base(&e).await;
    let other = Uuid::new_v4();
    let surface = json!({"params":{"name":"web","project_id":other}});
    let (status, body) = send(
        &e.app,
        "/api/v1/commands/environment.try",
        Some(&e.token),
        Some(surface),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], "scope_denied", "{body}");
    // The project is the binding's: a body cannot name one.
    let mut named = attested("web");
    named["project_id"] = json!(other);
    assert_eq!(post(&e, named).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(queued(&e).await, 0);
    // Another project's job is not found, as an unknown one.
    let recipes = vec!["web".to_string()];
    bound(&e, other).await;
    let theirs = aq_environments::worker::enqueue(
        &e.pool,
        &aq_environments::worker::NewJob {
            kind: "try",
            project_id: other,
            commit: &"c".repeat(40),
            recipes: &recipes,
            run: None,
            base_template: BASE,
        },
    )
    .await
    .unwrap();
    let (status, body) = job(&e, &json!(theirs)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let surface = json!({"params":{"job_id":theirs}});
    let (status, _) = send(
        &e.app,
        "/api/v1/commands/environment.job.show",
        Some(&e.token),
        Some(surface),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let surface = json!({"params":{"job_id":theirs,"project_id":other}});
    let (status, body) = send(
        &e.app,
        "/api/v1/commands/environment.job.show",
        Some(&e.token),
        Some(surface),
    )
    .await;
    assert_eq!(
        (status, &body["code"]),
        (StatusCode::FORBIDDEN, &json!("scope_denied"))
    );
}
