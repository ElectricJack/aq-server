use super::*;
use axum::{
    body::Body,
    http::Request,
    middleware::{Next, from_fn},
    response::IntoResponse,
};
use http_body_util::BodyExt;
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct Payload {
    value: String,
}
pub(super) struct TestAction(pub(super) Arc<AtomicUsize>);
#[async_trait]
impl ActionKind for TestAction {
    type Payload = Payload;
    const KIND: &'static str = "test.effect";
    const GRANT: &'static str = "test.effect";
    const TARGET: aq_command_registry::TargetRule = aq_command_registry::TargetRule::Subject;
    async fn authorize(
        &self,
        _: &AppState,
        user: &User,
        _: &ActionRequest,
        _: &Payload,
    ) -> Result<Scope, ApiError> {
        require_owner_role(user)?;
        Ok(Scope::AllAuthorized)
    }
    async fn authorize_lookup(
        &self,
        state: &AppState,
        user: &User,
        _: &Scope,
        _: Option<&Operation>,
    ) -> Result<(), ApiError> {
        require_owner_role(user)?;
        existing::owner(state, user).await
    }
    async fn execute(&self, _: &AppState, _: &User, _: &ActionRequest, p: Payload) -> Outcome {
        self.0.fetch_add(1, Ordering::SeqCst);
        if p.value == "timeout" {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
        if p.value == "lost" {
            Outcome::unknown()
        } else if p.value == "conflict" {
            Outcome::conflict(std::collections::BTreeMap::from([(
                "test".into(),
                "3".into(),
            )]))
        } else if p.value == "confirmed_with_versions" {
            let mut o = Outcome::confirmed(vec![], vec![]);
            o.current_versions = std::collections::BTreeMap::from([("test".into(), "3".into())]);
            o
        } else {
            Outcome::confirmed(vec![], vec![])
        }
    }
}
fn require_owner_role(user: &User) -> Result<(), ApiError> {
    if user.role != Role::Owner {
        return Err(ApiError::for_status(StatusCode::FORBIDDEN));
    }
    Ok(())
}

pub(super) async fn setup() -> (TestDb, AppState, User, Arc<Registry>, Arc<AtomicUsize>) {
    let db = TestDb::create().await;
    let store = db.store().await;
    let now = chrono::Utc::now();
    let user = store
        .bootstrap_owner("owner@localhost", "dev", "owner@localhost", now)
        .await
        .unwrap()
        .unwrap();
    let config: Config = toml::from_str(
        r#"format_version=1
environment="development"
[server]
admin_bind="127.0.0.1:0"
worker_bind="127.0.0.1:0"
public_origin="http://localhost"
[auth]
mode="dev"
dev_email="owner@localhost"
[bootstrap]
enabled=false
"#,
    )
    .unwrap();
    let identity = verifier_from_config(&config.auth, reqwest::Client::new());
    let state = AppState {
        internal_work: Default::default(),
        config: Arc::new(config),
        store,
        identity,
        clock: Arc::new(chrono::Utc::now),
        agent_access: None,
        source_retention: None,
    };
    let count = Arc::new(AtomicUsize::new(0));
    let mut registry = Registry::default();
    registry.register(TestAction(count.clone())).unwrap();
    (db, state, user, Arc::new(registry), count)
}
pub(super) async fn submit(
    state: &AppState,
    registry: &Registry,
    user: &User,
    request: ActionRequest,
) -> Result<Operation, ApiError> {
    registry
        .for_user(state, user)
        .submit_scoped(request, None)
        .await
}
pub(super) fn request(value: &str) -> ActionRequest {
    serde_json::from_value(json!({"operation_id":Uuid::new_v4(),"kind":"test.effect",
        "subject":{"type":"test","id":"subject"},"expected_versions":{},"payload":{"value":value}}))
    .unwrap()
}
pub(super) fn post_request(request: &ActionRequest) -> Request<Body> {
    Request::post("/api/v1/actions")
        .header("x-aq-dev-email", "owner@localhost")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(request).unwrap()))
        .unwrap()
}
pub(super) fn get_request(id: Uuid) -> Request<Body> {
    Request::get(format!("/api/v1/operations/{id}"))
        .header("x-aq-dev-email", "owner@localhost")
        .body(Body::empty())
        .unwrap()
}
pub(super) async fn response_json(response: axum::response::Response) -> serde_json::Value {
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}
pub(super) struct Deadline {
    pub started: Arc<AtomicUsize>,
    pub revoked: bool,
}
impl Deadline {
    pub(super) fn router(self, state: &AppState, user: &User, registry: Arc<Registry>) -> Router {
        let Self { started, revoked } = self;
        let pool = state.store.pool.clone();
        let actor = user.id;
        let gate = from_fn(move |req: Request<Body>, next: Next| {
            let started = started.clone();
            let pool = pool.clone();
            async move {
                let dispatch = next.run(req);
                tokio::pin!(dispatch);
                tokio::select! {
                    response=&mut dispatch => response,
                    _=wait_for_dispatch(&started) => {
                        if revoked { sqlx::query("UPDATE users SET role='viewer' WHERE id=$1").bind(actor).execute(&pool).await.unwrap(); }
                        StatusCode::SERVICE_UNAVAILABLE.into_response()
                    },
                }
            }
        });
        routes(registry)
            .layer(gate)
            .layer(from_fn(crate::dashboard_api::errors::normalize_errors))
            .with_state(state.clone())
    }
}
async fn wait_for_dispatch(started: &AtomicUsize) {
    while started.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
}
