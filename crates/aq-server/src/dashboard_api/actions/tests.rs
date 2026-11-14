use super::*;
use crate::{config::Config, identity::verifier_from_config, store::Role, test_support::TestDb};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Payload {
    value: String,
}
struct TestAction(Arc<AtomicUsize>);
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
        if user.role != Role::Owner {
            return Err(ApiError::for_status(StatusCode::FORBIDDEN));
        }
        Ok(Scope::AllAuthorized)
    }
    async fn authorize_lookup(
        &self,
        state: &AppState,
        user: &User,
        _: &Scope,
        _: Option<&Operation>,
    ) -> Result<(), ApiError> {
        if user.role != Role::Owner {
            return Err(ApiError::for_status(StatusCode::FORBIDDEN));
        }
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
async fn setup() -> (TestDb, AppState, User, Arc<Registry>, Arc<AtomicUsize>) {
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
fn request(value: &str) -> ActionRequest {
    serde_json::from_value(json!({"operation_id":Uuid::new_v4(),"kind":"test.effect",
        "subject":{"type":"test","id":"subject"},"expected_versions":{},"payload":{"value":value}}))
    .unwrap()
}
#[tokio::test]
async fn conflict_outcome_carries_current_versions() {
    let (_db, state, user, registry, _) = setup().await;
    let request = request("conflict");
    let op = submit(&state, &registry, &user, request.clone())
        .await
        .unwrap();
    assert_eq!(op.disposition, Disposition::Conflict);
    assert_eq!(op.recovery, Recovery::RefreshAndReview);
    assert_eq!(
        op.current_versions,
        std::collections::BTreeMap::from([("test".into(), "3".into())])
    );
    let lookup = journal::lookup(&state.store.pool, user.id, request.operation_id)
        .await
        .unwrap();
    assert_eq!(lookup, op);
    assert_eq!(json!(lookup)["current_versions"], json!({"test":"3"}));
    // Current versions are only conflict evidence; anything else is unproven.
    let bad = submit(
        &state,
        &registry,
        &user,
        self::request("confirmed_with_versions"),
    )
    .await
    .unwrap();
    assert_eq!(bad.disposition, Disposition::Unknown);
    assert!(bad.current_versions.is_empty());
    assert!(json!(bad).get("current_versions").is_none());
}
#[tokio::test]
async fn unknown_effect_operation_is_recoverable_by_lookup() {
    let (_db, state, user, registry, count) = setup().await;
    let request = request("lost");
    let original = submit(&state, &registry, &user, request.clone())
        .await
        .unwrap();
    assert_eq!(original.disposition, Disposition::Unknown);
    assert_eq!(original.recovery, Recovery::LookupOriginal);
    let lookup = journal::lookup(&state.store.pool, user.id, request.operation_id)
        .await
        .unwrap();
    assert_eq!(lookup, original);
    assert_eq!(
        submit(&state, &registry, &user, request).await.unwrap(),
        original
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn concurrent_retries_and_changed_bytes_do_not_redispatch() {
    let (_db, state, user, registry, count) = setup().await;
    let request = request("ok");
    let (one, two) = tokio::join!(
        submit(&state, &registry, &user, request.clone()),
        submit(&state, &registry, &user, request.clone())
    );
    assert!(one.is_ok() && two.is_ok());
    assert_eq!(count.load(Ordering::SeqCst), 1);
    let mut changed = request.clone();
    changed.payload = json!({"value":"changed"});
    assert_eq!(
        submit(&state, &registry, &user, changed)
            .await
            .unwrap_err()
            .status(),
        StatusCode::CONFLICT
    );
    let mut revoked = user.clone();
    revoked.role = Role::Viewer;
    assert_eq!(
        submit(&state, &registry, &revoked, request.clone())
            .await
            .unwrap_err()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        journal::lookup(&state.store.pool, Uuid::new_v4(), request.operation_id)
            .await
            .unwrap_err()
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn terminal_replay_rechecks_current_lookup_authority() {
    let (_db, state, user, registry, count) = setup().await;
    let request = request("ok");
    let original = submit(&state, &registry, &user, request.clone())
        .await
        .unwrap();
    // Model an authenticated/authorized context becoming stale during replay IO.
    // Lookup authorization reads the current row rather than the captured User.
    sqlx::query("UPDATE users SET role='viewer' WHERE id=$1")
        .bind(user.id)
        .execute(&state.store.pool)
        .await
        .unwrap();
    let error = submit(&state, &registry, &user, request.clone())
        .await
        .unwrap_err();
    assert_eq!(error.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        journal::lookup(&state.store.pool, user.id, request.operation_id)
            .await
            .unwrap(),
        original
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn strict_payloads_and_retention_preserve_unresolved_and_tombstones() {
    let (_db, state, user, registry, count) = setup().await;
    let mut bad = request("ok");
    bad.payload["command"] = json!("forbidden");
    assert_eq!(
        submit(&state, &registry, &user, bad)
            .await
            .unwrap_err()
            .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(count.load(Ordering::SeqCst), 0);
    let resolved = request("ok");
    submit(&state, &registry, &user, resolved.clone())
        .await
        .unwrap();
    let unknown = request("lost");
    submit(&state, &registry, &user, unknown.clone())
        .await
        .unwrap();
    let now = (state.clock)();
    assert_eq!(journal::retire(&state.store.pool, now).await.unwrap(), 0);
    assert_eq!(
        journal::retire(&state.store.pool, now + chrono::Duration::days(91))
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        journal::lookup(&state.store.pool, user.id, unknown.operation_id)
            .await
            .unwrap()
            .disposition,
        Disposition::Unknown
    );
    assert_eq!(
        submit(&state, &registry, &user, resolved)
            .await
            .unwrap_err()
            .status(),
        StatusCode::GONE
    );
    assert_eq!(count.load(Ordering::SeqCst), 2);
    assert!(
        sqlx::query("DELETE FROM dashboard_operations")
            .execute(&state.store.pool)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn lookup_after_router_restart_preserves_unknown_and_other_principal_is_neutral() {
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let (_db, state, user, registry, count) = setup().await;
    let request = request("lost");
    let router = routes(registry.clone()).with_state(state.clone());
    let response = router
        .oneshot(
            Request::post("/api/v1/actions")
                .header("x-aq-dev-email", "owner@localhost")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&request).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body: serde_json::Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["data"]["disposition"], "unknown");
    assert_eq!(body["api_version"], "dashboard-actions-1");
    let restarted = routes(registry.clone()).with_state(state.clone());
    let response = restarted
        .oneshot(
            Request::get(format!("/api/v1/operations/{}", request.operation_id))
                .header("x-aq-dev-email", "owner@localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let lookup: serde_json::Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(lookup["data"], body["data"]);
    assert_eq!(count.load(Ordering::SeqCst), 1);
    sqlx::query("UPDATE users SET role='viewer' WHERE id=$1")
        .bind(user.id)
        .execute(&state.store.pool)
        .await
        .unwrap();
    let revoked_lookup = routes(registry.clone())
        .with_state(state.clone())
        .oneshot(
            Request::get(format!("/api/v1/operations/{}", request.operation_id))
                .header("x-aq-dev-email", "owner@localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(revoked_lookup.status(), StatusCode::NOT_FOUND);
    let response = routes(registry)
        .with_state(state.clone())
        .oneshot(
            Request::post("/api/v1/actions")
                .header("x-aq-dev-email", "owner@localhost")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&request).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(count.load(Ordering::SeqCst), 1);
    let other = User {
        id: Uuid::new_v4(),
        ..user
    };
    let error = journal::lookup(&state.store.pool, other.id, request.operation_id)
        .await
        .unwrap_err();
    assert_eq!(error.status(), StatusCode::NOT_FOUND);
}

#[test]
fn canonical_request_identity_normalizes_keys_but_binds_scope_and_values() {
    let a:ActionRequest=serde_json::from_value(json!({"operation_id":"11111111-1111-4111-8111-111111111111","kind":"test.effect","subject":{"type":"test","id":"one"},"expected_versions":{"b":"2","a":"1"},"payload":{"z":1,"a":2}})).unwrap();
    let b:ActionRequest=serde_json::from_str(r#"{"payload":{"a":2,"z":1},"expected_versions":{"a":"1","b":"2"},"subject":{"id":"one","type":"test"},"kind":"test.effect","operation_id":"11111111-1111-4111-8111-111111111111"}"#).unwrap();
    let digest = canonical_digest(&a, &Scope::AllAuthorized).unwrap();
    assert_eq!(digest, canonical_digest(&b, &Scope::AllAuthorized).unwrap());
    assert_ne!(
        digest,
        canonical_digest(
            &b,
            &Scope::Project {
                project_id: Uuid::new_v4()
            }
        )
        .unwrap()
    );
    let mut changed = b;
    changed.reason = Some("changed".into());
    assert_ne!(
        digest,
        canonical_digest(&changed, &Scope::AllAuthorized).unwrap()
    );
    assert!(digest.starts_with("sha256:"));
    assert_eq!(digest.len(), 71);
}

#[test]
fn registry_is_finite_versioned_and_rejects_open_payload_schemas() {
    #[derive(Deserialize, JsonSchema)]
    struct Open {
        _value: String,
    }
    struct UnsafeAction;
    #[async_trait]
    impl ActionKind for UnsafeAction {
        type Payload = Open;
        const KIND: &'static str = "unsafe.run";
        const GRANT: &'static str = "unsafe.run";
        const TARGET: aq_command_registry::TargetRule = aq_command_registry::TargetRule::Subject;
        async fn authorize(
            &self,
            _: &AppState,
            _: &User,
            _: &ActionRequest,
            _: &Open,
        ) -> Result<Scope, ApiError> {
            Ok(Scope::AllAuthorized)
        }
        async fn authorize_lookup(
            &self,
            _: &AppState,
            _: &User,
            _: &Scope,
            _: Option<&Operation>,
        ) -> Result<(), ApiError> {
            Ok(())
        }
        async fn execute(&self, _: &AppState, _: &User, _: &ActionRequest, _: Open) -> Outcome {
            Outcome::unknown()
        }
    }
    let mut registry = Registry::default();
    assert!(registry.register(UnsafeAction).is_err());
    assert!(registry.schemas().is_empty());
    registry
        .register(TestAction(Arc::new(AtomicUsize::new(0))))
        .unwrap();
    assert!(
        registry
            .register(TestAction(Arc::new(AtomicUsize::new(0))))
            .is_err()
    );
    assert!(contract::schema(&Registry::default())["$defs"]["ActionRequest"]["not"].is_object());
    assert_eq!(
        contract::schema(&registry)["$defs"]["ActionRequest"]["oneOf"][0]["properties"]["kind"]["const"],
        "test.effect"
    );
}

#[tokio::test]
async fn admitted_but_interrupted_operation_remains_pending_without_redispatch() {
    let (_db, state, user, registry, count) = setup().await;
    let request = request("ok");
    let digest = canonical_digest(&request, &Scope::AllAuthorized).unwrap();
    let (original, dispatch) = journal::admit(
        &state.store.pool,
        user.id,
        &request,
        Scope::AllAuthorized,
        digest,
        (state.clock)(),
    )
    .await
    .unwrap();
    assert!(dispatch);
    // Simulate losing the admitting request after commit, before any effect.
    let replay = submit(&state, &registry, &user, request.clone())
        .await
        .unwrap();
    assert_eq!(replay, original);
    assert_eq!(replay.disposition, Disposition::Pending);
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert_eq!(
        journal::retire(
            &state.store.pool,
            (state.clock)() + chrono::Duration::days(365)
        )
        .await
        .unwrap(),
        0
    );
    assert_eq!(
        journal::lookup(&state.store.pool, user.id, request.operation_id)
            .await
            .unwrap(),
        original
    );
}

#[tokio::test]
async fn database_guards_prevent_identity_changes_early_retirement_and_schema_loss() {
    let (_db, state, user, registry, _) = setup().await;
    let request = request("ok");
    submit(&state, &registry, &user, request.clone())
        .await
        .unwrap();
    for sql in [
        "UPDATE dashboard_operations SET request_digest='sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'",
        "UPDATE dashboard_operations SET principal_id='11111111-1111-4111-8111-111111111111'",
        "UPDATE dashboard_operations SET scope='{\"kind\":\"project\"}'::jsonb",
        "UPDATE dashboard_operations SET response=NULL,retired_at=updated_at+interval '89 days'",
        "TRUNCATE dashboard_operations",
    ] {
        assert!(
            sqlx::query(sql).execute(&state.store.pool).await.is_err(),
            "{sql}"
        );
    }
    sqlx::query("DROP TRIGGER dashboard_operation_guard ON dashboard_operations")
        .execute(&state.store.pool)
        .await
        .unwrap();
    assert!(
        crate::store::Store::connect_with(_db.options())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn admission_failure_never_dispatches_and_lost_completion_stays_recoverable() {
    let (_db, state, user, registry, count) = setup().await;
    sqlx::query("CREATE FUNCTION reject_test_operation() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'test failure'; END $$") .execute(&state.store.pool).await.unwrap();
    sqlx::query("CREATE TRIGGER reject_test_admission BEFORE INSERT ON dashboard_operations FOR EACH ROW EXECUTE FUNCTION reject_test_operation()") .execute(&state.store.pool).await.unwrap();
    let request = request("ok");
    assert_eq!(
        submit(&state, &registry, &user, request.clone())
            .await
            .unwrap_err()
            .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert_eq!(
        journal::lookup(&state.store.pool, user.id, request.operation_id)
            .await
            .unwrap_err()
            .status(),
        StatusCode::NOT_FOUND
    );
    sqlx::query("DROP TRIGGER reject_test_admission ON dashboard_operations")
        .execute(&state.store.pool)
        .await
        .unwrap();
    sqlx::query("CREATE TRIGGER reject_test_completion BEFORE UPDATE ON dashboard_operations FOR EACH ROW EXECUTE FUNCTION reject_test_operation()") .execute(&state.store.pool).await.unwrap();
    let operation = submit(&state, &registry, &user, request.clone())
        .await
        .unwrap();
    assert_eq!(operation.disposition, Disposition::Pending);
    assert_eq!(operation.recovery, Recovery::LookupOriginal);
    assert_eq!(
        journal::lookup(&state.store.pool, user.id, request.operation_id)
            .await
            .unwrap(),
        operation
    );
    assert_eq!(
        submit(&state, &registry, &user, request).await.unwrap(),
        operation
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn timed_out_effect_is_unknown_and_never_reexecuted() {
    let (_db, state, user, registry, count) = setup().await;
    let request = request("timeout");
    let operation = submit(&state, &registry, &user, request.clone())
        .await
        .unwrap();
    assert_eq!(operation.disposition, Disposition::Unknown);
    assert_eq!(operation.recovery, Recovery::LookupOriginal);
    assert_eq!(
        submit(&state, &registry, &user, request.clone())
            .await
            .unwrap(),
        operation
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn outer_deadline_after_admission_returns_the_retained_operation() {
    use axum::{
        body::Body,
        http::Request,
        middleware::{Next, from_fn},
        response::IntoResponse,
    };
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    for revoked in [false, true] {
        let (_db, state, user, registry, count) = setup().await;
        let request = request("timeout");
        // Same cancellation semantics as the production admission gate, with a
        // short deterministic deadline after the adapter has begun its effect.
        let started = count.clone();
        let pool = state.store.pool.clone();
        let actor = user.id;
        let router=routes(registry.clone()).layer(from_fn(move |req:Request<Body>,next:Next| {
        let started=started.clone();
        let pool=pool.clone();
        async move {
            let dispatch=next.run(req);
            tokio::pin!(dispatch);
            tokio::select! {
                response=&mut dispatch=>response,
                _=async {while started.load(Ordering::SeqCst)==0 {tokio::task::yield_now().await;}}=> {
                    if revoked { sqlx::query("UPDATE users SET role='viewer' WHERE id=$1").bind(actor).execute(&pool).await.unwrap(); }
                    StatusCode::SERVICE_UNAVAILABLE.into_response()
                },
            }
        }
    })).layer(from_fn(super::super::errors::normalize_errors)).with_state(state.clone());
        let response = router
            .oneshot(
                Request::post("/api/v1/actions")
                    .header("x-aq-dev-email", "owner@localhost")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&request).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            if revoked {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                StatusCode::ACCEPTED
            }
        );
        let body: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        if revoked {
            assert!(body.get("data").is_none(), "{body}");
            assert!(body.get("pending_operations").is_none(), "{body}");
            sqlx::query("UPDATE users SET role='owner' WHERE id=$1")
                .bind(actor)
                .execute(&state.store.pool)
                .await
                .unwrap();
        } else {
            assert_eq!(
                body["data"]["operation_id"],
                request.operation_id.to_string()
            );
            assert_eq!(body["data"]["disposition"], "pending");
        }
        assert_eq!(
            submit(&state, &registry, &user, request)
                .await
                .unwrap()
                .disposition,
            Disposition::Pending
        );
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn lookup_requires_current_policy_for_retired_and_unregistered_kinds() {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;
    let (_db, state, user, registry, _) = setup().await;
    let request = request("ok");
    submit(&state, &registry, &user, request.clone())
        .await
        .unwrap();
    journal::retire(
        &state.store.pool,
        (state.clock)() + chrono::Duration::days(91),
    )
    .await
    .unwrap();
    let lookup = || {
        Request::get(format!("/api/v1/operations/{}", request.operation_id))
            .header("x-aq-dev-email", "owner@localhost")
            .body(Body::empty())
            .unwrap()
    };
    assert_eq!(
        routes(registry.clone())
            .with_state(state.clone())
            .oneshot(lookup())
            .await
            .unwrap()
            .status(),
        StatusCode::GONE
    );
    assert_eq!(
        routes(Arc::new(Registry::default()))
            .with_state(state.clone())
            .oneshot(lookup())
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    sqlx::query("UPDATE users SET role='viewer' WHERE id=$1")
        .bind(user.id)
        .execute(&state.store.pool)
        .await
        .unwrap();
    assert_eq!(
        routes(registry)
            .with_state(state)
            .oneshot(lookup())
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
}

#[test]
fn dashboard_action_schema_lists_task_and_knowledge_kinds() {
    let registry = existing::schemas();
    for kind in [
        "task.graph.create",
        "task.edit",
        "task.activate",
        "task.reopen",
        "task.complete",
        "task.cancel",
        "task.retry",
        "task.reprioritize",
        "task.dependency.add",
        "task.dependency.remove",
        "knowledge.author",
        "knowledge.correct",
        "knowledge.verify",
        "knowledge.retire",
        "knowledge.restore",
        "playbook.run.start",
        "playbook.run.cancel",
        "playbook.activation.activate",
        "playbook.activation.rollback",
    ] {
        let schema = registry
            .schemas()
            .get(kind)
            .unwrap_or_else(|| panic!("{kind} schema"));
        assert_eq!(schema["type"], "object", "{kind}");
        assert_eq!(schema["additionalProperties"], false, "{kind}");
        for field in ["operation_id", "project_id"] {
            assert!(schema["properties"][field].is_object(), "{kind} {field}");
        }
    }
}
