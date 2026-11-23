//! Playbook sources and authoring runs (spec 0013 §4.1, §4.5): the built-in
//! and registered source records, a run recorded per request, skipped when
//! nothing the agent would see changed, superseded by a newer request, and the
//! source and purpose found from an artifact digest. Resuming after a restart
//! submits and records each run's internal task exactly once.
use aq_internal_work::{Affected, AnswerShape, Purpose, Request, store as internal};
use aq_playbook_authoring::{
    self as authoring, Ended, Error, Finished, GRACE_MS, SourceKind, SourceRecord, active,
    ensure_builtin, finish, in_flight, register, requested, run_for_artifact, supersede_older,
    task_for,
};
use aq_server::test_support::TestDb;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::Duration,
};
use uuid::Uuid;

/// A source's Markdown, its revision and its facts.
type Now = (String, u64, BTreeMap<String, Value>);

/// What the host says each source's Markdown, revision and facts are now.
#[derive(Clone)]
struct Host {
    pool: PgPool,
    now: Arc<Mutex<BTreeMap<String, Now>>>,
    /// Read the pool too, as the server's host does (Settings, options).
    reads_pool: bool,
    /// What the fake engine was asked to do.
    engine: Arc<Mutex<Engine>>,
    /// A registered source's document is the stored one, as on the server.
    stored: bool,
}

/// A fake engine and grant store: it records installs, grants and activations.
#[derive(Default)]
struct Engine {
    installed: BTreeSet<String>,
    grants: BTreeMap<String, (Uuid, BTreeSet<String>)>,
    granted: usize,
    revoked: Vec<String>,
    /// `(binding, artifact digest)`, in order.
    activations: Vec<(String, String)>,
    down: bool,
    /// The engine refuses every activation (a passing fault).
    refuses_activation: bool,
    /// While an activation is under way, someone tries to delete this run;
    /// whether the delete had to wait is recorded.
    delete_during_activation: Option<i64>,
    delete_waited: Option<bool>,
}

impl authoring::Deployer for Host {
    async fn install(&self, artifact: &authoring::model::Artifact) -> authoring::Result<()> {
        let mut engine = self.engine.lock().unwrap();
        engine.installed.insert(artifact.digest.clone());
        Ok(())
    }
    async fn ensure_grant(
        &self,
        source: &str,
        capabilities: &BTreeSet<String>,
    ) -> authoring::Result<(Uuid, i64)> {
        let mut engine = self.engine.lock().unwrap();
        if engine.down {
            return Err(Error::Conflict("Playbooks are not running".into()));
        }
        if let Some((id, held)) = engine.grants.get(source)
            && held == capabilities
        {
            return Ok((*id, 1));
        }
        let id = Uuid::new_v4();
        engine.granted += 1;
        (engine.grants).insert(source.into(), (id, capabilities.clone()));
        Ok((id, 1))
    }
    async fn revoke_grant(&self, source: &str) -> authoring::Result<()> {
        let mut engine = self.engine.lock().unwrap();
        engine.grants.remove(source);
        engine.revoked.push(source.into());
        Ok(())
    }
    async fn activate(&self, _: &str, binding: &str, digest: &str) -> authoring::Result<()> {
        let doomed = self.engine.lock().unwrap().delete_during_activation.take();
        if let Some(run) = doomed {
            let delete =
                sqlx::query("DELETE FROM playbook_authoring_runs WHERE id=$1 AND NOT active")
                    .bind(run)
                    .execute(&self.pool);
            let waited = tokio::time::timeout(Duration::from_millis(400), delete).await;
            self.engine.lock().unwrap().delete_waited = Some(waited.is_err());
        }
        let mut engine = self.engine.lock().unwrap();
        if engine.refuses_activation {
            return Err(Error::Conflict("the engine is busy".into()));
        }
        assert!(
            engine.installed.contains(digest),
            "activated before install"
        );
        engine.activations.push((binding.into(), digest.into()));
        Ok(())
    }
}

fn login_candidate() -> aq_internal_work::Candidate {
    aq_internal_work::Candidate {
        provider: "anthropic".into(),
        model: "claude-fixture".into(),
        reasoning: None,
        harness: Some("claude".into()),
        way: aq_internal_work::Way::Login,
        credential_index: 0,
    }
}

fn clock_ms() -> i64 {
    let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
    since.unwrap().as_millis() as i64
}

impl authoring::Server for Host {
    fn config(&self) -> authoring::Config {
        authoring::Config {
            keep_finished_runs: 3,
            facts_restarts: 1,
            ..authoring::Config::default()
        }
    }
    fn now_ms(&self) -> i64 {
        clock_ms()
    }
    async fn submit(&self, id: Uuid, request: &Request) -> authoring::Result<()> {
        (request.validate()).map_err(|e| Error::Invalid(e.to_string()))?;
        let candidates = [login_candidate()];
        internal::Queue::at(&self.pool, clock_ms())
            .submit_as(id, request, Ok(&candidates))
            .await
            .map_err(authoring::complete::internal_error)?;
        Ok(())
    }
}

impl AsRef<PgPool> for Host {
    fn as_ref(&self) -> &PgPool {
        &self.pool
    }
}

impl authoring::Host for Host {
    async fn markdown(&self, source: &SourceRecord) -> authoring::Result<(String, u64)> {
        if self.reads_pool {
            sqlx::query("SELECT pg_sleep(0.02)")
                .execute(&self.pool)
                .await?;
        }
        if self.stored {
            let stored = authoring::stored_markdown(&self.pool, &source.id).await?;
            return stored.ok_or_else(|| Error::Host("no document".into()));
        }
        let now = self.now.lock().unwrap();
        let (text, revision, _) = now.get(&source.id).cloned().unwrap_or_default();
        Ok((text, revision))
    }

    async fn facts(&self, source: &SourceRecord) -> authoring::Result<BTreeMap<String, Value>> {
        if self.reads_pool {
            sqlx::query("SELECT 1").execute(&self.pool).await?;
        }
        let now = self.now.lock().unwrap();
        Ok(now.get(&source.id).map(|s| s.2.clone()).unwrap_or_default())
    }
}

impl Host {
    fn document<'a>(&'a self, source: &'a str) -> Document<'a> {
        Document { host: self, source }
    }
}

struct Document<'a> {
    host: &'a Host,
    source: &'a str,
}

impl Document<'_> {
    fn set(self, text: &str, revision: u64, facts: Value) {
        let facts = facts.as_object().unwrap().clone().into_iter().collect();
        let mut now = self.host.now.lock().unwrap();
        now.insert(self.source.into(), (text.into(), revision, facts));
    }
}

async fn setup() -> (TestDb, Host) {
    let db = TestDb::create().await;
    let _store = db.store().await;
    let pool = db.pool().await;
    let host = Host {
        pool,
        now: Arc::default(),
        reads_pool: false,
        engine: Arc::default(),
        stored: false,
    };
    (db, host)
}

fn names(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

fn routing() -> SourceRecord {
    SourceRecord {
        id: "routing".into(),
        kind: SourceKind::Builtin,
        binding: "aq.routing".into(),
        scope: "installation".into(),
        purpose: "Route every task to a level, a provider and a harness".into(),
        judgment_purpose: Purpose::RoutingClassification,
        commands: names(&["work.read", "work.route", "judgment.assess"]),
        events: names(&["aq.work.route_needed", "aq.judgment.assessed"]),
        capabilities: names(&["work.read", "work.route", "judgment.assess"]),
        facts: vec!["routing_options".into(), "structure".into()],
        revision: 0,
    }
}

fn triage(binding: &str) -> SourceRecord {
    SourceRecord {
        id: "triage".into(),
        kind: SourceKind::Registered,
        binding: binding.into(),
        scope: "installation".into(),
        purpose: "Label new tasks".into(),
        judgment_purpose: Purpose::PlaybookCall,
        commands: names(&["work.read", "judgment.assess"]),
        events: names(&["aq.judgment.assessed"]),
        capabilities: names(&["work.read", "judgment.assess"]),
        facts: vec![],
        revision: 0,
    }
}

const POLICY: &str = "Research is Standard, high effort, unless it is clearly harder.";

fn options() -> Value {
    json!({"routing_options": {"classes": {"standard-high": [["anthropic", "claude"]]}}})
}

fn save(digest: &str, activate: bool) -> Finished {
    Finished {
        status: Ended::Saved,
        activate,
        artifact_digest: Some(digest.into()),
        artifact: Some(json!({"digest": digest})),
        ..Finished::default()
    }
}

/// Records run `id` as saved with artifact `digest`, and active when `active`.
async fn saved(pool: &PgPool, id: i64, digest: &str, active: bool) {
    assert!(finish(pool, id, &save(digest, active)).await.unwrap());
}

async fn status(pool: &PgPool, id: i64) -> (String, Option<i64>) {
    sqlx::query_as("SELECT status,finished_at_ms FROM playbook_authoring_runs WHERE id=$1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn runs(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM playbook_authoring_runs")
        .fetch_one(pool)
        .await
        .unwrap()
}

fn authoring_request(run: i64) -> Request {
    Request {
        purpose: Purpose::RoutingClassification,
        instructions: "Author the routing playbook.".into(),
        input: "source.md".into(),
        answer: AnswerShape::Json { schema: json!({}) },
        blocking: false,
        affected: Affected {
            kind: "playbook_source".into(),
            id: run.to_string(),
            project_id: None,
        },
        deadline_ms: 60_000,
        max_output_bytes: 4_096,
        escalate: false,
        profile_override: None,
        agent: None,
    }
}

async fn tasks(pool: &PgPool, id: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM internal_tasks WHERE id=$1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

// ---- Workspace, save, take effect, rollback (plan tasks C3, C4) ----

/// A file of the engine's worked example `notify-when-ready`.
fn example(path: &str) -> &'static str {
    let path = format!("notify-when-ready/{path}");
    let files = authoring::compiler::docs::example_files();
    files.into_iter().find(|(p, _)| *p == path).unwrap().1
}

fn example_cases() -> Vec<Value> {
    (authoring::compiler::docs::example_files().into_iter())
        .filter(|(path, _)| path.starts_with("notify-when-ready/cases/"))
        .map(|(_, text)| serde_json::from_str(text).unwrap())
        .collect()
}

const NOTIFY: &str = "notify-when-ready";
const BINDING: &str = "aq.notify-when-ready";

fn notify() -> SourceRecord {
    SourceRecord {
        id: NOTIFY.into(),
        kind: SourceKind::Registered,
        binding: BINDING.into(),
        scope: "project".into(),
        purpose: "Ask for a description when a task has none".into(),
        judgment_purpose: Purpose::PlaybookCall,
        commands: names(&["message.send", "work.read"]),
        events: names(&["aq.work.changed"]),
        capabilities: names(&["message.send", "work.read"]),
        facts: vec![],
        revision: 0,
    }
}

fn notify_facts() -> Value {
    serde_json::from_str(example("facts.json")).unwrap()
}

/// Registers the worked example as a source whose document is `markdown`.
async fn notify_source(host: &Host, markdown: &str, revision: u64) {
    register(&host.pool, &notify(), markdown).await.unwrap();
    host.document(NOTIFY)
        .set(markdown, revision, notify_facts());
}

/// The example's document with a closing line: the same playbook, reworded.
fn reworded(n: u64) -> String {
    format!("{}\nNote {n}.\n", example("source.md"))
}

/// What a fake agent leaves in `out/outcome.json` after `aq-playbooks save`.
fn saved_outcome() -> Value {
    json!({"kind": "saved", "draft": serde_json::from_str::<Value>(example("draft.json")).unwrap(),
        "cases": example_cases(), "artifact": format!("sha256:{}", "0".repeat(64)),
        "facts_digest": format!("sha256:{}", "0".repeat(64))})
}

fn answered(answer: Value) -> aq_internal_work::Outcome {
    aq_internal_work::Outcome::Answered {
        answer,
        provider: "anthropic".into(),
        model: "claude-fixture".into(),
        way: aq_internal_work::Way::Login,
        elapsed_ms: 1,
        usage: None,
        attempts: vec![],
    }
}

/// Authors the source and returns its run and internal task.
async fn authored(host: &Host) -> (i64, Uuid) {
    let run = authoring::author(host, NOTIFY)
        .await
        .unwrap()
        .expect("a run");
    let task: Uuid =
        sqlx::query_scalar("SELECT internal_task_id FROM playbook_authoring_runs WHERE id=$1")
            .bind(run)
            .fetch_one(&host.pool)
            .await
            .unwrap();
    (run, task)
}

async fn source_view(host: &Host) -> Value {
    let record = authoring::get(&host.pool, NOTIFY).await.unwrap().unwrap();
    authoring::view(&host.pool, &record, clock_ms())
        .await
        .unwrap()
}

/// The owner never reads an identifier in text meant for them.
fn owner_text(text: &str) {
    assert!(!text.contains("sha256:"), "{text}");
}

// ---- The sources API (plan task C6) ----

/// The owner, as the server's extractor would give it.
struct Owner;
impl<S: Send + Sync> axum::extract::FromRequestParts<S> for Owner {
    type Rejection = std::convert::Infallible;
    async fn from_request_parts(
        _: &mut axum::http::request::Parts,
        _: &S,
    ) -> Result<Self, Self::Rejection> {
        Ok(Owner)
    }
}
impl From<Owner> for Uuid {
    fn from(_: Owner) -> Uuid {
        Uuid::nil()
    }
}
struct Refused(Error);
impl From<Error> for Refused {
    fn from(e: Error) -> Self {
        Refused(e)
    }
}
impl axum::response::IntoResponse for Refused {
    fn into_response(self) -> axum::response::Response {
        use axum::http::StatusCode;
        let status = match &self.0 {
            Error::Invalid(_) => StatusCode::BAD_REQUEST,
            Error::NotFound(_) => StatusCode::NOT_FOUND,
            Error::Conflict(_) | Error::Host(_) => StatusCode::CONFLICT,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, axum::Json(json!({"message": self.0.to_string()}))).into_response()
    }
}

struct Api(axum::Router);
impl Api {
    fn new(host: &Host) -> Self {
        let routes = authoring::http::routes::<Host, Owner, Refused>();
        Api(routes.with_state(host.clone()))
    }
    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        use http_body_util::BodyExt;
        use tower::ServiceExt;
        let request = axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json");
        let body = body.map_or_else(axum::body::Body::empty, |v| v.to_string().into());
        let response = self
            .0
            .clone()
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status().as_u16();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }
}

const SOURCES: &str = "/api/playbooks/sources";

/// A host whose registered documents are the stored ones, with the facts
/// the worked example reads.
async fn api() -> (TestDb, Host, Api) {
    let (db, mut host) = setup().await;
    host.stored = true;
    host.document(NOTIFY).set("", 0, notify_facts());
    let api = Api::new(&host);
    (db, host, api)
}

fn registration(markdown: &str) -> Value {
    json!({"id": NOTIFY, "binding": BINDING,
        "purpose": "Ask for a description when a task has none", "markdown": markdown,
        "commands": ["message.send", "work.read"], "events": ["aq.work.changed"]})
}

async fn latest_task(host: &Host) -> Uuid {
    in_flight(&host.pool).await.unwrap()[0]
        .internal_task_id
        .unwrap()
}

// ---- Fix round (Codex review of C3 to C6) ----

// ---- Fix round 2 (Codex review of the routing switch-on) ----

/// The routing source's id and binding with the worked example's commands,
/// so the example's draft is its playbook.
async fn routing_like_source(host: &Host) -> (i64, Uuid) {
    let record = SourceRecord {
        id: "routing".into(),
        kind: SourceKind::Builtin,
        binding: "aq.routing".into(),
        ..notify()
    };
    ensure_builtin(&host.pool, &record).await.unwrap();
    host.document("routing")
        .set(example("source.md"), 1, notify_facts());
    let run = authoring::author(host, "routing").await.unwrap().unwrap();
    let task = in_flight(&host.pool).await.unwrap()[0].internal_task_id;
    (run, task.unwrap())
}

mod activation;
mod owner_api;
mod reconciliation;
mod recovery;
mod registration;
mod requests;
mod results;
mod supersession;
