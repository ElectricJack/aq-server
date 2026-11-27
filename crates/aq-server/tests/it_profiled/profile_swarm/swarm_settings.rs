use super::*;
const SWARM: &str = "/api/settings/swarm";
const MODE: &str = "/api/settings/swarm/mode";

/// The shipped levels are active: one cell per level, on their one provider.
async fn installed() -> (TestDb, Router, sqlx::PgPool, Value) {
    let (db, app, pool) = setup().await;
    let (status, snapshot) = call(&app, SWARM, None, false).await;
    assert_eq!(status, StatusCode::OK, "{snapshot}");
    assert!(!snapshot["cells"].as_array().unwrap().is_empty());
    (db, app, pool, snapshot)
}
/// Save a level document as the owner would in Settings.
async fn save_level(pool: &sqlx::PgPool, id: &str, name: &str, rows: &[(&str, &str, &str)]) {
    let actor: Uuid = sqlx::query_scalar("SELECT id FROM users WHERE role='owner'")
        .fetch_one(pool)
        .await
        .unwrap();
    let rows: String = (rows.iter())
        .map(|(provider, model, harness)| {
            format!("| {provider} | {model} | default | {harness} |\n")
        })
        .collect();
    let markdown = format!(
        "---\nid: {id}\nname: {name}\ndescription: A test level.\n---\n\
         ## Candidates\n| provider | model | reasoning | harnesses |\n|---|---|---|---|\n{rows}"
    );
    crate::common::roles::save(
        pool,
        actor,
        aq_profiles::roles::Kind::Intelligence,
        id,
        &markdown,
    )
    .await;
}
fn matrix(snapshot: &Value, limit: u32) -> Value {
    Value::Array(snapshot["cells"].as_array().unwrap().iter().map(|c| json!({
        "provider":c["provider"],"intelligence_class":c["intelligence_class"],"limit":limit
    })).collect())
}
fn save_request(snapshot: &Value) -> Value {
    json!({"operation_id":Uuid::new_v4(),"expected_revision":snapshot["revision"],
        "normal":matrix(snapshot,2),"reduced":matrix(snapshot,0)})
}

#[tokio::test]
async fn swarm_profiles_are_atomic_persistent_and_replayable() {
    let (_db, app, pool, initial) = installed().await;
    assert_eq!(initial["normal"], Value::Null);
    assert_eq!(initial["reduced"], Value::Null);
    assert_eq!(initial["active_mode"], "normal");
    assert!(
        initial["cells"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["effective_limit"].is_null())
    );
    let request = save_request(&initial);
    let (status, normal) = call(&app, SWARM, Some(request.clone()), true).await;
    assert_eq!(status, StatusCode::OK, "{normal}");
    assert_eq!(normal["revision"], "1");
    assert!(
        normal["cells"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["effective_limit"] == 2 && c["running"] == 0 && c["waiting"] == 0)
    );
    let mode =
        json!({"operation_id":Uuid::new_v4(),"expected_revision":"1","active_mode":"reduced"});
    let (status, reduced) = call(&app, MODE, Some(mode.clone()), true).await;
    assert_eq!(status, StatusCode::OK, "{reduced}");
    assert_eq!(reduced["revision"], "2");
    assert!(
        reduced["cells"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["effective_limit"] == 0)
    );
    let (status, replay) = call(&app, SWARM, Some(request.clone()), true).await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(
        replay, normal,
        "unknown-outcome retry returns the original receipt"
    );
    let path = format!(
        "{SWARM}/operations/{}",
        mode["operation_id"].as_str().unwrap()
    );
    let (status, receipt) = call(&app, &path, None, false).await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    assert_eq!(receipt, reduced);
    let persisted = aq_swarm_settings::read(&pool).await.unwrap();
    assert_eq!(
        persisted.settings.active_mode,
        aq_swarm_settings::Mode::Reduced
    );
    let (status, body) = call(&app, SWARM, Some(save_request(&initial)), true).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let mut different = request;
    different["normal"][0]["limit"] = json!(9);
    let (status, body) = call(&app, SWARM, Some(different), true).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (status, body) = call(
        &app,
        MODE,
        Some(json!({"operation_id":Uuid::new_v4(),"expected_revision":"2","active_mode":"normal"})),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body["cells"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["effective_limit"] == 2)
    );
}

#[tokio::test]
async fn swarm_rejects_incoherent_matrices_without_partial_writes() {
    let (_db, app, _pool, initial) = installed().await;
    let (status, body) = call(
        &app,
        MODE,
        Some(
            json!({"operation_id":Uuid::new_v4(),"expected_revision":"0","active_mode":"reduced"}),
        ),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let valid = save_request(&initial);
    let invalid_fields = [
        ("/normal", json!([])),
        ("/normal/0/limit", json!(-1)),
        ("/normal/0/limit", json!(1.5)),
        ("/normal/0/provider", json!("unsupported")),
        ("/reduced/0/limit", json!(3)),
    ];
    let mut cases: Vec<_> = invalid_fields
        .into_iter()
        .map(|(path, value)| {
            let mut request = valid.clone();
            *request.pointer_mut(path).unwrap() = value;
            request
        })
        .collect();
    let mut duplicate = valid.clone();
    let cell = duplicate["normal"][0].clone();
    duplicate["normal"].as_array_mut().unwrap().push(cell);
    cases.push(duplicate);
    for req in cases {
        let (status, body) = call(&app, SWARM, Some(req), true).await;
        assert!(
            status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY,
            "{status}: {body}"
        );
        assert_eq!(call(&app, SWARM, None, false).await.1, initial);
    }
    // The old catalog fence is gone, and an unknown field is refused.
    let mut fenced = valid.clone();
    fenced["expected_catalog_revision"] = json!("0");
    assert_eq!(
        call(&app, SWARM, Some(fenced), true).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(&app, SWARM, Some(valid), false).await.0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn swarm_concurrent_saves_have_one_winner_and_viewers_are_denied() {
    let (_db, app, pool, initial) = installed().await;
    let left = save_request(&initial);
    let right = save_request(&initial);
    let (left, right) = tokio::join!(
        call(&app, SWARM, Some(left), true),
        call(&app, SWARM, Some(right), true)
    );
    assert!(
        (left.0 == StatusCode::OK && right.0 == StatusCode::CONFLICT)
            || (right.0 == StatusCode::OK && left.0 == StatusCode::CONFLICT),
        "{left:?} {right:?}"
    );
    sqlx::query("UPDATE users SET role='viewer'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(&app, SWARM, None, false).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(&app, SWARM, Some(save_request(&initial)), true)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn swarm_failed_receipt_rolls_back_matrix_and_mode() {
    let (_db, app, pool, initial) = installed().await;
    sqlx::raw_sql("CREATE FUNCTION fail_swarm_receipt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'receipt unavailable'; END $$; CREATE TRIGGER fail_swarm_receipt BEFORE INSERT ON swarm_setting_operations FOR EACH ROW EXECUTE FUNCTION fail_swarm_receipt();").execute(&pool).await.unwrap();
    let mut limits = limits_request(&initial);
    limits["provider"] = json!({"provider":initial["cells"][0]["provider"],"limit":1});
    assert_eq!(
        call(&app, "/api/settings/swarm/limits", Some(limits), true)
            .await
            .0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(call(&app, SWARM, None, false).await.1, initial);
    let request = save_request(&initial);
    let (status, body) = call(&app, SWARM, Some(request.clone()), true).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert_eq!(call(&app, SWARM, None, false).await.1, initial);
    let path = format!(
        "{SWARM}/operations/{}",
        request["operation_id"].as_str().unwrap()
    );
    assert_eq!(
        call(&app, &path, None, false).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn a_changed_level_requires_explicit_new_capacity_and_preserves_saved_profiles() {
    let (_db, app, pool, initial) = installed().await;
    let (status, saved) = call(&app, SWARM, Some(save_request(&initial)), true).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    // A level gains a candidate on another provider.
    save_level(
        &pool,
        "standard-high",
        "Standard, high",
        &[
            ("anthropic", "claude-sonnet-5-5", "claude"),
            ("google", "gemini-test", "gemini"),
        ],
    )
    .await;
    let (status, drift) = call(&app, SWARM, None, false).await;
    assert_eq!(status, StatusCode::OK, "{drift}");
    assert_eq!(drift["normal"], saved["normal"]);
    assert!(
        drift["cells"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["provider"] == "google"
                && c["intelligence_class"] == "standard-high"
                && c["effective_limit"].is_null())
    );
    // A matrix read before the level changed no longer names every cell.
    let mut stale = save_request(&initial);
    stale["expected_revision"] = saved["revision"].clone();
    assert_eq!(
        call(&app, SWARM, Some(stale), true).await.0,
        StatusCode::BAD_REQUEST
    );
    let mode = json!({"operation_id":Uuid::new_v4(),"expected_revision":saved["revision"],"active_mode":"reduced"});
    assert_eq!(
        call(&app, MODE, Some(mode), true).await.0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn swarm_waiting_counts_keep_unclassified_work_visible() {
    let (_db, app, pool, initial) = installed().await;
    let cell = &initial["cells"][0];
    let now = Utc::now().timestamp_millis();
    for (provider, class) in [
        (
            cell["provider"].as_str().unwrap(),
            cell["intelligence_class"].as_str().unwrap(),
        ),
        ("anthropic", "__unclassified"),
    ] {
        sqlx::query("INSERT INTO swarm_waiting (task_id,provider,intelligence_class,reason,observed_at_ms,expires_at_ms) VALUES ($1,$2,$3,'swarm_concurrency_limit_reached',$4,$5)")
            .bind(Uuid::new_v4()).bind(provider).bind(class).bind(now).bind(now+60_000).execute(&pool).await.unwrap();
    }
    let (status, body) = call(&app, SWARM, None, false).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["cells"][0]["waiting"], 1);
    assert_eq!(
        body["unclassified"],
        json!([{"provider":"anthropic","running":0,"waiting":1}])
    );
}

fn named_cells(snapshot: &Value, class: &str) -> Vec<(String, String)> {
    (snapshot["cells"].as_array().unwrap().iter())
        .filter(|c| c["intelligence_class"] == class)
        .map(|c| {
            let text = |key: &str| c[key].as_str().unwrap().to_owned();
            (text("provider"), text("display_name"))
        })
        .collect()
}

#[tokio::test]
async fn swarm_cells_are_the_providers_of_each_level_document() {
    let (_db, app, pool, initial) = installed().await;
    // The shipped levels: nine documents, each on one provider.
    let cells = initial["cells"].as_array().unwrap();
    assert_eq!(cells.len(), 9, "{initial}");
    assert!(cells.iter().all(|c| c["provider"] == "anthropic"));
    assert!(initial.get("catalog_revision").is_none());
    assert_eq!(
        named_cells(&initial, "standard-high"),
        [("anthropic".to_owned(), "Standard, high".to_owned())]
    );
    // A new level document is a new class: one cell per provider it names,
    // shown under the document's name.
    save_level(
        &pool,
        "night-shift",
        "Night shift",
        &[
            ("anthropic", "claude-sonnet-5-5", "claude"),
            ("google", "gemini-test", "gemini"),
            ("google", "gemini-other", "gemini"),
        ],
    )
    .await;
    let snapshot = call(&app, SWARM, None, false).await.1;
    assert_eq!(snapshot["cells"].as_array().unwrap().len(), 11);
    assert_eq!(
        named_cells(&snapshot, "night-shift"),
        [
            ("anthropic".to_owned(), "Night shift".to_owned()),
            ("google".to_owned(), "Night shift".to_owned())
        ]
    );
    let mut request = save_request(&snapshot);
    request["reduced"] = Value::Null;
    let (status, saved) = call(&app, SWARM, Some(request), true).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["reduced"], Value::Null);
}

fn limits_request(snapshot: &Value) -> Value {
    json!({"operation_id":Uuid::new_v4(),"expected_revision":snapshot["revision"]})
}

#[tokio::test]
async fn individual_limits_preserve_unlimited_cells_and_legacy_profiles() {
    let (_db, app, pool, initial) = installed().await;
    let cell = &initial["cells"][0];
    let mut request = limits_request(&initial);
    request["provider"] = json!({"provider":cell["provider"],"limit":3});
    request["cell"] = json!({"provider":cell["provider"],"intelligence_class":cell["intelligence_class"],"limit":0});
    let path = "/api/settings/swarm/limits";
    let (status, saved) = call(&app, path, Some(request.clone()), true).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert!(saved["normal"].is_null());
    assert_eq!(saved["provider_limits"][0]["limit"], 3);
    assert_eq!(saved["cells"][0]["effective_limit"], 0);
    assert_eq!(call(&app, path, Some(request.clone()), true).await.1, saved);
    let selected = aq_swarm_settings::admission::Cell {
        provider: cell["provider"].as_str().unwrap().into(),
        intelligence_class: cell["intelligence_class"].as_str().unwrap().into(),
    };
    let mut tx = pool.begin().await.unwrap();
    assert_eq!(
        aq_swarm_settings::admission::check(&mut tx, Some(&selected))
            .await
            .unwrap(),
        Some(aq_swarm_settings::admission::CEILING)
    );
    let unrelated = aq_swarm_settings::admission::Cell {
        intelligence_class: "another-class".into(),
        ..selected.clone()
    };
    assert_eq!(
        aq_swarm_settings::admission::check(&mut tx, Some(&unrelated))
            .await
            .unwrap(),
        None
    );
    tx.rollback().await.unwrap();
    let mut clear = limits_request(&saved);
    clear["cell"] = json!({"provider":cell["provider"],"intelligence_class":cell["intelligence_class"],"limit":null});
    let (status, cleared) = call(&app, path, Some(clear), true).await;
    assert_eq!(status, StatusCode::OK, "{cleared}");
    assert!(cleared["cells"][0]["effective_limit"].is_null());
    assert_eq!(cleared["provider_limits"], saved["provider_limits"]);
    let (status, legacy) = call(&app, SWARM, Some(save_request(&cleared)), true).await;
    assert_eq!(status, StatusCode::OK, "{legacy}");
    assert!(
        legacy["cells"][0]["effective_limit"].is_null(),
        "explicit unlimited overrides preserved legacy matrix"
    );
    assert_eq!(legacy["normal"], matrix(&initial, 2));
    assert_eq!(
        call(&app, path, Some(limits_request(&legacy)), true)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(&app, path, Some(request), false).await.0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn individual_limits_are_atomic_and_revision_checked() {
    let (_db, app, pool, initial) = installed().await;
    let path = "/api/settings/swarm/limits";
    let mut request = limits_request(&initial);
    request["provider"] = json!({"provider":initial["cells"][0]["provider"],"limit":0});
    let mut invalid = request.clone();
    invalid["cell"] = json!({"provider":"unsupported","intelligence_class":"unknown","limit":1});
    assert_eq!(
        call(&app, path, Some(invalid), true).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(call(&app, SWARM, None, false).await.1, initial);
    let mut stale = request.clone();
    stale["expected_revision"] = json!("7");
    assert_eq!(
        call(&app, path, Some(stale), true).await.0,
        StatusCode::CONFLICT
    );
    let mut other = request.clone();
    other["operation_id"] = json!(Uuid::new_v4());
    let (a, b) = tokio::join!(
        call(&app, path, Some(request), true),
        call(&app, path, Some(other), true)
    );
    assert!(
        (a.0 == StatusCode::OK && b.0 == StatusCode::CONFLICT)
            || (b.0 == StatusCode::OK && a.0 == StatusCode::CONFLICT),
        "{a:?} {b:?}"
    );
    let selected = aq_swarm_settings::admission::Cell {
        provider: initial["cells"][0]["provider"].as_str().unwrap().into(),
        intelligence_class: "unconfigured-class".into(),
    };
    let mut tx = pool.begin().await.unwrap();
    assert_eq!(
        aq_swarm_settings::admission::check(&mut tx, Some(&selected))
            .await
            .unwrap(),
        Some(aq_swarm_settings::admission::CEILING)
    );
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn independent_limits_require_current_supervisor_admission_marker() {
    let (_db, _app, pool, _initial) = installed().await;
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("UPDATE swarm_settings SET limits=$1 WHERE singleton")
        .bind(json!({"provider_limits":[{"provider":"anthropic","limit":1}]}))
        .execute(&mut *tx)
        .await
        .unwrap();
    // Exercise the production generation trigger with an old host's marker.
    // Its generation FK is deferred, exactly as in a host prepare transaction.
    sqlx::raw_sql("CREATE TEMP TABLE guard_probe(actor_id UUID,generation BIGINT); CREATE TRIGGER probe BEFORE INSERT ON guard_probe FOR EACH ROW EXECUTE FUNCTION guard_supervisor_swarm_admission();")
        .execute(&mut *tx).await.unwrap();
    let actor = Uuid::new_v4();
    sqlx::query("INSERT INTO swarm_supervisor_admissions(actor_id,generation,provider,intelligence_class) VALUES($1,1,'anthropic','engineering')")
        .bind(actor).execute(&mut *tx).await.unwrap();
    sqlx::query("SAVEPOINT old_host")
        .execute(&mut *tx)
        .await
        .unwrap();
    let old = sqlx::query("INSERT INTO guard_probe VALUES($1,1)")
        .bind(actor)
        .execute(&mut *tx)
        .await;
    assert!(
        old.is_err(),
        "v115 host's marker cannot bypass independent ceilings"
    );
    sqlx::query("ROLLBACK TO SAVEPOINT old_host")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE swarm_supervisor_admissions SET admission_version=2 WHERE actor_id=$1")
        .bind(actor)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO guard_probe VALUES($1,1)")
        .bind(actor)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
}
