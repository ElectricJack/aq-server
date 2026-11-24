use super::*;

#[tokio::test]
async fn routes_require_the_owner_and_the_request_header() {
    let f = started().await;
    let put = json!({"markdown": developer("x"), "expected_revision": "1", "operation_id": Uuid::new_v4()});
    for path in [
        "/api/settings/roles",
        "/api/settings/roles/developer",
        "/api/settings/intelligence-profiles",
        "/api/settings/execution-profiles/sandbox",
        "/api/settings/selection-defaults",
        "/api/settings/profiles/limits",
        "/api/settings/profiles/snapshot",
    ] {
        let (status, body) = f.viewer().get(path).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path} {body}");
        let (status, body) = f.owner().get(path).await;
        assert_eq!(status, StatusCode::OK, "{path} {body}");
    }
    let before = f.active().await;
    let one = "/api/settings/roles/developer";
    let (status, body) = f.viewer().put(one, put.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body) = f.owner().put(one, put).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"], "csrf_rejected");
    let delete = format!(
        "/api/settings/intelligence-profiles/deep-high?expected_revision=1&operation_id={}",
        Uuid::new_v4()
    );
    let (status, _) = f.viewer().delete(&delete).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = f.owner().delete(&delete).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(f.active().await, before);
}

#[tokio::test]
async fn an_owner_reads_saves_and_deletes_documents_over_http() {
    let f = started().await;
    let (revision, sandbox) = check_shipped_documents(&f).await;
    let saved = create_and_edit_tester(&f, revision).await;
    change_selection_defaults(&f, &saved).await;
    delete_tester(&f, &saved, &sandbox).await;
}

const TESTER_PATH: &str = "/api/settings/roles/tester";

async fn check_shipped_documents(f: &Fixture) -> (i64, Value) {
    let (revision, digest) = f.active().await;
    let (status, list) = f.owner().get("/api/settings/roles").await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list["revision"], revision.to_string());
    assert_eq!(list["active_digest"], digest);
    let documents = list["documents"].as_array().unwrap();
    assert_eq!(documents.len(), 2);
    assert_eq!(documents[0]["id"], "developer");
    assert_eq!(documents[0]["markdown"], shipped(Kind::Role, "developer"));
    assert_eq!(documents[0]["fields"]["execution"], "sandbox");
    assert_eq!(
        documents[0]["fields"]["operations"],
        json!(["task.read", "knowledge.read", "source.write"])
    );
    assert_eq!(documents[0]["problem"], Value::Null);
    let (_, level) = f
        .owner()
        .get("/api/settings/intelligence-profiles/standard-high")
        .await;
    assert_eq!(
        level["fields"]["candidates"][0]["model"],
        "claude-sonnet-5-5"
    );
    let (_, sandbox) = f
        .owner()
        .get("/api/settings/execution-profiles/sandbox")
        .await;
    assert_eq!(sandbox["fields"]["confinement"], "container");
    let (status, _) = f.owner().get("/api/settings/roles/tester").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    (revision, sandbox)
}

async fn create_and_edit_tester(f: &Fixture, revision: i64) -> Value {
    // Create a role.
    let tester = shipped(Kind::Role, "reviewer")
        .replace("id: reviewer", "id: tester")
        .replace("name: Reviewer", "name: Tester");
    let operation = Uuid::new_v4();
    let create = json!({"markdown": tester, "expected_revision": "0", "operation_id": operation});
    let path = TESTER_PATH;
    let (status, saved) = f.owner_write().put(path, create.clone()).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["revision"], (revision + 1).to_string());
    let (_, replayed) = f.owner_write().put(path, create).await;
    assert_eq!(replayed, saved);
    let (_, snapshot) = f.owner().get("/api/settings/profiles/snapshot").await;
    assert_eq!(
        snapshot,
        json!({"revision": saved["revision"], "active_digest": saved["digest"]})
    );
    let (_, read) = f.owner().get(path).await;
    assert_eq!(read["revision"], saved["revision"]);
    assert_eq!(read["fields"]["name"], "Tester");

    // An invalid document: 400 with every problem; a stale revision: 409.
    let broken = tester
        .replace("execution: sandbox", "execution: nowhere")
        .replace("task.read", "task.fly");
    let body = json!({"markdown": broken, "expected_revision": saved["revision"], "operation_id": Uuid::new_v4()});
    let (status, refused) = f.owner_write().put(path, body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert_eq!(refused["error"], "bad_request");
    assert_eq!(
        refused["problems"],
        json!([
            {"document": "role `tester`", "field": "execution", "message": "no execution profile `nowhere` exists"},
            {"document": "role `tester`", "field": "operations", "message": "`task.fly` is not an AQ operation a role can hold"},
        ])
    );
    let stale =
        json!({"markdown": tester, "expected_revision": "0", "operation_id": Uuid::new_v4()});
    let (status, body) = f.owner_write().put(path, stale).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (status, _) = f.owner_write().put(path, json!({"markdown": tester})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    saved
}

async fn change_selection_defaults(f: &Fixture, saved: &Value) {
    // Selection defaults.
    let (_, defaults) = f.owner().get("/api/settings/selection-defaults").await;
    assert_eq!(defaults["revision"], "0");
    assert_eq!(defaults["selection_defaults"]["max_attempts"], 2);
    let mut changed = defaults["selection_defaults"].clone();
    changed["max_attempts"] = json!(3);
    let body = json!({"selection_defaults": changed, "expected_revision": "0", "operation_id": Uuid::new_v4()});
    let (status, moved) = f
        .owner_write()
        .put("/api/settings/selection-defaults", body)
        .await;
    assert_eq!(status, StatusCode::OK, "{moved}");
    assert_ne!(moved["digest"], saved["digest"]);
    // A save keeps every key it does not state: a client that does not know the newer keys
    // (the dashboard form) cannot reset them. An explicit value still changes one, an
    // explicit `null` still clears a cost limit, and an unknown key is refused.
    let revision = || async {
        let read = f.owner().get("/api/settings/selection-defaults");
        read.await.1["revision"].as_str().unwrap().to_owned()
    };
    let put = |body: Value, revision: String| {
        let body = json!({"selection_defaults": body, "expected_revision": revision, "operation_id": Uuid::new_v4()});
        let path = "/api/settings/selection-defaults";
        f.owner_write().put(path, body)
    };
    let stated = json!({"max_uncharged_continuations": 7, "max_never_granted_offers": 3, "max_cost_microunits": 5, "currency": "USD"});
    let (status, body) = put(stated, revision().await).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let partial = json!({"max_attempts": 4, "max_cost_microunits": null, "currency": null});
    let (status, body) = put(partial, revision().await).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, read) = f.owner().get("/api/settings/selection-defaults").await;
    let now = &read["selection_defaults"];
    assert_eq!(now["max_attempts"], 4);
    assert_eq!(now["max_uncharged_continuations"], 7, "kept: {now}");
    assert_eq!(now["max_never_granted_offers"], 3, "kept: {now}");
    assert_eq!(now["max_wake_attempts"], 3, "never stated: {now}");
    assert_eq!(now["max_cost_microunits"], Value::Null, "cleared: {now}");
    let (status, _) = put(json!({"no_such_key": 1}), revision().await).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // The lineage time budgets are gone: a save that states one is refused as unknown.
    for key in ["max_lineage_elapsed_ms", "max_lineage_sandbox_ms"] {
        let (status, _) = put(json!({ key: 3_600_000 }), revision().await).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{key}");
    }
    let (status, _) = put(json!({"max_never_granted_offers": 0}), revision().await).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "the merged whole is validated"
    );
    let (_, limits) = f.owner().get("/api/settings/profiles/limits").await;
    assert_eq!(limits["limits"]["snapshots_kept"], 20);
}

async fn delete_tester(f: &Fixture, saved: &Value, sandbox: &Value) {
    let path = TESTER_PATH;
    // Delete: refused while referenced, done otherwise.
    let delete = |path: &str, revision: &Value| {
        format!(
            "{path}?expected_revision={}&operation_id={}",
            revision.as_str().unwrap(),
            Uuid::new_v4()
        )
    };
    let (status, refused) = f
        .owner_write()
        .delete(&delete(
            "/api/settings/execution-profiles/sandbox",
            &sandbox["revision"],
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert_eq!(refused["problems"].as_array().unwrap().len(), 3);
    let (status, gone) = f
        .owner_write()
        .delete(&delete(path, &saved["revision"]))
        .await;
    assert_eq!(status, StatusCode::OK, "{gone}");
    let (status, _) = f.owner().get(path).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = f.owner_write().delete("/api/settings/roles/tester").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // The old catalog's Settings and activation routes are gone.
    for path in [
        "/api/settings/intelligence",
        "/api/settings/intelligence/structure",
        "/api/settings/intelligence-extra",
        "/api/profiles/catalogs",
    ] {
        let (status, _) = f.owner().get(path).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }
}
