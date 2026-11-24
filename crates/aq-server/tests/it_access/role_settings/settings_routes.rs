use super::*;

#[tokio::test]
async fn the_routing_document_is_saved_replayed_and_fenced_over_http() {
    const PATH: &str = "/api/settings/routing-document";
    let f = started().await;
    let (status, body) = f.owner().get(PATH).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({"markdown": "", "revision": "0"}));
    let (status, _) = f.viewer_read().get(PATH).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a viewer does not read it");

    let put = json!({"markdown": "Route a one-line fix to simple-low.",
        "expected_revision": "0", "operation_id": Uuid::new_v4()});
    let (status, body) = f.viewer().put(PATH, put.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body) = f.owner().put(PATH, put.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"], "csrf_rejected");
    let before = f.active().await;
    let (status, saved) = f.owner_write().put(PATH, put.clone()).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    // Saving it compiles nothing: the same snapshot stays active.
    assert_eq!(saved["digest"], json!(before.1));
    assert_eq!(f.active().await.1, before.1);
    let (_, read) = f.owner().get(PATH).await;
    assert_eq!(
        read,
        json!({"markdown": "Route a one-line fix to simple-low.", "revision": saved["revision"]})
    );

    // The same operation again answers as the first time and saves nothing.
    let (status, again) = f.owner_write().put(PATH, put.clone()).await;
    assert_eq!((status, &again), (StatusCode::OK, &saved));
    // The operation id with another text is a conflict.
    let mut other = put.clone();
    other["markdown"] = json!("Something else.");
    let (status, body) = f.owner_write().put(PATH, other).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    // A save from a stale read is a conflict and changes nothing.
    let stale =
        json!({"markdown": "Late.", "expected_revision": "0", "operation_id": Uuid::new_v4()});
    let (status, body) = f.owner_write().put(PATH, stale).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(f.owner().get(PATH).await.1, read);
    // A next save names the revision it read.
    let next = json!({"markdown": "", "expected_revision": saved["revision"],
        "operation_id": Uuid::new_v4()});
    let (status, body) = f.owner_write().put(PATH, next).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // The store reads what the route saved (the authoring host reads this).
    let (markdown, revision) = settings::routing_document(&f.pool).await.unwrap();
    assert_eq!(
        (markdown.as_str(), revision.to_string()),
        ("", body["revision"].as_str().unwrap().to_owned())
    );
    // A document over the store's document limit is refused.
    let limit = StoreLimits::default().documents.max_document_bytes;
    let big = json!({"markdown": "x".repeat(limit + 1), "expected_revision": body["revision"],
        "operation_id": Uuid::new_v4()});
    let (status, body) = f.owner_write().put(PATH, big).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[cfg(feature = "delivery_playbook")]
#[tokio::test]
async fn the_delivery_document_is_saved_replayed_and_fenced_over_http() {
    const PATH: &str = "/api/settings/delivery-document";
    let f = started().await;
    // Empty by default: no delivery policy.
    let (status, body) = f.owner().get(PATH).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({"markdown": "", "revision": "0"}));
    let (status, _) = f.viewer_read().get(PATH).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a viewer does not read it");

    let put = json!({"markdown": "Merge when the checks pass.",
        "expected_revision": "0", "operation_id": Uuid::new_v4()});
    let (status, body) = f.viewer().put(PATH, put.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body) = f.owner().put(PATH, put.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"], "csrf_rejected");
    let before = f.active().await;
    let (status, saved) = f.owner_write().put(PATH, put.clone()).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    // Saving it compiles nothing: the same snapshot stays active.
    assert_eq!(saved["digest"], json!(before.1));
    assert_eq!(f.active().await.1, before.1);
    let (_, read) = f.owner().get(PATH).await;
    assert_eq!(
        read,
        json!({"markdown": "Merge when the checks pass.", "revision": saved["revision"]})
    );
    // The routing document is a separate place and is unchanged.
    let (_, routing) = f.owner().get("/api/settings/routing-document").await;
    assert_eq!(routing, json!({"markdown": "", "revision": "0"}));

    // The same operation again answers as the first time and saves nothing.
    let (status, again) = f.owner_write().put(PATH, put.clone()).await;
    assert_eq!((status, &again), (StatusCode::OK, &saved));
    // The operation id with another text is a conflict.
    let mut other = put.clone();
    other["markdown"] = json!("Something else.");
    let (status, body) = f.owner_write().put(PATH, other).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    // A save from a stale read is a conflict and changes nothing.
    let stale =
        json!({"markdown": "Late.", "expected_revision": "0", "operation_id": Uuid::new_v4()});
    let (status, body) = f.owner_write().put(PATH, stale).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(f.owner().get(PATH).await.1, read);
    // A next save names the revision it read.
    let next = json!({"markdown": "", "expected_revision": saved["revision"],
        "operation_id": Uuid::new_v4()});
    let (status, body) = f.owner_write().put(PATH, next).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (markdown, revision) = settings::delivery_document(&f.pool).await.unwrap();
    assert_eq!(
        (markdown.as_str(), revision.to_string()),
        ("", body["revision"].as_str().unwrap().to_owned())
    );
    // A document over the store's document limit is refused.
    let limit = StoreLimits::default().documents.max_document_bytes;
    let big = json!({"markdown": "x".repeat(limit + 1), "expected_revision": body["revision"],
        "operation_id": Uuid::new_v4()});
    let (status, body) = f.owner_write().put(PATH, big).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    // The body must be a markdown text and nothing else.
    let wrong = json!({"limits": {}, "expected_revision": "0", "operation_id": Uuid::new_v4()});
    let (status, _) = f.owner_write().put(PATH, wrong).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[cfg(feature = "delivery_playbook")]
#[tokio::test]
async fn the_delivery_limits_are_defaulted_validated_and_fenced_over_http() {
    use aq_role_settings::delivery::DeliveryLimits;
    const PATH: &str = "/api/settings/delivery-limits";
    let f = started().await;
    let defaults = json!({
        "main_sync_interval_ms": 60000, "main_freshness_ms": 30000,
        "decision_repeat_ms": 120000, "longest_check_wait_ms": 3600000,
        "git_transfer_timeout_ms": 600000, "max_push_bytes": 268435456,
        "require_landed_results": true, "check_log_bytes": 16384,
        "hold_followup_interval_ms": 86400000});
    let (status, body) = f.owner().get(PATH).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({"limits": defaults, "revision": "0"}));
    let (status, _) = f.viewer_read().get(PATH).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a viewer does not read them");

    let mut changed = defaults.clone();
    changed["main_sync_interval_ms"] = json!(15000);
    changed["hold_followup_interval_ms"] = json!(120000);
    let put = json!({"limits": changed, "expected_revision": "0",
        "operation_id": Uuid::new_v4()});
    let (status, body) = f.viewer().put(PATH, put.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body) = f.owner().put(PATH, put.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let before = f.active().await;
    let (status, saved) = f.owner_write().put(PATH, put.clone()).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(f.active().await.1, before.1, "nothing is compiled");
    let (_, read) = f.owner().get(PATH).await;
    assert_eq!(
        read,
        json!({"limits": changed, "revision": saved["revision"]})
    );
    let (limits, revision) = settings::delivery_limits(&f.pool).await.unwrap();
    assert_eq!(limits.main_sync_interval_ms, 15000);
    assert_eq!(limits.hold_followup_interval_ms, 120000);
    assert_eq!(revision.to_string(), saved["revision"].as_str().unwrap());
    // The store's own limits keep their revision.
    let (_, own) = f.owner().get("/api/settings/profiles/limits").await;
    assert_eq!(own["revision"], "0");

    // The same operation again answers as the first time.
    let (status, again) = f.owner_write().put(PATH, put.clone()).await;
    assert_eq!((status, &again), (StatusCode::OK, &saved));
    // A stale read is a conflict and changes nothing.
    let stale = json!({"limits": defaults, "expected_revision": "0",
        "operation_id": Uuid::new_v4()});
    let (status, body) = f.owner_write().put(PATH, stale).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(f.owner().get(PATH).await.1, read);

    // Invalid limits are refused and store nothing, whatever the fence says.
    let expected = saved["revision"].clone();
    for limits in invalid_delivery_limits(&defaults) {
        let put = json!({"limits": limits, "expected_revision": expected,
            "operation_id": Uuid::new_v4()});
        let (status, body) = f.owner_write().put(PATH, put).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{limits}: {body}");
    }
    assert_eq!(f.owner().get(PATH).await.1, read);
    // Markdown is not a limits body.
    let wrong = json!({"markdown": "x", "expected_revision": expected,
        "operation_id": Uuid::new_v4()});
    let (status, _) = f.owner_write().put(PATH, wrong).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(DeliveryLimits::default().validate(), Ok(()));
}

#[cfg(feature = "delivery_playbook")]
fn invalid_delivery_limits(defaults: &Value) -> Vec<Value> {
    let mut cases = Vec::new();
    for field in defaults.as_object().unwrap().keys() {
        let mut zero = defaults.clone();
        zero[field] = json!(0);
        cases.push(zero);
        let mut huge = defaults.clone();
        huge[field] = json!(u64::MAX);
        cases.push(huge);
        let mut negative = defaults.clone();
        negative[field] = json!(-1);
        cases.push(negative);
        // A field Settings stored before it existed reads as its default.
        if ![
            "require_landed_results",
            "check_log_bytes",
            "hold_followup_interval_ms",
        ]
        .contains(&field.as_str())
        {
            let mut missing = defaults.clone();
            missing.as_object_mut().unwrap().remove(field);
            cases.push(missing);
        }
    }
    let mut unknown = defaults.clone();
    unknown["surprise"] = json!(1);
    cases.push(unknown);
    cases
}

#[tokio::test]
async fn the_observation_limits_are_defaulted_validated_and_fenced_over_http() {
    const PATH: &str = "/api/settings/observation-limits";
    let f = started().await;
    // The defaults are the numbers that were constants before.
    let defaults = json!({
        "max_sources_per_node": 256, "max_events_per_node": 65536,
        "max_retained_bytes_per_node": 134217728, "max_calls_per_summary": 65536,
        "final_drain_grace_ms": 300000});
    let (status, body) = f.owner().get(PATH).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({"limits": defaults, "revision": "0"}));
    let (status, _) = f.viewer_read().get(PATH).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a viewer does not read them");

    let mut changed = defaults.clone();
    changed["max_sources_per_node"] = json!(1024);
    let put = json!({"limits": changed, "expected_revision": "0",
        "operation_id": Uuid::new_v4()});
    let (status, body) = f.viewer().put(PATH, put.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let before = f.active().await;
    let (status, saved) = f.owner_write().put(PATH, put.clone()).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(f.active().await.1, before.1, "nothing is compiled");
    let (_, read) = f.owner().get(PATH).await;
    assert_eq!(
        read,
        json!({"limits": changed, "revision": saved["revision"]})
    );
    // What a cap check reads on its own connection.
    let mut conn = f.pool.acquire().await.unwrap();
    let (limits, _) = settings::observation_limits(&mut conn).await.unwrap();
    assert_eq!(limits.max_sources_per_node, 1024);
    // The same operation again answers as the first time; a stale read is a
    // conflict; an invalid or unknown value is refused. Nothing changes.
    let (status, again) = f.owner_write().put(PATH, put).await;
    assert_eq!((status, &again), (StatusCode::OK, &saved));
    let stale = json!({"limits": defaults, "expected_revision": "0",
        "operation_id": Uuid::new_v4()});
    let (status, body) = f.owner_write().put(PATH, stale).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let mut cases = Vec::new();
    for field in defaults.as_object().unwrap().keys() {
        for bad in [json!(0), json!(u64::MAX), json!(-1)] {
            let mut limits = defaults.clone();
            limits[field] = bad;
            cases.push(limits);
        }
        let mut missing = defaults.clone();
        missing.as_object_mut().unwrap().remove(field);
        cases.push(missing);
    }
    let mut unknown = defaults.clone();
    unknown["surprise"] = json!(1);
    cases.push(unknown);
    for limits in cases {
        let put = json!({"limits": limits, "expected_revision": saved["revision"],
            "operation_id": Uuid::new_v4()});
        let (status, body) = f.owner_write().put(PATH, put).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{limits}: {body}");
    }
    assert_eq!(f.owner().get(PATH).await.1, read);
}

#[tokio::test]
async fn model_suggestions_are_read_from_the_discovery_cache_and_never_fail() {
    const PATH: &str = "/api/settings/model-suggestions";
    // No cache configured: nothing is suggested.
    let f = started().await;
    let (status, body) = f.owner().get(PATH).await;
    assert_eq!((status, &body), (StatusCode::OK, &json!({"harnesses": []})));
    let (status, _) = f.viewer_read().get(PATH).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let directory = tempfile::tempdir().unwrap();
    let cache = directory.path().join("models.json");
    let root = format!(
        "intelligence_preferences_file={:?}",
        cache.to_str().unwrap()
    );
    let f = started_with(&root).await;
    // Configured but absent, and malformed: still an empty answer.
    for text in [None, Some("not json")] {
        if let Some(text) = text {
            std::fs::write(&cache, text).unwrap();
        }
        let (status, body) = f.owner().get(PATH).await;
        assert_eq!((status, &body), (StatusCode::OK, &json!({"harnesses": []})));
    }
    std::fs::write(
        &cache,
        json!([
            {"provider":"openai","model":"gpt-x","harness":"codex","source":"Codex CLI","observed_at":"2026-11-16T00:00:00Z"},
            {"provider":"anthropic","model":"claude-y","harness":"claude"},
        ])
        .to_string(),
    )
    .unwrap();
    let (status, body) = f.owner().get(PATH).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({"harnesses": [
            {"harness":"claude","models":[{"provider":"anthropic","model":"claude-y","source":null,"observed_at":null}]},
            {"harness":"codex","models":[{"provider":"openai","model":"gpt-x","source":"Codex CLI","observed_at":"2026-11-16T00:00:00Z"}]},
        ]})
    );
    // A suggestion is not a gate: a level may name a model that is not listed.
    let level = "---\nid: standard-high\nname: Standard, high\ndescription: Ordinary work.\n---\n\
        ## Candidates\n| provider | model | reasoning | harnesses |\n|---|---|---|---|\n\
        | anthropic | a-model-nobody-suggested | high | claude |\n";
    f.save(Kind::Intelligence, "standard-high", level)
        .await
        .unwrap();
}
