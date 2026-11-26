use super::*;

pub(super) fn observation() -> Value {
    let c = credential();
    json!({"operation_id":Uuid::new_v4(),"expected_revision":1,"sequence":1,"scope":{"credential_revision":1,"provider":c.provider,"credential_profile":c.profile,"account_reference":c.account_reference,"endpoint_reference":c.endpoint_reference,"model":"model-a","node_id":null},"state":"quota_unavailable","ttl_ms":1000,"cooldown_ms":1000})
}

async fn submit(app: &Router, body: Value) -> (StatusCode, Value) {
    call(
        app,
        "/api/intelligence/observations",
        Some(body),
        Session::Owner,
    )
    .await
}
pub(super) async fn verify_receipt(app: &Router, body: &Value) -> Value {
    let (status, first) = submit(app, body.clone()).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["source"]["kind"], "operator");
    assert_eq!(first["revision"], 2);

    assert_eq!(submit(app, body.clone()).await.1, first);
    first
}
pub(super) async fn verify_refusals(app: &Router, body: &Value) {
    let mut changed = body.clone();
    changed["state"] = "available".into();

    assert_eq!(submit(app, changed).await.0, StatusCode::CONFLICT);
    let mut stale = body.clone();
    stale["operation_id"] = json!(Uuid::new_v4());
    stale["expected_revision"] = 2.into();

    assert_eq!(submit(app, stale.clone()).await.0, StatusCode::CONFLICT);
    stale["sequence"] = 2.into();
    stale["scope"]["account_reference"] = "foreign-account".into();

    assert_eq!(submit(app, stale).await.0, StatusCode::BAD_REQUEST);
    let mut unknown = body.clone();
    unknown["principal_id"] = json!(Uuid::new_v4());

    assert_eq!(
        submit(app, unknown).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
}
pub(super) async fn verify_removed_scope(state: &AppState, body: Value, first: Value) {
    let mut empty = registry(None);
    empty.revision = 2;
    empty.credentials.clear();

    let app = aq_server::build_admin_router(configured(state, &empty));
    assert_eq!(register(&app, Session::Owner).await, StatusCode::OK);
    assert_eq!(submit(&app, body).await.1, first);
    let (_, view) = call(&app, "/api/intelligence/registry", None, Session::Owner).await;
    assert_eq!(view["accounting"], "unavailable");
    assert_eq!(view["credentials"], json!([]));
    assert!(!view.to_string().contains("private_fingerprints"));
}
pub(super) async fn verify_rotation(state: &AppState) {
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("rotation");
    std::fs::write(&key, b"synthetic-rotated-key").unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut rotated = registry(Some(key));
    rotated.revision = 3;

    let app = aq_server::build_admin_router(configured(state, &rotated));
    assert_eq!(register(&app, Session::Owner).await, StatusCode::CONFLICT);
    rotated.credentials[0].credential.revision = 2;
    let app = aq_server::build_admin_router(configured(state, &rotated));
    assert_eq!(register(&app, Session::Owner).await, StatusCode::OK);
}
