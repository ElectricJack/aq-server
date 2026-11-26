use super::*;

#[tokio::test]
async fn task_reprioritize_preserves_other_metadata() {
    let x = setup().await;
    let task = x.f.selection.lineage.task_id;
    let mut original = node(task, x.workspace(), "task", 5);
    original["metadata"]["example:weight"] = json!(2.5);
    x.put(original);
    let mut updated = node(task, x.workspace(), "task", 6);
    updated["metadata"]["aq:priority"] = json!(7);
    x.reply(200, change_set(vec![updated]));
    let op = Uuid::new_v4();
    let (status, body) = x
        .act(&action(
            "task.reprioritize",
            Subject::Task(task, "5"),
            json!({"operation_id":op,"project_id":x.project(),"expected_version":"5","priority":7}),
        ))
        .await;
    assert_eq!(body["data"]["disposition"], "confirmed", "{status} {body}");
    let posts = x.posts();
    assert_eq!(
        posts[0]["operations"][0]["patch"],
        json!({"metadata":{"example:weight":2.5,"aq:priority":7}})
    );
}

#[tokio::test]
async fn task_route_set_writes_an_owner_route_and_is_refused_for_a_class_without_a_level() {
    let x = setup().await;
    let task = x.f.selection.lineage.task_id;
    x.put(node(task, x.workspace(), "task", 5));
    x.reply(200, change_set(vec![node(task, x.workspace(), "task", 6)]));
    let (status, body) = x
        .act(&x.route_action(
            "task.route.set",
            (task, "5"),
            json!({"class":"standard-high","harnesses":["codex"]}),
        ))
        .await;
    assert_eq!(body["data"]["disposition"], "confirmed", "{status} {body}");
    let posts = x.posts();
    assert_eq!(posts.len(), 1);
    let metadata = &posts[0]["operations"][0]["patch"]["metadata"];
    assert_eq!(metadata["example:weight"], 2);
    assert_eq!(metadata["aq:route"]["c"], "standard-high");
    assert_eq!(metadata["aq:route"]["h"], json!(["codex"]));
    assert_eq!(metadata["aq:route"]["i"], "any");
    assert_eq!(metadata["aq:route"]["s"], "owner");
    let history = aq_server::routing::history(&x.f.db.pool().await, task)
        .await
        .unwrap();
    assert_eq!(history.len(), 1);
    let route = history[0].route.as_ref().unwrap();
    assert_eq!(route.reasons[0].code, "owner_set");
    assert_eq!(route.policy_digest, None);

    let (status, body) = x
        .act(&x.route_action("task.route.set", (task, "6"), json!({"class":"deep-max"})))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(
        x.posts().len(),
        1,
        "a class with no level document never reaches Graph"
    );
}

#[tokio::test]
async fn task_route_clear_removes_it() {
    let x = setup().await;
    let task = x.f.selection.lineage.task_id;
    x.put(node(task, x.workspace(), "task", 5));
    x.reply(200, change_set(vec![node(task, x.workspace(), "task", 6)]));
    let (_, body) = x
        .act(&x.route_action(
            "task.route.set",
            (task, "5"),
            json!({"class":"standard-high"}),
        ))
        .await;
    assert_eq!(body["data"]["disposition"], "confirmed", "{body}");
    let mut routed = node(task, x.workspace(), "task", 6);
    routed["metadata"] = x.posts()[0]["operations"][0]["patch"]["metadata"].clone();
    x.put(routed);
    x.reply(200, change_set(vec![node(task, x.workspace(), "task", 7)]));
    let (status, body) = x
        .act(&x.route_action("task.route.clear", (task, "6"), json!({})))
        .await;
    assert_eq!(body["data"]["disposition"], "confirmed", "{status} {body}");
    let posts = x.posts();
    let metadata = &posts[1]["operations"][0]["patch"]["metadata"];
    assert!(metadata.get("aq:route").is_none(), "{metadata}");
    assert_eq!(metadata["example:weight"], 2);
    let history = aq_server::routing::history(&x.f.db.pool().await, task)
        .await
        .unwrap();
    assert!(
        history[0].route.is_none(),
        "the clear is the newest history row"
    );
}

#[tokio::test]
async fn stale_task_version_is_reported_as_stale() {
    let x = setup().await;
    let task = x.f.selection.lineage.task_id;
    x.put(node(task, x.workspace(), "task", 7));
    for (kind, fields) in [
        ("task.route.set", json!({"class":"standard-high"})),
        ("task.route.clear", json!({})),
    ] {
        let (status, body) = x.act(&x.route_action(kind, (task, "5"), fields)).await;
        assert_eq!(
            body["data"]["disposition"], "conflict",
            "{kind} {status} {body}"
        );
        assert_eq!(body["data"]["current_versions"], json!({"task":"7"}));
    }
    assert!(x.posts().is_empty());
}

/// Spec 0014 §5.5: a filed task names its role; it must exist, and a disabled
/// role may be named.
#[tokio::test]
async fn a_filed_task_names_an_existing_role_and_profile_hint_is_gone() {
    let x = setup().await;
    let pool = x.f.db.pool().await;
    // Without a role nothing is stored.
    let plain = Uuid::new_v4();
    x.reply(200, change_set(vec![node(plain, x.workspace(), "task", 1)]));
    let (status, body) = x.act(&single_create(&x, plain, json!({}))).await;
    assert_eq!(body["data"]["disposition"], "confirmed", "{status} {body}");
    let metadata = x.posts()[0]["operations"][0]["metadata"].clone();
    assert!(metadata.get("aq:role").is_none(), "{metadata}");
    // An unknown role is refused with a typed error that lists the roles.
    let (status, body) = x
        .act(&single_create(&x, Uuid::new_v4(), json!({"role":"tester"})))
        .await;
    assert_unknown_role(status, &body, UNKNOWN_TESTER);
    // The old way of naming a profile is an unknown field.
    let (status, body) = x
        .act(&single_create(
            &x,
            Uuid::new_v4(),
            json!({"profile_hint":"coding"}),
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "invalid_request", "{body}");
    assert_eq!(x.posts().len(), 1, "a refused create never reaches Graph");
    // A disabled role may be named: the task is filed (and then held).
    common::save_role(&pool, x.f.actor, "reviewer", "reviewer", false).await;
    let held = Uuid::new_v4();
    x.reply(200, change_set(vec![node(held, x.workspace(), "task", 1)]));
    let (status, body) = x
        .act(&single_create(&x, held, json!({"role":"reviewer"})))
        .await;
    assert_eq!(body["data"]["disposition"], "confirmed", "{status} {body}");
    assert_eq!(
        x.posts()[1]["operations"][0]["metadata"]["aq:role"],
        "reviewer"
    );
}

fn role_set(x: &Fixture, task: Uuid, version: &str, role: Value) -> Value {
    x.route_action("task.role.set", (task, version), json!({"role":role}))
}

/// The owner changes a task's role while no attempt of it has been granted.
#[tokio::test]
async fn task_role_set_sets_and_clears_the_role_and_is_refused_after_a_grant() {
    let x = setup().await;
    let pool = x.f.db.pool().await;
    let task = x.f.selection.lineage.task_id;
    let state = |state: &'static str| {
        let pool = pool.clone();
        async move {
            let set = format!(
                "UPDATE execution_attempts SET state=state||'{state}'::jsonb WHERE task_id=$1"
            );
            sqlx::query(&set).bind(task).execute(&pool).await.unwrap();
            sqlx::query("DELETE FROM profiled_authority_hot WHERE attempt_id IN (SELECT attempt_id FROM execution_attempts WHERE task_id=$1)")
                .bind(task).execute(&pool).await.unwrap();
        }
    };
    // An attempt that was offered and not granted does not fix the role.
    state(r#"{"status":"offered","grant":null}"#).await;
    x.put(node(task, x.workspace(), "task", 5));
    x.reply(200, change_set(vec![node(task, x.workspace(), "task", 6)]));
    let (status, body) = x.act(&role_set(&x, task, "5", json!("reviewer"))).await;
    assert_eq!(body["data"]["disposition"], "confirmed", "{status} {body}");
    // One Graph revision, with every other metadata key kept: the task's
    // history shows it as a `role` change (aq-human-work).
    let posts = x.posts();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0]["operations"][0]["op"], "update_node");
    assert_eq!(posts[0]["operations"][0]["expected_version"], 5);
    let metadata = &posts[0]["operations"][0]["patch"]["metadata"];
    assert_eq!(metadata["aq:role"], "reviewer");
    assert_eq!(metadata["example:weight"], 2);

    // An unknown role is refused; a stale version conflicts.
    let mut named = node(task, x.workspace(), "task", 6);
    named["metadata"] = metadata.clone();
    x.put(named);
    let (status, body) = x.act(&role_set(&x, task, "6", json!("tester"))).await;
    assert_unknown_role(status, &body, UNKNOWN_TESTER);
    let (_, body) = x.act(&role_set(&x, task, "5", json!("developer"))).await;
    assert_eq!(body["data"]["disposition"], "conflict", "{body}");
    assert_eq!(body["data"]["current_versions"], json!({"task":"6"}));
    assert_eq!(x.posts().len(), 1);

    // Clearing removes the key, so the project's default role applies.
    x.reply(200, change_set(vec![node(task, x.workspace(), "task", 7)]));
    let (status, body) = x.act(&role_set(&x, task, "6", Value::Null)).await;
    assert_eq!(body["data"]["disposition"], "confirmed", "{status} {body}");
    let cleared = &x.posts()[1]["operations"][0]["patch"]["metadata"];
    assert!(cleared.get("aq:role").is_none(), "{cleared}");
    assert_eq!(cleared["example:weight"], 2);

    // Once an attempt has been granted the role is fixed.
    state(r#"{"status":"granted","grant":{}}"#).await;
    x.put(node(task, x.workspace(), "task", 7));
    for role in [json!("reviewer"), Value::Null] {
        let (status, body) = x.act(&role_set(&x, task, "7", role)).await;
        assert_eq!(body["data"]["disposition"], "rejected", "{status} {body}");
    }
    // A finished attempt was granted too.
    state(r#"{"status":"finished","grant":null}"#).await;
    let (_, body) = x.act(&role_set(&x, task, "7", json!("reviewer"))).await;
    assert_eq!(body["data"]["disposition"], "rejected", "{body}");
    assert_eq!(x.posts().len(), 2, "a refused change never reaches Graph");
}

/// Spec 0017 §5.4: `task.create` and `task.graph.create` take `needs`, stored
/// as `aq:needs`; an empty list stores nothing and a malformed one is refused.
#[cfg(feature = "project_environments")]
#[tokio::test]
async fn filed_tasks_name_their_needs() {
    let x = setup().await;
    let single = Uuid::new_v4();
    x.reply(
        200,
        change_set(vec![node(single, x.workspace(), "task", 1)]),
    );
    let named = json!({"needs":["node","playwright"]});
    let (status, body) = x.act(&single_create(&x, single, named)).await;
    assert_eq!(body["data"]["disposition"], "confirmed", "{status} {body}");
    let metadata = &x.posts()[0]["operations"][0]["metadata"];
    assert_eq!(metadata["aq:needs"], json!(["node", "playwright"]));
    for bad in [json!(["node", "node"]), json!(["c++"]), json!([""])] {
        let (status, body) = x
            .act(&single_create(&x, Uuid::new_v4(), json!({"needs":bad})))
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}: {body}");
    }
    let ids: Vec<Uuid> = (0..2).map(|_| Uuid::new_v4()).collect();
    let tasks = json!([{"id":ids[0],"title":"Task","needs":["rust"]},{"id":ids[1],"title":"Task","needs":[]}]);
    let payload = json!({"operation_id":Uuid::new_v4(),"project_id":x.project(),"expected_version":"1","tasks":tasks});
    let created = ids.iter().map(|id| node(*id, x.workspace(), "task", 1));
    x.reply(200, change_set(created.collect()));
    let (status, body) = x
        .act(&action(
            "task.graph.create",
            Subject::Project(x.project(), "1"),
            payload,
        ))
        .await;
    assert_eq!(body["data"]["disposition"], "confirmed", "{status} {body}");
    let operations = x.posts()[1]["operations"].clone();
    assert_eq!(operations[0]["metadata"]["aq:needs"], json!(["rust"]));
    assert!(operations[1]["metadata"].get("aq:needs").is_none());
    assert_eq!(x.posts().len(), 2, "a refused create never reaches Graph");
}

/// An ungranted task's needs can be set or cleared. A live grant fences edits.
#[cfg(feature = "project_environments")]
#[tokio::test]
async fn task_needs_set_sets_and_clears_needs_and_is_refused_after_a_grant() {
    let x = setup().await;
    let pool = x.f.db.pool().await;
    let task = x.f.selection.lineage.task_id;
    let set = |version: &str, needs: Value| {
        x.route_action("task.needs.set", (task, version), json!({"needs":needs}))
    };
    x.put(node(task, x.workspace(), "task", 5));
    x.reply(200, change_set(vec![node(task, x.workspace(), "task", 6)]));
    let (status, body) = x.act(&set("5", json!(["node"]))).await;
    assert_eq!(body["data"]["disposition"], "confirmed", "{status} {body}");
    let metadata = x.posts()[0]["operations"][0]["patch"]["metadata"].clone();
    assert_eq!(metadata["aq:needs"], json!(["node"]));
    assert_eq!(metadata["example:weight"], 2);
    let (status, body) = x.act(&set("5", json!(["node", "node"]))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // An empty list clears the key.
    let mut named = node(task, x.workspace(), "task", 6);
    named["metadata"] = metadata;
    x.put(named);
    x.reply(200, change_set(vec![node(task, x.workspace(), "task", 7)]));
    let (status, body) = x.act(&set("6", json!([]))).await;
    assert_eq!(body["data"]["disposition"], "confirmed", "{status} {body}");
    let cleared = &x.posts()[1]["operations"][0]["patch"]["metadata"];
    assert!(cleared.get("aq:needs").is_none(), "{cleared}");
    assert_eq!(cleared["example:weight"], 2);

    // A live grant still fixes this attempt’s needs.
    let granted = r#"UPDATE execution_attempts SET state=state||'{"status":"granted","grant":{}}'::jsonb WHERE task_id=$1"#;
    sqlx::query(granted)
        .bind(task)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM profiled_authority_hot WHERE attempt_id IN (SELECT attempt_id FROM execution_attempts WHERE task_id=$1)")
        .bind(task).execute(&pool).await.unwrap();
    x.put(node(task, x.workspace(), "task", 7));
    for needs in [json!(["rust"]), Value::Null] {
        let (status, body) = x.act(&set("7", needs)).await;
        assert_eq!(body["data"]["disposition"], "rejected", "{status} {body}");
    }
    assert_eq!(x.posts().len(), 2, "a refused change never reaches Graph");
}
