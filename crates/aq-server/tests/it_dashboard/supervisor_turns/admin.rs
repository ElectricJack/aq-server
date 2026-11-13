//! Supervisor authority over Intelligence settings and internal runs (spec 0012
//! §8): the `/v1/supervisor/admin` nest on the machine listener, its `settings`
//! scope, its three path prefixes, and the owner CSRF guard it leaves alone.
use super::Fixture;
use aq_internal_work::{Affected, AnswerShape, Purpose, Request as Work, store};
use aq_server::{InternalWork, agent_access::supervisor_conversation::reply};
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
    routing::any,
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tower::ServiceExt;
use uuid::Uuid;

const NEST: &str = "/v1/supervisor/admin";
const INTERNAL: &str = "/api/settings/internal-work";
const STRUCTURE: &str = "/api/settings/intelligence/structure";
const PROFILES: &str = "/api/settings/intelligence";
const FIVE: [&str; 5] = [
    "graph.read",
    "graph.write",
    "enqueue",
    "messaging",
    "decisions",
];

/// Who sends a request: a supervisor bearer, or the dev-mode owner browser
/// with or without the CSRF header.
enum As<'a> {
    Supervisor(&'a str),
    Owner { csrf: bool },
}

struct Admin {
    f: Fixture,
    token: String,
    machine: Router,
    owner: Router,
}

/// A global supervisor with a running session and a credential, over a state
/// whose internal work service is wired (retry needs it).
async fn admin() -> Admin {
    let mut f = Fixture::new(aq_server::STATE_FORMAT_VERSION).await;
    f.state.internal_work =
        InternalWork::new(f.pool.clone(), f.state.clock.clone(), None, None, None).unwrap();
    f.send().await;
    let r = f.reserve().await.reservation.unwrap();
    let p = f.plan(r.id, 1).await;
    f.start(&p).await;
    let token = reply::issue_credential(&f.pool, f.actor, 60_000, 999_000)
        .await
        .unwrap();
    let machine = aq_server::build_worker_router(f.state.clone());
    let owner = aq_server::build_admin_router(f.state.clone());
    Admin {
        f,
        token,
        machine,
        owner,
    }
}

async fn call(
    app: &Router,
    method: &str,
    path: &str,
    who: As<'_>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    request = match who {
        As::Supervisor(token) => request.header("authorization", format!("Bearer {token}")),
        As::Owner { csrf } => {
            let request = request.header("x-aq-dev-email", "owner@localhost");
            if csrf {
                request.header("x-aq-request", "1")
            } else {
                request
            }
        }
    };
    let body = body.map_or_else(Body::empty, |v| Body::from(v.to_string()));
    let response = app
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

impl Admin {
    async fn supervisor(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let path = format!("{NEST}{path}");
        call(
            &self.machine,
            method,
            &path,
            As::Supervisor(&self.token),
            body,
        )
        .await
    }
    async fn as_owner(&self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        call(&self.owner, method, path, As::Owner { csrf: true }, body).await
    }
    /// Activates the example catalog through the owner router, as the owner would.
    async fn activate(&self) -> aq_profiles::ValidatedCatalog {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../aq-profiles/examples/catalog");
        let catalog = aq_profiles::load(&dir).unwrap();
        let artifact = String::from_utf8(catalog.canonical_bytes().unwrap()).unwrap();
        let body =
            json!({"operation_id":Uuid::new_v4(),"expected_revision":0,"artifact_json":artifact});
        let (status, out) = self
            .as_owner("POST", "/api/profiles/catalogs/activate", Some(body))
            .await;
        assert_eq!(status, StatusCode::OK, "{out}");
        catalog
    }
    /// Replaces the live credential with one issued before `settings` existed:
    /// same generation and session, the original five scopes.
    async fn reissue_with_five_scopes(&mut self) {
        use sha2::Digest as _;
        let digest = |t: &str| hex::encode(sha2::Sha256::digest(t.as_bytes()));
        let old = format!("aqsv1_{}", digest("five scopes"));
        sqlx::query("UPDATE central_supervisor_credentials SET revoked_at_ms=999500 WHERE actor_id=$1 AND revoked_at_ms IS NULL")
            .bind(self.f.actor).execute(&self.f.pool).await.unwrap();
        sqlx::query("INSERT INTO central_supervisor_credentials(token_digest,actor_id,generation,session_id,scopes,issued_at_ms,expires_at_ms) SELECT $1,actor_id,generation,session_id,$2,999000,1059000 FROM central_supervisor_credentials WHERE token_digest=$3")
            .bind(digest(&old)).bind(FIVE.map(String::from).to_vec()).bind(digest(&self.token))
            .execute(&self.f.pool).await.unwrap();
        self.token = old;
    }
}

fn internal_save(snapshot: &Value, default: Value) -> Value {
    json!({"operation_id":Uuid::new_v4(),"expected_revision":snapshot["revision"],
        "expected_catalog_revision":snapshot["catalog_revision"],
        "default_profile_id":default,"overrides":{}})
}

fn failed_run() -> Work {
    Work {
        purpose: Purpose::DecisionRevision,
        instructions: "Revise the decision.".into(),
        input: "Decision: keep the queue".into(),
        answer: AnswerShape::Choice {
            choices: vec!["keep".into(), "drop".into()],
            rationale_max_bytes: 512,
        },
        blocking: false,
        affected: Affected {
            kind: "task".into(),
            id: "T-12".into(),
            project_id: None,
        },
        deadline_ms: 60_000,
        max_output_bytes: 4_096,
        escalate: true,
        profile_override: None,
    }
}

#[tokio::test]
async fn supervisor_with_settings_scope_can_save_internal_work_settings_and_the_routing_policy() {
    let a = admin().await;
    let issued: Vec<String> =
        sqlx::query_scalar("SELECT scopes FROM central_supervisor_credentials WHERE actor_id=$1")
            .bind(a.f.actor)
            .fetch_one(&a.f.pool)
            .await
            .unwrap();
    assert_eq!(
        issued,
        [
            "graph.read",
            "graph.write",
            "enqueue",
            "messaging",
            "decisions",
            "settings"
        ]
    );
    let catalog = a.activate().await;

    // The internal work default, saved without the CSRF header.
    let (status, snapshot) = a.supervisor("GET", INTERNAL, None).await;
    assert_eq!(status, StatusCode::OK, "{snapshot}");
    let body = internal_save(&snapshot, json!("engineering"));
    let (status, saved) = a.supervisor("POST", INTERNAL, Some(body.clone())).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["disposition"], "saved");
    let (_, read) = a.as_owner("GET", INTERNAL, None).await;
    assert_eq!(read["default_profile_id"], "engineering");
    let id = body["operation_id"].as_str().unwrap();
    let receipt = a
        .supervisor("GET", &format!("{INTERNAL}/operations/{id}"), None)
        .await;
    assert_eq!(receipt, (StatusCode::OK, saved));
    // The save is attributed to the owner who approved the supervisor.
    let actor: Uuid = sqlx::query_scalar(
        "SELECT actor_id FROM internal_work_settings_operations WHERE operation_id=$1",
    )
    .bind(
        body["operation_id"]
            .as_str()
            .unwrap()
            .parse::<Uuid>()
            .unwrap(),
    )
    .fetch_one(&a.f.pool)
    .await
    .unwrap();
    assert_eq!(actor, a.f.user.id);

    // The routing policy (the structure document's Markdown).
    let (status, structure) = a.supervisor("GET", STRUCTURE, None).await;
    assert_eq!(status, StatusCode::OK, "{structure}");
    let policy = json!({"operation_id":Uuid::new_v4(),"expected_revision":structure["revision"],
        "expected_catalog_revision":structure["catalog_revision"],
        "routing_markdown":"Route a one-line fix to simple.",
        "parents":structure["parents"],"assignments":structure["assignments"]});
    let (status, saved) = a.supervisor("POST", STRUCTURE, Some(policy)).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    let (_, read) = a.as_owner("GET", STRUCTURE, None).await;
    assert_eq!(read["routing_markdown"], "Route a one-line fix to simple.");

    // A profile.
    let (status, settings) = a.supervisor("GET", PROFILES, None).await;
    assert_eq!(status, StatusCode::OK, "{settings}");
    let profile = &catalog.artifact().catalog.intelligence[0];
    let edit = json!({"operation_id":Uuid::new_v4(),"expected_revision":settings["revision"],
        "profile_id":profile.id,"profile_version":profile.version,"name":"Supervisor engineering",
        "description":"Edited by a supervisor","candidates":profile.candidates});
    let (status, saved) = a.supervisor("POST", PROFILES, Some(edit)).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["profile"]["name"], "Supervisor engineering");

    // The owner paths keep their CSRF guard: a browser-style request without
    // the header is refused, and a supervisor bearer does not stand in for it.
    let (_, before) = a.as_owner("GET", INTERNAL, None).await;
    let body = internal_save(&before, Value::Null);
    let browser = call(
        &a.owner,
        "POST",
        INTERNAL,
        As::Owner { csrf: false },
        Some(body.clone()),
    )
    .await;
    assert_eq!(browser.0, StatusCode::FORBIDDEN, "{}", browser.1);
    let bearer = call(
        &a.owner,
        "POST",
        INTERNAL,
        As::Supervisor(&a.token),
        Some(body),
    )
    .await;
    assert_eq!(bearer.0, StatusCode::FORBIDDEN, "{}", bearer.1);
    let (_, after) = a.as_owner("GET", INTERNAL, None).await;
    assert_eq!(after, before);
    // The nest exists only on the machine listener.
    let path = format!("{NEST}{INTERNAL}");
    let owner_listener = call(&a.owner, "GET", &path, As::Supervisor(&a.token), None).await;
    assert_eq!(owner_listener.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn supervisor_without_settings_scope_gets_403_on_the_admin_nest() {
    let mut a = admin().await;
    let snapshot = a.supervisor("GET", INTERNAL, None).await.1;
    a.reissue_with_five_scopes().await;
    let (status, projects) = call(
        &a.machine,
        "GET",
        "/v1/supervisor/projects",
        As::Supervisor(&a.token),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the credential is otherwise live: {projects}"
    );
    let failed = store::submit(&a.f.pool, &failed_run(), &[], 1_000_000)
        .await
        .unwrap();
    for (method, path, body) in [
        ("GET", INTERNAL.to_string(), None),
        (
            "POST",
            INTERNAL.to_string(),
            Some(internal_save(&snapshot, Value::Null)),
        ),
        ("GET", STRUCTURE.to_string(), None),
        ("GET", "/api/internal-work/runs".to_string(), None),
        (
            "POST",
            format!("/api/internal-work/runs/{failed}/retry"),
            None,
        ),
    ] {
        let (status, out) = a.supervisor(method, &path, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}: {out}");
    }
    let tasks: i64 = sqlx::query_scalar("SELECT count(*) FROM internal_tasks")
        .fetch_one(&a.f.pool)
        .await
        .unwrap();
    assert_eq!(tasks, 1, "no retry was submitted");
    let (status, _) = call(
        &a.machine,
        "GET",
        &format!("{NEST}{INTERNAL}"),
        As::Owner { csrf: true },
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "no bearer");
}

#[tokio::test]
async fn admin_nest_refuses_paths_outside_settings_and_internal_work() {
    let a = admin().await;
    for (method, path) in [
        ("GET", "/api/worker-logins"),
        ("POST", "/api/profiles/catalogs/activate"),
        ("GET", "/api/profiles/catalogs"),
        ("GET", "/api/settings/swarm"),
        ("GET", "/api/settings/intelligence-extra"),
        ("GET", "/api/human/projects"),
    ] {
        let (status, out) = a.supervisor(method, path, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}: {out}");
    }
    // The owner handlers take no PUT; the router refuses it before the gate.
    let (status, _) = a.supervisor("PUT", INTERNAL, None).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);

    // The gate itself, over an inner router that serves more than it may
    // forward: an outside path is 404 and a method other than GET, HEAD and
    // POST is 403, both before the inner router is reached.
    let reached = Arc::new(AtomicUsize::new(0));
    let hit = |reached: Arc<AtomicUsize>| {
        any(move || async move {
            reached.fetch_add(1, Ordering::SeqCst);
            "reached"
        })
    };
    let inner = Router::new()
        .route("/api/worker-logins", hit(reached.clone()))
        .route("/api/settings/intelligence-extra", hit(reached.clone()))
        .route("/api/internal-work", hit(reached.clone()))
        .route("/api/internal-work/runs", hit(reached.clone()));
    let gate =
        aq_supervisor_control::admin::routes(inner, a.f.pool.clone(), Arc::new(|| 1_000_000));
    for path in [
        "/api/worker-logins",
        "/api/settings/intelligence-extra",
        "/api/internal-work",
    ] {
        let (status, _) = call(
            &gate,
            "GET",
            &format!("{NEST}{path}"),
            As::Supervisor(&a.token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }
    let path = format!("{NEST}/api/internal-work/runs");
    for method in ["PUT", "DELETE", "PATCH"] {
        let (status, _) = call(&gate, method, &path, As::Supervisor(&a.token), None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method}");
    }
    assert_eq!(reached.load(Ordering::SeqCst), 0);
    let (status, _) = call(&gate, "GET", &path, As::Supervisor(&a.token), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(reached.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn supervisor_can_list_and_retry_internal_runs() {
    let a = admin().await;
    let failed = store::submit(&a.f.pool, &failed_run(), &[], 1_000_000)
        .await
        .unwrap();
    let (status, runs) = a.supervisor("GET", "/api/internal-work/runs", None).await;
    assert_eq!(status, StatusCode::OK, "{runs}");
    assert_eq!(runs["runs"][0]["id"], failed.to_string());
    let (status, run) = a
        .supervisor("GET", &format!("/api/internal-work/runs/{failed}"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{run}");
    assert_eq!(run["status"], "failed");
    let (status, retried) = a
        .supervisor(
            "POST",
            &format!("/api/internal-work/runs/{failed}/retry"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{retried}");
    let new: Uuid = retried["id"].as_str().unwrap().parse().unwrap();
    let row = store::get(&a.f.pool, new).await.unwrap().unwrap();
    assert_eq!(row.retry_of, Some(failed));
    assert_eq!(row.request.affected.id, "T-12");
    // A supervisor's retry counts against its attempts; the owner's does not.
    let original = store::get(&a.f.pool, failed).await.unwrap().unwrap();
    assert_eq!(
        (original.supervisor_attempts, row.supervisor_attempts),
        (1, 1)
    );
    let (status, _) = a
        .as_owner(
            "POST",
            &format!("/api/internal-work/runs/{new}/retry"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let row = store::get(&a.f.pool, new).await.unwrap().unwrap();
    assert_eq!(row.supervisor_attempts, 1);

    let diagnosis = "profile internal has no working model";
    let (status, escalated) = a
        .supervisor(
            "POST",
            &format!("/api/internal-work/runs/{new}/escalate"),
            Some(json!({"diagnosis": diagnosis})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{escalated}");
    assert_eq!(
        (&escalated["escalation"], &escalated["diagnosis"]),
        (&json!("owner"), &json!(diagnosis))
    );
    let (status, resolved) = a
        .supervisor(
            "POST",
            &format!("/api/internal-work/runs/{new}/resolve"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{resolved}");
    assert_eq!(resolved["escalation"], "resolved");
}

#[tokio::test]
async fn supervisor_reads_and_switches_routing_through_the_admin_nest() {
    const ROUTING: &str = "/api/settings/intelligence/routing";
    let a = admin().await;
    let (status, body) = a.supervisor("GET", ROUTING, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({"enabled": false, "active": null, "latest": null})
    );
    // Routing turns on only once a routing playbook is active, for a supervisor as for the owner.
    let on = Some(json!({"enabled": true}));
    let (status, refused) = a.supervisor("POST", ROUTING, on).await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(
        refused["message"],
        "Routing can be turned on once a routing playbook is active"
    );
    let off = Some(json!({"enabled": false}));
    let (status, body) = a.supervisor("POST", ROUTING, off).await;
    assert_eq!(
        (status, body["enabled"].clone()),
        (StatusCode::OK, json!(false))
    );
    assert_eq!(a.as_owner("GET", ROUTING, None).await.1, body);
}
