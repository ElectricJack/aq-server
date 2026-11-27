//! aq-role-settings (spec 0014 §7, §7.1): the stored role and profile
//! documents, whole-set validation, the active snapshot, rolling snapshots
//! and the owner routes.
use aq_profiles::roles::{self, Kind, SelectionDefaults};
use aq_role_settings::{self as settings, Error, Receipt, StoreLimits};
use aq_server::{AppState, config::Config, identity::verifier_from_config, test_support::TestDb};
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use chrono::{TimeZone, Utc};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;

const NOW: i64 = 1_000_000;

struct Fixture {
    _db: TestDb,
    pool: PgPool,
    owner: Uuid,
    app: Router,
}
/// A migrated database with an owner, a viewer and the shipped defaults
/// installed and active, as after server start.
async fn started() -> Fixture {
    started_with("").await
}
/// [`started`], with `root` (top-level TOML keys) in the server's config.
async fn started_with(root: &str) -> Fixture {
    let db = TestDb::create().await;
    let store = db.store().await;
    let time = Utc.timestamp_millis_opt(NOW).unwrap();
    store
        .bootstrap_owner("owner@localhost", "dev", "owner@localhost", time)
        .await
        .unwrap();
    store
        .invite_user("viewer@localhost", aq_server::store::Role::Viewer, time)
        .await
        .unwrap();
    let config: Config = toml::from_str(&format!(
        r#"{root}
format_version=1
environment="development"
[server]
admin_bind="127.0.0.1:0"
worker_bind="127.0.0.1:0"
public_origin="http://127.0.0.1:8080"
[auth]
mode="dev"
dev_email="owner@localhost"
[bootstrap]
owner_email="owner@localhost"
enabled=true
"#
    ))
    .unwrap();
    let pool = db.pool().await;
    let owner = sqlx::query_scalar("SELECT id FROM users WHERE role='owner'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let state = AppState {
        internal_work: Default::default(),
        identity: verifier_from_config(&config.auth, reqwest::Client::new()),
        config: Arc::new(config),
        store,
        clock: Arc::new(|| Utc.timestamp_millis_opt(NOW).unwrap()),
        agent_access: None,
        source_retention: None,
    };
    // `TestDb::store` has installed the shipped defaults, as server start does.
    let start = settings::startup(&pool, NOW).await.unwrap();
    assert!(start.installed.is_empty());
    assert!(start.problems.is_empty(), "{:?}", start.problems);
    Fixture {
        _db: db,
        pool,
        owner,
        app: aq_server::build_admin_router(state),
    }
}
fn shipped(kind: Kind, id: &str) -> String {
    let label = format!("/{id}.md");
    roles::default_documents()
        .into_iter()
        .find(|d| d.kind == kind && d.label.ends_with(&label))
        .unwrap()
        .text
}
/// The developer role with `text` appended to its rules: a valid edit.
fn developer(text: &str) -> String {
    format!("{}- {text}\n", shipped(Kind::Role, "developer"))
}
impl Fixture {
    async fn revision_of(&self, kind: Kind, id: &str) -> i64 {
        settings::get(&self.pool, kind, id)
            .await
            .unwrap()
            .map_or(0, |d| d.revision)
    }
    /// Save at the document's current revision with a fresh operation id.
    async fn save(&self, kind: Kind, id: &str, markdown: &str) -> Result<Receipt, Error> {
        let expected = self.revision_of(kind, id).await;
        settings::save(
            settings::Writer::new(&self.pool, self.owner, NOW).edit(expected, Uuid::new_v4()),
            kind,
            id,
            markdown,
        )
        .await
    }
    async fn limits(&self, snapshots_kept: u32, operations_kept: u32) {
        let current = settings::settings(&self.pool).await.unwrap();
        let limits = StoreLimits {
            snapshots_kept,
            operations_kept,
            ..current.limits
        };
        settings::save_limits(
            settings::Writer::new(&self.pool, self.owner, NOW)
                .edit(current.limits_revision, Uuid::new_v4()),
            &limits,
        )
        .await
        .unwrap();
    }
    async fn active(&self) -> (i64, String) {
        let active = settings::active_snapshot(&self.pool)
            .await
            .unwrap()
            .unwrap();
        (active.revision, active.snapshot.digest().to_string())
    }
    async fn stored_digests(&self) -> Vec<String> {
        sqlx::query_scalar("SELECT digest FROM role_snapshots ORDER BY activated_revision")
            .fetch_all(&self.pool)
            .await
            .unwrap()
    }
    fn owner(&self) -> Client<'_> {
        Client {
            app: &self.app,
            email: "owner@localhost",
            csrf: false,
        }
    }
    fn owner_write(&self) -> Client<'_> {
        Client {
            app: &self.app,
            email: "owner@localhost",
            csrf: true,
        }
    }
    fn viewer_read(&self) -> Client<'_> {
        Client {
            app: &self.app,
            email: "viewer@localhost",
            csrf: false,
        }
    }
    fn viewer(&self) -> Client<'_> {
        Client {
            app: &self.app,
            email: "viewer@localhost",
            csrf: true,
        }
    }
}
struct Client<'a> {
    app: &'a Router,
    email: &'static str,
    csrf: bool,
}
impl Client<'_> {
    async fn get(self, path: &str) -> (StatusCode, Value) {
        self.send("GET", path, None).await
    }
    async fn put(self, path: &str, body: Value) -> (StatusCode, Value) {
        self.send("PUT", path, Some(body)).await
    }
    async fn delete(self, path: &str) -> (StatusCode, Value) {
        self.send("DELETE", path, None).await
    }
    async fn send(self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .header("x-aq-dev-email", self.email);
        if self.csrf {
            request = request.header("X-AQ-Request", "1");
        }
        let body = Body::from(body.map(|v| v.to_string()).unwrap_or_default());
        let response = self
            .app
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
}
fn problems(error: Error) -> Vec<String> {
    match error {
        Error::Problems(problems) => problems.iter().map(ToString::to_string).collect(),
        other => panic!("expected problems, got {other:?}"),
    }
}

mod documents;
mod owner_routes;
mod retention;
mod settings_routes;
