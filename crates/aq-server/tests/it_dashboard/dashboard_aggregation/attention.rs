use super::*;

fn classified_task(project: uuid::Uuid, task: uuid::Uuid) -> aq_internal_work::Request {
    use aq_internal_work::{Affected, AnswerShape, Purpose, Request as Work};
    Work {
        purpose: Purpose::RoutingClassification,
        instructions: "Classify the task.".into(),
        input: "Task: fix the login page".into(),
        answer: AnswerShape::Choice {
            choices: vec!["small".into(), "large".into()],
            rationale_max_bytes: 512,
        },
        blocking: false,
        affected: Affected {
            kind: "task".into(),
            id: task.to_string(),
            project_id: Some(project),
        },
        deadline_ms: 60_000,
        max_output_bytes: 4_096,
        escalate: true,
        profile_override: None,
        agent: None,
    }
}

/// Spec 0012 §8 step 3: a failed internal run a supervisor could not fix
/// opens the affected item, not the run, and leaves once it is resolved.
#[tokio::test]
async fn owner_escalated_internal_run_is_an_attention_item_pointing_at_the_affected_item() {
    use aq_internal_work::store;
    use serde_json::json;
    use uuid::Uuid;
    let f = common::profiled::fixture().await;
    common::profiled::insert(&f).await;
    let pool = f.db.pool().await;
    let project = f.selection.lineage.project_id;
    let task = Uuid::new_v4();
    let work = classified_task(project, task);
    let failed = store::submit(&pool, &work, &[], 1_000).await.unwrap();
    // With a supervisor only: not the owner's yet.
    let waiting = store::submit(&pool, &work, &[], 1_000).await.unwrap();
    assert_eq!(store::escalate_due(&pool, 1_000).await.unwrap().len(), 2);
    let diagnosis = "profile internal has no working model";
    store::mark_owner(&pool, failed, diagnosis, 2_000)
        .await
        .unwrap();
    let app = aq_server::build_admin_router(f.state.clone());
    let internal = |body: &Value| -> Vec<Value> {
        body["data"]["attention"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|v| v["kind"] == "internal_work")
            .cloned()
            .collect()
    };
    let (status, body) = read(&app, "/api/v1/overview").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = internal(&body);
    assert_eq!(items.len(), 1, "{body}");
    assert_eq!(
        items[0],
        json!({
            "id": format!("internal_work:{failed}"),
            "kind": "internal_work",
            "title": "Internal work failed",
            "project": body["data"]["in_motion"][0]["project"],
            "reason": diagnosis,
            "impact": "Review the owning record for current context",
            "since": "1970-01-01T00:00:02.000Z",
            "subject": {"type": "task", "id": task, "revision": items[0]["subject"]["revision"]},
            "action": null,
            "dismissible": true
        })
    );
    assert_eq!(items[0]["project"]["id"], json!(project));
    assert!(!body.to_string().contains(&waiting.to_string()));
    assert!(
        body["data"]["sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["source"] == "internal_work" && s["coverage"]["state"] == "complete"),
        "{body}"
    );
    let (_, other) = read(
        &app,
        &format!("/api/v1/overview?scope=project:{}", Uuid::new_v4()),
    )
    .await;
    assert!(internal(&other).is_empty());
    store::resolve(&pool, failed).await.unwrap();
    let (_, body) = read(&app, "/api/v1/overview").await;
    assert!(internal(&body).is_empty(), "{body}");
}

#[tokio::test]
async fn owner_escalated_routing_failure_points_at_the_task() {
    use aq_internal_work::{Affected, Purpose, Reason, store};
    use serde_json::json;
    use uuid::Uuid;
    let f = common::profiled::fixture().await;
    common::profiled::insert(&f).await;
    let pool = f.db.pool().await;
    let task = Uuid::new_v4();
    let affected = Affected {
        kind: "task".into(),
        id: task.to_string(),
        project_id: Some(f.selection.lineage.project_id),
    };
    let words = "The routing document allows no agent to take this task, so it is left unrouted.";
    let failure = (Reason::NoCandidate, words);
    let purpose = Purpose::RoutingClassification;
    let record = |cause: &'static str| {
        store::Queue::at(&pool, 1_000).record_failure(
            store::FailureCause {
                purpose,
                affected: &affected,
                cause,
            },
            failure,
        )
    };
    let run = record("unroutable@3").await.unwrap().unwrap();
    assert_eq!(
        record("unroutable@3").await.unwrap(),
        None,
        "once per cause"
    );
    assert_eq!(store::escalate_due(&pool, 1_000).await.unwrap().len(), 1);
    (store::mark_owner(&pool, run, "the document forbids it", 2_000).await).unwrap();
    let app = aq_server::build_admin_router(f.state.clone());
    let (status, body) = read(&app, "/api/v1/overview").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body["data"]["attention"].as_array().unwrap();
    let item = (items
        .iter()
        .find(|v| v["id"] == format!("internal_work:{run}")))
    .unwrap();
    assert_eq!(
        item["subject"],
        json!({"type": "task", "id": task, "revision": item["subject"]["revision"]})
    );
    assert_eq!(item["title"], "A task could not be routed");
    assert_eq!(item["reason"], words);
    // A route resolves it: the item is gone.
    assert_eq!(
        store::resolve_failures(&pool, purpose, &affected)
            .await
            .unwrap(),
        1
    );
    let (_, body) = read(&app, "/api/v1/overview").await;
    let items = body["data"]["attention"].as_array().unwrap();
    assert!(
        !items
            .iter()
            .any(|v| v["id"] == format!("internal_work:{run}"))
    );
}

#[tokio::test]
async fn owner_escalated_internal_runs_are_never_dropped() {
    use aq_internal_work::store;
    use serde_json::json;
    use uuid::Uuid;
    let f = common::profiled::fixture().await;
    common::profiled::insert(&f).await;
    let pool = f.db.pool().await;
    let policy = internal_request("playbook_source", "routing", None);
    let policy = store::submit(&pool, &policy, &[], 1_000).await.unwrap();
    let operation = internal_request("playbook_operation", &Uuid::new_v4().to_string(), None);
    let operation = store::submit(&pool, &operation, &[], 1_000).await.unwrap();
    let task = Uuid::new_v4();
    let elsewhere = internal_request("task", &task.to_string(), Some(Uuid::new_v4()));
    let elsewhere = store::submit(&pool, &elsewhere, &[], 1_000).await.unwrap();
    for id in [policy, operation, elsewhere] {
        store::mark_owner(&pool, id, "no working model", 2_000)
            .await
            .unwrap();
    }
    let app = aq_server::build_admin_router(f.state.clone());
    let (status, body) = read(&app, "/api/v1/overview").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let item = |run: Uuid| {
        body["data"]["attention"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["id"] == format!("internal_work:{run}"))
            .cloned()
            .unwrap_or_else(|| panic!("no item for {run}: {body}"))
    };
    let shown = item(policy);
    assert_eq!(
        shown["subject"],
        json!({"type": "internal_run", "id": policy, "revision": shown["subject"]["revision"]})
    );
    assert_eq!(shown["impact"], "Playbook source routing");
    assert_eq!(shown["reason"], "no working model");
    assert!(shown["project"].is_null());
    let shown = item(operation);
    assert_eq!(shown["subject"]["type"], "internal_run");
    let operation_id = store::get(&pool, operation)
        .await
        .unwrap()
        .unwrap()
        .affected_id;
    assert_eq!(
        shown["impact"],
        format!("Playbook operation {operation_id}")
    );
    let shown = item(elsewhere);
    assert_eq!(
        shown["subject"],
        json!({"type": "task", "id": task, "revision": shown["subject"]["revision"]})
    );
    assert!(shown["project"].is_null(), "{shown}");
    assert!(
        body["data"]["sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["source"] == "internal_work" && s["coverage"]["state"] == "complete"),
        "{body}"
    );
}

async fn post(app: &axum::Router, id: &str, revision: &str) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::post(format!("/api/v1/attention/{id}/dismiss"))
                .header("x-aq-dev-email", "owner@localhost")
                .header("x-aq-request", "1")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"expected_revision": revision}).to_string(),
                ))
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

/// View All lists every escalated run the overview shows (past one page), a
/// cleared run leaves both views and stays cleared, and with the decision owner
/// unmounted the total is unknown rather than zero.
#[tokio::test]
async fn escalated_internal_runs_are_listed_in_full_and_can_be_cleared() {
    use aq_internal_work::store;
    use serde_json::json;
    use uuid::Uuid;
    let f = common::profiled::fixture().await;
    common::profiled::insert(&f).await;
    let pool = f.db.pool().await;
    let project = f.selection.lineage.project_id;
    let mut runs = vec![];
    for _ in 0..3 {
        let work = classified_task(project, Uuid::new_v4());
        let run = store::submit(&pool, &work, &[], 1_000).await.unwrap();
        store::mark_owner(&pool, run, "no working model", 2_000)
            .await
            .unwrap();
        runs.push(run);
    }
    let app = aq_server::build_admin_router(f.state.clone());
    let (status, bootstrap) = read(&app, "/api/v1/bootstrap").await;
    assert_eq!(status, StatusCode::OK, "{bootstrap}");
    assert!(
        bootstrap["data"]["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|cap| cap["name"] == "attention.dismiss" && cap["enabled"] == true)
    );
    let ids = |body: &Value, field: &str| -> Vec<String> {
        let mut ids: Vec<String> = body["data"][field]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["id"].as_str().unwrap().to_owned())
            .collect();
        ids.sort();
        ids
    };
    let (_, overview) = read(&app, "/api/v1/overview").await;
    let (status, list) = read(&app, "/api/v1/attention").await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(ids(&list, "items"), ids(&overview, "attention"));
    assert_eq!(list["data"]["items"].as_array().unwrap().len(), 3);
    assert!(list["data"]["total"]["value"].is_null(), "{list}");
    assert!(list["data"]["items"][0]["dismissible"] == true);
    let (_, page) = read(&app, "/api/v1/attention?limit=2").await;
    assert_eq!(page["data"]["items"].as_array().unwrap().len(), 2);
    assert_eq!(page["data"]["has_more"], true);
    let cursor = page["data"]["next_cursor"].as_str().unwrap();
    let (_, rest) = read(&app, &format!("/api/v1/attention?limit=2&cursor={cursor}")).await;
    assert_eq!(rest["data"]["items"].as_array().unwrap().len(), 1);
    assert_eq!(rest["data"]["has_more"], false);

    let cleared = format!("internal_work:{}", runs[0]);
    let revision = |body: &Value, id: &str| -> String {
        body["data"]["attention"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["id"] == id)
            .unwrap()["subject"]["revision"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let pin = revision(&overview, &cleared);
    let (status, first) = post(&app, &cleared, &pin).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    f.time.fetch_add(100, std::sync::atomic::Ordering::SeqCst);
    let (status, replay) = post(&app, &cleared, &pin).await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(first, replay);
    for kind in ["decision", "question", "blocked"] {
        let (status, body) = post(&app, &format!("{kind}:{}", Uuid::new_v4()), "1").await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["code"], "attention_not_dismissible");
    }
    // A new router models refresh/restart: state is read from the database.
    let app = aq_server::build_admin_router(f.state.clone());
    let (_, refreshed) = read(&app, "/api/v1/overview").await;
    let (_, list) = read(&app, "/api/v1/attention").await;
    assert_eq!(ids(&refreshed, "attention").len(), 2);
    assert_eq!(ids(&list, "items"), ids(&refreshed, "attention"));
    assert!(!ids(&list, "items").contains(&cleared));
    assert_eq!(
        refreshed["data"]["counts"]["needs_you"]["value"],
        list["data"]["total"]["value"]
    );
    let open: i64 =
        sqlx::query_scalar("SELECT count(*) FROM internal_tasks WHERE escalation='owner'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(open, 3, "dismissal never resolves the run");

    // Dismissal belongs only to the principal who made it.
    sqlx::query("INSERT INTO users(id,email,role,created_at) VALUES($1,'another-owner@localhost','owner',now())")
        .bind(Uuid::new_v4()).execute(&pool).await.unwrap();
    let response = app
        .clone()
        .oneshot(
            Request::get("/api/v1/overview")
                .header("x-aq-dev-email", "another-owner@localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let another: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(ids(&another, "attention").contains(&cleared));

    // Stored notifications never evict older dismissals.
    sqlx::query("INSERT INTO dashboard_attention_dismissals(principal_id,attention_id,expected_revision,dismissed_at_ms) SELECT principal_id,'result:retained-' || n,'1',2 FROM dashboard_attention_dismissals CROSS JOIN generate_series(1,5000) AS n WHERE attention_id=$1")
        .bind(&cleared).execute(&pool).await.unwrap();
    assert_eq!(post(&app, &cleared, &pin).await.1, first);

    // A changed source revision reappears and rejects the old pin, including a replay.
    sqlx::query("UPDATE internal_tasks SET diagnosis='new diagnosis' WHERE id=$1")
        .bind(runs[0])
        .execute(&pool)
        .await
        .unwrap();
    let (status, body) = post(&app, &cleared, &pin).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "attention_revision_changed");
    let (_, changed) = read(&app, "/api/v1/overview").await;
    assert!(ids(&changed, "attention").contains(&cleared));
    let new_pin = revision(&changed, &cleared);
    assert_ne!(pin, new_pin);
    assert_eq!(post(&app, &cleared, &new_pin).await.0, StatusCode::OK);

    // An already-gone subject is successful and replays the same record.
    let gone = format!("internal_work:{}", Uuid::new_v4());
    let (status, first) = post(&app, &gone, "1").await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(post(&app, &gone, "1").await.1, first);

    // Both protected affected kinds are refused even under an internal_work id.
    for kind in ["decision", "question"] {
        let work = internal_request(kind, &Uuid::new_v4().to_string(), Some(project));
        let run = store::submit(&pool, &work, &[], 1_000).await.unwrap();
        store::mark_owner(&pool, run, "needs a decision", 2_000)
            .await
            .unwrap();
        let id = format!("internal_work:{run}");
        let (_, overview) = read(&app, "/api/v1/overview").await;
        assert_eq!(
            post(&app, &id, &revision(&overview, &id)).await.0,
            StatusCode::CONFLICT
        );
    }

    // Authorization is rechecked even on replay of an existing dismissal.
    sqlx::query("UPDATE users SET role='viewer' WHERE id=$1")
        .bind(f.actor)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        post(&app, &cleared, &new_pin).await.0,
        StatusCode::FORBIDDEN
    );
}
