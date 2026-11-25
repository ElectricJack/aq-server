use super::*;

#[derive(Clone, Copy)]
pub(super) enum Session {
    Owner,
    Viewer,
    OwnerWithoutCsrf,
}

pub(super) async fn call(
    app: &Router,
    path: &str,
    body: Option<Value>,
    session: Session,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(if body.is_some() { "POST" } else { "GET" })
        .uri(path)
        .header("content-type", "application/json")
        .header(
            "x-aq-dev-email",
            match session {
                Session::Viewer => "viewer@localhost",
                _ => "owner@localhost",
            },
        );
    if !matches!(session, Session::OwnerWithoutCsrf) {
        req = req.header("X-AQ-Request", "1");
    }
    let response = app
        .clone()
        .oneshot(
            req.body(Body::from(body.map(|v| v.to_string()).unwrap_or_default()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

pub(super) async fn register(app: &Router, session: Session) -> StatusCode {
    call(app, "/api/intelligence/register", Some(json!({})), session)
        .await
        .0
}

pub(super) fn configured(state: &AppState, registry: &IntelligenceConfig) -> AppState {
    let mut config = (*state.config).clone();
    config.intelligence = Some(Arc::new(registry.load().unwrap()));
    AppState {
        config: Arc::new(config),
        ..state.clone()
    }
}
pub(super) async fn machine(
    app: &Router,
    path: &str,
    credential: &str,
    body: Value,
) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {credential}"))
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}
