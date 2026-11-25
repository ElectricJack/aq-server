use super::*;

const ROUTING: &str = "/api/settings/routing";

/// Bootstraps the owner, so the admin router admits the dev login.
async fn owner(e: &Env) {
    let now = Utc.timestamp_millis_opt(NOW_MS).unwrap();
    let store = &e.state.store;
    let _ = store
        .bootstrap_owner("owner@localhost", "dev", "owner@localhost", now)
        .await;
}

/// The owner's admin router over `e`'s state.
async fn owner_app(e: &Env) -> Router {
    owner(e).await;
    aq_server::build_admin_router(e.state.clone())
}

async fn call(app: &Router, method: &str, body: Option<Value>) -> (StatusCode, Value) {
    let request = axum::http::Request::builder()
        .uri(ROUTING)
        .method(method)
        .header("X-AQ-Request", "1")
        .header("content-type", "application/json");
    let body = body.map_or_else(axum::body::Body::empty, |b| b.to_string().into());
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

/// Spec 0011 §4 W6: while routing has left a task without a route, the route read says why,
/// and the failure raised for it is open. A route (here the owner's) resolves both.
#[tokio::test]
async fn a_route_resolves_what_was_raised_and_the_read_stops_saying_unrouted() {
    use aq_internal_work::{Affected, Purpose, Reason, store};
    let e = setup().await;
    let app = owner_app(&e).await;
    let read = || async {
        let request = axum::http::Request::get(format!("/api/tasks/{}/routes", e.task))
            .header("x-aq-dev-email", "owner@localhost")
            .body(axum::body::Body::empty())
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice::<Value>(&bytes).unwrap()
    };
    assert_eq!(read().await, json!({"items": []}));
    let reasons = json!([{"code": "policy", "detail": "Never route payroll work."}]);
    sqlx::query("INSERT INTO route_needed_emissions(task_id,task_version,emitted_at_ms,outcome,reasons) VALUES($1,3,1,'unroutable',$2)")
        .bind(e.task)
        .bind(&reasons)
        .execute(&e.pool)
        .await
        .unwrap();
    let affected = Affected {
        kind: "task".into(),
        id: e.task.to_string(),
        project_id: None,
    };
    let purpose = Purpose::RoutingClassification;
    let failure = (
        Reason::NoCandidate,
        "The routing document allows no agent to take this task.",
    );
    let raised = store::Queue::at(&e.pool, 1).record_failure(
        store::FailureCause {
            purpose,
            affected: &affected,
            cause: "unroutable@3",
        },
        failure,
    );
    let raised = raised.await.unwrap().unwrap();
    assert_eq!(
        read().await,
        json!({"items": [], "unrouted": {"outcome": "unroutable", "reasons": reasons, "task_version": 3}})
    );
    // A stale write changes nothing; a route that is written resolves it.
    assert_eq!(
        e.write(2, &route(Source::Owner)).await.unwrap(),
        Written::Stale
    );
    assert!(read().await.get("unrouted").is_some());
    assert!(matches!(
        e.write(3, &route(Source::Owner)).await.unwrap(),
        Written::Routed { .. }
    ));
    let body = read().await;
    assert_eq!(body.get("unrouted"), None);
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    let run = store::get(&e.pool, raised).await.unwrap().unwrap();
    assert_eq!(run.escalation, "resolved");
    // Nothing is left for the escalation step to send anywhere.
    assert!(store::escalate_due(&e.pool, 2).await.unwrap().is_empty());
}

/// Spec 0013 §7: the switch needs an active routing playbook. None can be active yet (the
/// decision table is retired), so turning routing on is refused; turning it off always works.
#[tokio::test]
async fn routing_cannot_be_switched_on_without_a_routing_playbook() {
    let e = setup().await;
    let app = owner_app(&e).await;
    let (status, body) = call(&app, "POST", Some(json!({"enabled": true}))).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(
        body["message"],
        "Routing can be turned on once a routing playbook is active"
    );
    assert!(!routing::enabled(&e.pool).await.unwrap());
    // Before the server has written the routing source there is no playbook to show.
    let source = json!({"id": "routing", "kind": "builtin", "binding": "aq.routing",
        "active": null, "latest": null});
    let off = json!({"enabled": false, "source": source});
    assert_eq!(call(&app, "GET", None).await, (StatusCode::OK, off.clone()));
    // A switch left on by an earlier build is turned off.
    sqlx::query("UPDATE routing_state SET enabled=TRUE")
        .execute(&e.pool)
        .await
        .unwrap();
    let (status, body) = call(&app, "GET", None).await;
    assert_eq!(
        (status, body["enabled"].clone()),
        (StatusCode::OK, json!(true))
    );
    let turned_off = call(&app, "POST", Some(json!({"enabled": false}))).await;
    assert_eq!(turned_off, (StatusCode::OK, off));
    assert!(!routing::enabled(&e.pool).await.unwrap());
}

/// Spec 0014 §5.2: what Settings offers is each class that has a level document, with every
/// (provider, harness) pair that level's candidates list, once each. No harness is qualified on
/// a worker here: qualification is placement's question, not what is offered (a309a837).
#[tokio::test]
async fn options_offer_each_level_every_pair_its_candidates_list() {
    use crate::common::roles;
    let e = setup().await;
    owner(&e).await;
    let actor: Uuid = sqlx::query_scalar("SELECT id FROM users LIMIT 1")
        .fetch_one(&e.pool)
        .await
        .unwrap();
    let shipped = routing::options(&e.state).await.unwrap();
    let snapshot = roles::active(&e.pool).await;
    // The shipped defaults: every offered class has a level document.
    assert!(!shipped.classes.is_empty());
    assert!((shipped.classes.keys()).all(|c| snapshot.intelligence(&c.to_string()).is_some()));
    let candidates = [
        ("fixture-a", "model-a", "high"),
        ("fixture-a", "model-b", "low"),
        ("fixture-b", "model-c", "high"),
    ];
    let level = roles::level("standard-high", &candidates);
    roles::save(&e.pool, actor, Kind::Intelligence, "standard-high", &level).await;
    let offer = |provider: &str| Offer {
        provider: provider.into(),
        harness: roles::HARNESS.into(),
    };
    let options = routing::options(&e.state).await.unwrap();
    assert_eq!(
        options.classes[&class("standard-high")],
        ["fixture-a", "fixture-b"].map(offer)
    );
    // What the routing document's author is told about a level is its
    // document: the id, the name and the description.
    let saved = roles::active(&e.pool).await;
    let document = saved.intelligence("standard-high").unwrap();
    let facts = options.facts();
    assert!(
        facts["levels"]
            .as_array()
            .unwrap()
            .contains(&json!("standard-high"))
    );
    assert_eq!(
        facts["level_descriptions"]["standard-high"],
        json!({"name": document.name, "description": document.description})
    );
    assert!(!document.description.is_empty());
    let levels = facts["levels"].as_array().unwrap().len();
    assert_eq!(
        facts["level_descriptions"].as_object().unwrap().len(),
        levels
    );

    // The authoring host reads the routing document the owner saved.
    use aq_playbook_authoring::{Host, builtin};
    let read = Host::markdown(&e.state, &builtin::routing()).await.unwrap();
    assert_eq!(read, (String::new(), 0));
    let id = Uuid::new_v4();
    let receipt = aq_role_settings::save_routing_document(
        aq_role_settings::Writer::new(&e.pool, actor, 1).edit(0, id),
        "Reviews go to deep.",
    )
    .await
    .unwrap();
    let read = Host::markdown(&e.state, &builtin::routing()).await.unwrap();
    assert_eq!(
        read,
        ("Reviews go to deep.".to_owned(), receipt.revision as u64)
    );
    let facts = Host::facts(&e.state, &builtin::routing()).await.unwrap();
    assert!(facts.contains_key("level_descriptions"));
}
