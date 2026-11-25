use super::*;

#[tokio::test]
async fn a_registered_source_is_authored_and_its_view_shows_the_cases() {
    let (_db, host, api) = api().await;
    assert_initial_registration(&api, &host).await;
    let task = latest_task(&host).await;
    authoring::completed(&host, task, &answered(saved_outcome()))
        .await
        .unwrap();
    let (status, view) = api.call("GET", &format!("{SOURCES}/{NOTIFY}"), None).await;
    assert_eq!(status, 200);
    assert_eq!(view["latest"]["status"], "saved");
    assert!(view["latest"]["finished_at_ms"].is_number());
    assert_active_cases(&view);
    let (_, list) = api.call("GET", SOURCES, None).await;
    assert_eq!(list["items"], json!([view]));

    // The owner changes the document: a new run, then a second playbook.
    let revision = view["revision"].as_u64().unwrap();
    let path = format!("{SOURCES}/{NOTIFY}/markdown");
    let body = json!({"markdown": reworded(2), "expected_revision": revision});
    let (status, view) = api.call("PUT", &path, Some(body.clone())).await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["latest"]["status"], "running");
    assert_eq!(view["revision"], revision + 1);
    // A save over a revision someone else replaced is refused.
    assert_eq!(api.call("PUT", &path, Some(body)).await.0, 409);
    let task = latest_task(&host).await;
    authoring::completed(&host, task, &answered(saved_outcome()))
        .await
        .unwrap();
    // Author again with nothing changed starts nothing and answers the view.
    let (status, view) = api
        .call("POST", &format!("{SOURCES}/{NOTIFY}/runs"), None)
        .await;
    assert_eq!((status, &view["latest"]["status"]), (200, &json!("saved")));
    let active = &view["active"];
    assert_eq!(
        (&active["can_rollback"], &active["comparison"]["change"]),
        (&json!(true), &json!("wording"))
    );
    let second = view["active"]["artifact"].clone();
    // Rollback answers the refreshed view; a second one has nothing to go back to.
    let rollback = format!("{SOURCES}/{NOTIFY}/rollback");
    let (status, view) = api.call("POST", &rollback, None).await;
    assert_eq!(status, 200, "{view}");
    assert_ne!(view["active"]["artifact"], second);
    assert_eq!(view["active"]["can_rollback"], false);
    let (status, refused) = api.call("POST", &rollback, None).await;
    assert_eq!(status, 409, "{refused}");
    assert_eq!(
        api.call("POST", &format!("{SOURCES}/nobody/rollback"), None)
            .await
            .0,
        404
    );
}

#[tokio::test]
async fn a_registration_names_commands_of_the_catalog_and_a_free_binding() {
    let (_db, host, api) = api().await;
    let mut unknown = registration("A document.\n");
    unknown["commands"] = json!(["work.delete"]);
    assert_eq!(api.call("POST", SOURCES, Some(unknown)).await.0, 400);
    let mut nameless = registration("A document.\n");
    nameless["id"] = json!("a/b");
    assert_eq!(api.call("POST", SOURCES, Some(nameless)).await.0, 400);
    let mut extra = registration("A document.\n");
    extra["capabilities"] = json!(["work.route"]);
    assert!(api.call("POST", SOURCES, Some(extra)).await.0 >= 400);
    assert_eq!(runs(&host.pool).await, 0);
    assert_eq!(
        api.call("POST", SOURCES, Some(registration("A document.\n")))
            .await
            .0,
        200
    );
    let mut taken = registration("Another.\n");
    taken["id"] = json!("other");
    assert_eq!(api.call("POST", SOURCES, Some(taken)).await.0, 409);
    // With no engine to deploy on, the source is refused in words.
    host.engine.lock().unwrap().down = true;
    let (status, refused) = api
        .call("POST", &format!("{SOURCES}/{NOTIFY}/runs"), None)
        .await;
    assert_eq!(status, 409);
    assert!(refused["message"].as_str().unwrap().contains("not running"));
}

#[tokio::test]
async fn put_markdown_on_a_builtin_source_is_refused() {
    let (_db, host, api) = api().await;
    // Before the server has written the built-in record there is no source.
    assert_eq!(
        api.call("GET", &format!("{SOURCES}/routing"), None).await.0,
        404
    );
    ensure_builtin(&host.pool, &routing()).await.unwrap();
    let (status, view) = api.call("GET", &format!("{SOURCES}/routing"), None).await;
    assert_eq!((status, &view["kind"]), (200, &json!("builtin")));
    assert_eq!(
        (&view["active"], &view["latest"]),
        (&Value::Null, &Value::Null)
    );
    let body = json!({"markdown": "Route everything to me.", "expected_revision": 1});
    let (status, refused) = api
        .call("PUT", &format!("{SOURCES}/routing/markdown"), Some(body))
        .await;
    assert_eq!(status, 409, "{refused}");
    assert_eq!(runs(&host.pool).await, 0);
}

#[tokio::test]
async fn deleting_a_registered_source_revokes_its_grant_and_stops_its_runs() {
    let (_db, host, api) = api().await;
    let pool = host.pool.clone();
    api.call("POST", SOURCES, Some(registration(example("source.md"))))
        .await;
    let task = latest_task(&host).await;
    authoring::completed(&host, task, &answered(saved_outcome()))
        .await
        .unwrap();
    let revision = source_view(&host).await["revision"].as_u64().unwrap();
    let body = json!({"markdown": reworded(2), "expected_revision": revision});
    api.call("PUT", &format!("{SOURCES}/{NOTIFY}/markdown"), Some(body))
        .await;
    let running = in_flight(&pool).await.unwrap()[0].id;

    let path = format!("{SOURCES}/{NOTIFY}");
    assert_eq!(
        api.call("DELETE", &path, None).await,
        (200, json!({"deleted": true}))
    );
    assert_eq!(host.engine.lock().unwrap().revoked, [NOTIFY]);
    assert_eq!(status(&pool, running).await.0, "superseded");
    assert!(active(&pool, NOTIFY).await.unwrap().is_none());
    assert_missing_source(&api, &path).await;
    // The run history stays, and a late result changes nothing.
    assert_eq!(runs(&pool).await, 2);
    assert_eq!(authoring::reinstall_active(&host).await.unwrap(), 0);
    // Registering the id again brings the source back.
    let (status, view) = api
        .call("POST", SOURCES, Some(registration(example("source.md"))))
        .await;
    assert_eq!(
        (status, &view["latest"]["status"]),
        (200, &json!("running"))
    );
}

#[tokio::test]
async fn the_routing_source_cannot_be_deleted() {
    let (_db, host, api) = api().await;
    ensure_builtin(&host.pool, &routing()).await.unwrap();
    let (status, refused) = api
        .call("DELETE", &format!("{SOURCES}/routing"), None)
        .await;
    assert_eq!(status, 409, "{refused}");
    assert!(host.engine.lock().unwrap().revoked.is_empty());
    assert_eq!(
        api.call("GET", &format!("{SOURCES}/routing"), None).await.0,
        200
    );
}

async fn assert_initial_registration(api: &Api, host: &Host) {
    assert_eq!(
        api.call("GET", SOURCES, None).await,
        (200, json!({"items": []}))
    );
    let (status, view) = api
        .call("POST", SOURCES, Some(registration(example("source.md"))))
        .await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(
        (&view["id"], &view["kind"], &view["binding"]),
        (&json!(NOTIFY), &json!("registered"), &json!(BINDING))
    );
    assert_running_source(&view);
    // What the playbook may do follows from its commands, and the grant
    // exists before the run starts.
    let record = authoring::get(&host.pool, NOTIFY).await.unwrap().unwrap();
    assert_eq!(record.capabilities, names(&["message.send", "work.read"]));
    assert_eq!(
        host.engine.lock().unwrap().grants[NOTIFY].1,
        record.capabilities
    );
}

fn assert_active_cases(view: &Value) {
    let active = &view["active"];
    assert!(active["artifact"].is_string() && active["since_ms"].as_i64().unwrap() > 0);
    assert_eq!(active["can_rollback"], false);
    assert_eq!(active["previous"], Value::Null);
    let cases = active["cases"].as_array().unwrap();
    assert_eq!(cases.len(), example_cases().len());
    for case in cases {
        for field in ["name", "situation", "result"] {
            assert!(case[field].is_string(), "{case}");
        }
        assert_eq!(case["passed"], true);
        owner_text(case["result"].as_str().unwrap());
    }
}

async fn assert_missing_source(api: &Api, path: &str) {
    assert_eq!(api.call("GET", path, None).await.0, 404);
    assert_eq!(api.call("GET", SOURCES, None).await.1, json!({"items": []}));
    assert_eq!(api.call("DELETE", path, None).await.0, 404);
}

fn assert_running_source(view: &Value) {
    assert_eq!(view["active"], Value::Null);
    assert_eq!(view["latest"]["status"], "running");
    for field in ["run", "internal_task_id"] {
        assert!(view["latest"][field].is_string(), "{field}: {view}");
    }
    assert!(view["latest"]["elapsed_ms"].is_number());
}
