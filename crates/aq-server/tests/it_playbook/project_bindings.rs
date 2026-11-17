//! AQ-19 public-owner routes, fake authoritative peers and explicit real-peer gate.
use crate::common;
use aq_server::AppState;
use axum::http::StatusCode;
use common::selection::*;
use serde_json::{Value, json};
use std::sync::{Arc, atomic::Ordering};
use uuid::Uuid;

#[tokio::test]
async fn missing_intelligence_registry_refuses_new_selection_and_offer_fence_explicitly() {
    use aq_server::intelligence::resolver::{self, ResolveRequest};
    let f = selection_fixture().await;
    let request = || ResolveRequest {
        operation_id: Uuid::new_v4(),
        lineage: f.lineage.clone(),
        project: f.frozen.clone(),
        launches: vec![common::roles::prepared_launch()],
        replacement: None,
        record_refused: true,
        route: None,
    };
    let decision = resolver::resolve_trusted(&f.env.state, f.actor, request(), &f.snapshot)
        .await
        .unwrap();
    sqlx::query("DELETE FROM intelligence_registry")
        .execute(&f.env.pool)
        .await
        .unwrap();
    let error = resolver::resolve_trusted(&f.env.state, f.actor, request(), &f.snapshot)
        .await
        .unwrap_err();
    assert!(
        matches!(error, aq_server::error::AppError::Conflict(ref reason) if reason.contains("registry has not been installed")),
        "{error:?}"
    );
    let mut tx = f.env.pool.begin().await.unwrap();
    sqlx::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
        .execute(&mut *tx)
        .await
        .unwrap();
    let error = resolver::fence_for_offer(&f.env.state, &mut tx, &decision)
        .await
        .unwrap_err();
    assert!(
        matches!(error, aq_server::error::AppError::Conflict(ref reason) if reason.contains("registry has not been installed")),
        "{error:?}"
    );
}
/// 1,024 retained decisions: unlinked and recent (pending, hence live), or linked to
/// finished runs (settled).
async fn fill_decisions(f: &SelectionFixture, live: bool) {
    use crate::common::sealed;
    let now = (f.env.state.clock)().timestamp_millis();
    let (node, project) = (Uuid::new_v4(), f.lineage.project_id);
    let mut tx = sealed::replica(&f.env.pool).await;
    for _ in 0..1024 {
        if live {
            sealed::decision(&mut tx, now - 1_000).await;
        } else {
            let attempt = sealed::attempt(&mut tx, false, Some("finished")).await;
            let decision = sealed::decision(&mut tx, now - 86_400_000).await;
            sealed::offer(&mut tx, attempt, decision, node, project, now - 86_400_000).await;
        }
    }
    tx.commit().await.unwrap();
}
async fn resolve_once(f: &SelectionFixture) -> Result<(), aq_server::error::AppError> {
    use aq_server::intelligence::resolver::{self, ResolveRequest};
    let request = ResolveRequest {
        operation_id: Uuid::new_v4(),
        lineage: f.lineage.clone(),
        project: f.frozen.clone(),
        launches: vec![common::roles::prepared_launch()],
        replacement: None,
        record_refused: true,
        route: None,
    };
    resolver::resolve_trusted(&f.env.state, f.actor, request, &f.snapshot)
        .await
        .map(|_| ())
}
#[tokio::test]
async fn live_decisions_at_1024_block_a_new_selection() {
    let f = selection_fixture().await;
    fill_decisions(&f, true).await;
    let error = resolve_once(&f).await.unwrap_err();
    assert!(
        matches!(error, aq_server::error::AppError::Conflict(ref r) if r.contains("retention limit")),
        "{error:?}"
    );
}
#[tokio::test]
async fn settled_decisions_at_1024_do_not_block_a_new_selection() {
    let f = selection_fixture().await;
    fill_decisions(&f, false).await;
    resolve_once(&f).await.unwrap();
}
#[tokio::test]
async fn revisioned_binding_replay_history_and_retained_snapshot_survive_updates() {
    let env = setup(false).await;
    let operation = Uuid::new_v4();
    let request = json!({"operation_id":operation,"expected_revision":0,"binding":env.body});
    let (status, first) = call(&env.app, &path(), Some(request.clone()), "owner@localhost").await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let frozen = call(
        &env.app,
        &format!("{}/eligibility", path()),
        Some(admission(&env)),
        "owner@localhost",
    )
    .await;
    assert_eq!(frozen.0, StatusCode::OK);
    assert_eq!(frozen.1["eligible"], true, "{}", frozen.1);
    assert_eq!(
        call(&env.app, &path(), Some(request.clone()), "owner@localhost")
            .await
            .1,
        first
    );
    let mut changed = request.clone();
    changed["binding"]["name"] = "Changed".into();
    assert_eq!(
        call(&env.app, &path(), Some(changed), "owner@localhost")
            .await
            .0,
        StatusCode::CONFLICT
    );
    let mut update = request;
    update["operation_id"] = json!(Uuid::new_v4());
    update["expected_revision"] = 1.into();
    update["binding"]["ceilings"]["context_bytes"] = 4096.into();
    let updated = call(&env.app, &path(), Some(update.clone()), "owner@localhost").await;
    assert_eq!(updated.0, StatusCode::OK);
    assert_eq!(updated.1["record"]["revision"], 2);
    update["operation_id"] = json!(Uuid::new_v4());
    assert_eq!(
        call(&env.app, &path(), Some(update), "owner@localhost")
            .await
            .0,
        StatusCode::CONFLICT
    );
    let old = call(
        &env.app,
        &format!("{}/revisions/1", path()),
        None,
        "owner@localhost",
    )
    .await;
    assert_eq!(old.1["binding"]["ceilings"]["context_bytes"], 8192);
    assert_eq!(frozen.1["frozen_binding"]["project"]["revision"], 1);
    let stale = call(
        &env.app,
        &format!("{}/eligibility", path()),
        Some(admission(&env)),
        "owner@localhost",
    )
    .await;
    assert_eq!(stale.1["eligible"], false);
    let stop = json!({"operation_id":Uuid::new_v4(),"expected_revision":2});
    let stop_path = format!("{}/disable", path());
    let stopped = call(&env.app, &stop_path, Some(stop.clone()), "owner@localhost").await;
    assert_eq!(stopped.0, StatusCode::OK, "{}", stopped.1);
    assert_eq!(stopped.1["record"]["binding"]["enabled"], false);
    assert_eq!(
        call(&env.app, &stop_path, Some(stop), "owner@localhost")
            .await
            .1,
        stopped.1
    );
    let mut binding = stopped.1["record"]["binding"].clone();
    binding["enabled"] = json!(true);
    let start = json!({"operation_id":Uuid::new_v4(),"expected_revision":3,"binding":binding});
    let started = call(&env.app, &path(), Some(start.clone()), "owner@localhost").await;
    assert_eq!(started.0, StatusCode::OK, "{}", started.1);
    assert_eq!(
        started.1["record"]["binding"], binding,
        "Start preserves all other project settings"
    );
    assert_eq!(started.1["record"]["revision"], 4);
    assert_eq!(
        call(&env.app, &path(), Some(start), "owner@localhost")
            .await
            .1,
        started.1
    );
    let listed = call(&env.app, "/api/projects", None, "owner@localhost").await;
    assert_eq!(
        listed.1["execution"],
        json!({}),
        "Missing worker policy is explicit rather than inferred from enabled binding"
    );
}
/// Spec 0014 §5.4: a binding names its default role, its default intelligence
/// profile and optional notes per role; each must exist, and what a binding
/// names cannot be deleted.
#[tokio::test]
async fn binding_role_defaults_are_checked_and_protect_what_they_name() {
    use aq_profiles::roles::Kind;
    let env = setup(false).await;
    let owner: Uuid = sqlx::query_scalar("SELECT id FROM users WHERE role='owner'")
        .fetch_one(&env.pool)
        .await
        .unwrap();
    // A binding saved without naming its defaults gets them, and the stored
    // record always names them.
    let bound = bind(&env).await;
    let stored = &bound["record"]["binding"];
    assert_eq!(stored["default_role"], "developer", "{stored}");
    assert_eq!(stored["default_intelligence"], "standard-high", "{stored}");
    assert!(stored.get("role_notes").is_none(), "{stored}");
    let row: Value = sqlx::query_scalar("SELECT binding FROM project_execution_bindings")
        .fetch_one(&env.pool)
        .await
        .unwrap();
    assert_eq!(row["default_role"], "developer");
    assert_eq!(row["default_intelligence"], "standard-high");
    assert_eq!(
        call(&env.app, &path(), None, "owner@localhost").await.0,
        200
    );

    let put = |revision: u64, change: Value| {
        let mut binding = env.body.clone();
        (binding.as_object_mut().unwrap()).extend(change.as_object().unwrap().clone());
        let request =
            json!({"operation_id":Uuid::new_v4(),"expected_revision":revision,"binding":binding});
        let app = env.app.clone();
        async move { call(&app, &path(), Some(request), "owner@localhost").await }
    };
    for (change, message) in [
        (
            json!({"default_role":"tester"}),
            "default_role: Role `tester` does not exist. The roles are: developer, reviewer.",
        ),
        (
            json!({"default_intelligence":"enormous"}),
            "default_intelligence: Intelligence profile `enormous` does not exist. The intelligence profiles are: complex-high, complex-low, deep-high, deep-low, simple-high, simple-low, standard-high, standard-low, standard-medium.",
        ),
        (
            json!({"role_notes":{"tester":"Run the slow suite."}}),
            "role_notes: Role `tester` does not exist. The roles are: developer, reviewer.",
        ),
    ] {
        let (status, body) = put(1, change).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["message"], message);
    }
    // A note is bounded by the role store's document byte limit, a Setting.
    let mut limits = aq_role_settings::StoreLimits::default();
    limits.documents.max_document_bytes = 4096;
    aq_role_settings::save_limits(&env.pool, owner, &limits, 0, Uuid::new_v4(), 1)
        .await
        .unwrap();
    let (status, body) = put(1, json!({"role_notes":{"reviewer":"n".repeat(4097)}})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        body["message"],
        "role_notes: the note for role `reviewer` is 4097 bytes, over the limit of 4096 bytes"
    );
    let named = json!({"default_role":"reviewer","default_intelligence":"deep-high",
        "role_notes":{"developer":"n".repeat(4096)}});
    let (status, body) = put(1, named.clone()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let saved = &body["record"]["binding"];
    assert_eq!(saved["default_role"], "reviewer");
    assert_eq!(saved["default_intelligence"], "deep-high");
    assert_eq!(saved["role_notes"], named["role_notes"]);
    let (_, read) = call(&env.app, &path(), None, "owner@localhost").await;
    assert_eq!(read["binding"]["default_role"], "reviewer", "{read}");

    // The document store refuses to delete what the binding names, naming
    // the project; a document nothing names is deleted.
    let remove = |segment: &'static str, kind: Kind, id: &'static str| {
        let (app, pool) = (env.app.clone(), env.pool.clone());
        async move {
            use http_body_util::BodyExt;
            use tower::ServiceExt;
            let stored = aq_role_settings::get(&pool, kind, id).await.unwrap();
            let uri = format!(
                "/api/settings/{segment}/{id}?expected_revision={}&operation_id={}",
                stored.unwrap().revision,
                Uuid::new_v4()
            );
            let request = axum::http::Request::delete(uri)
                .header("X-AQ-Request", "1")
                .header("x-aq-dev-email", "owner@localhost")
                .body(axum::body::Body::empty())
                .unwrap();
            let response = app.oneshot(request).await.unwrap();
            let status = response.status();
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            (status, serde_json::from_slice::<Value>(&bytes).unwrap())
        }
    };
    let name = env.body["name"].as_str().unwrap();
    for (segment, kind, id, problem) in [
        (
            "roles",
            Kind::Role,
            "reviewer",
            format!("role `reviewer`: is the default role of project `{name}`"),
        ),
        (
            "roles",
            Kind::Role,
            "developer",
            format!("role `developer`: is given notes by project `{name}`"),
        ),
        (
            "intelligence-profiles",
            Kind::Intelligence,
            "deep-high",
            format!(
                "intelligence profile `deep-high`: is the default intelligence profile of project `{name}`"
            ),
        ),
    ] {
        let (status, body) = remove(segment, kind, id).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        let message = body["message"].as_str().unwrap();
        assert!(message.contains(&problem), "{message}");
        assert!(
            aq_role_settings::get(&env.pool, kind, id)
                .await
                .unwrap()
                .is_some()
        );
    }
    let (status, body) = remove("intelligence-profiles", Kind::Intelligence, "simple-low").await;
    assert_eq!(status, StatusCode::OK, "{body}");
}
#[tokio::test]
async fn absent_stale_forbidden_and_offline_components_refuse_and_disable_stays_available() {
    let env = setup(false).await;
    bind(&env).await;
    for (git, graph) in [(1, 0), (0, 1), (0, 2), (0, 3), (2, 0)] {
        env.git_status.store(git, Ordering::SeqCst);
        env.graph_status.store(graph, Ordering::SeqCst);
        let out = call(
            &env.app,
            &format!("{}/eligibility", path()),
            Some(admission(&env)),
            "owner@localhost",
        )
        .await;
        assert_eq!(out.0, StatusCode::OK);
        assert_eq!(out.1["eligible"], false);
        assert!(out.1["frozen_binding"].is_null());
    }
    env.git_status.store(0, Ordering::SeqCst);
    env.graph_status.store(0, Ordering::SeqCst);
    env.generation.store(2, Ordering::SeqCst);
    let stale = call(
        &env.app,
        &format!("{}/eligibility", path()),
        Some(admission(&env)),
        "owner@localhost",
    )
    .await;
    assert!(
        stale.1["reasons"]
            .as_array()
            .unwrap()
            .contains(&json!("repository_stale"))
    );
    env.graph_status.store(3, Ordering::SeqCst);
    let disabled = call(
        &env.app,
        &format!("{}/disable", path()),
        Some(json!({"operation_id":Uuid::new_v4(),"expected_revision":1})),
        "owner@localhost",
    )
    .await;
    assert_eq!(disabled.0, StatusCode::OK);
    assert_eq!(disabled.1["record"]["binding"]["enabled"], false);
}
#[tokio::test]
async fn owner_scope_node_restrictions_and_mutation_unknown_fields_are_enforced() {
    let env = setup(false).await;
    let request = json!({"operation_id":Uuid::new_v4(),"expected_revision":0,"binding":env.body});
    assert_eq!(
        call(&env.app, &path(), Some(request.clone()), "viewer@localhost")
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let mut forged = request.clone();
    forged["binding"]["graph_url"] = "http://attacker".into();
    assert_eq!(
        call(&env.app, &path(), Some(forged), "owner@localhost")
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let mut unknown = request.clone();
    unknown["binding"]["allowed_worker_groups"] = json!(["worker-self-asserted"]);
    assert_eq!(
        call(&env.app, &path(), Some(unknown), "owner@localhost")
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    bind(&env).await;
    let mut denied = admission(&env);
    denied["environment"] = "unconfined".into();
    assert_eq!(
        call(
            &env.app,
            &format!("{}/eligibility", path()),
            Some(denied),
            "owner@localhost"
        )
        .await
        .1["eligible"],
        false
    );
    env.time.fetch_add(120_000, Ordering::SeqCst);
    assert_eq!(
        call(
            &env.app,
            &format!("{}/eligibility", path()),
            Some(admission(&env)),
            "owner@localhost"
        )
        .await
        .1["eligible"],
        false
    );
    assert_eq!(
        call(&env.app, "/api/projects?limit=1", None, "owner@localhost")
            .await
            .1["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}
#[tokio::test]
#[ignore = "requires immutable selected aq-git and aq-graph components via scripts/test-project-bindings.py"]
async fn selected_real_public_repository_and_graph_workspace_bind() {
    let env = setup(true).await;
    let result = bind(&env).await;
    assert_eq!(
        result["record"]["binding"]["repository_binding_generation"],
        1
    );
    let decision = call(
        &env.app,
        &format!("{}/eligibility", path()),
        Some(admission(&env)),
        "owner@localhost",
    )
    .await;
    assert_eq!(decision.0, StatusCode::OK);
    assert_eq!(decision.1["eligible"], true, "{}", decision.1);
    let mut stale = env.body.clone();
    stale["repository_binding_generation"] = 2.into();
    let refused = call(
        &env.app,
        &path(),
        Some(json!({"operation_id":Uuid::new_v4(),"expected_revision":1,"binding":stale})),
        "owner@localhost",
    )
    .await;
    assert_eq!(refused.0, StatusCode::CONFLICT);
    let unchanged = call(&env.app, &path(), None, "owner@localhost").await;
    assert_eq!(unchanged.1["revision"], 1);
    println!(
        "qualified public repo.show expected-generation binding + graph API1 workspace read, CAS stale refusal; no task lifecycle or Git-private-state writes"
    );
}
async fn wait_for_blocked(pool: &sqlx::PgPool, fragment: &str) {
    let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let count:i64=sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND query LIKE $1").bind(format!("%{fragment}%")).fetch_one(pool).await.unwrap();
        if count > 0 {
            return;
        }
        assert!(
            std::time::Instant::now() < end,
            "request did not reach intended lock wait"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}
#[tokio::test]
async fn observation_expiring_during_mutation_lock_wait_never_commits_fresh_validation() {
    let env = setup(false).await;
    let mut blocker = env.pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('aq-project-binding-write-v1',0))")
        .execute(&mut *blocker)
        .await
        .unwrap();
    let app = env.app.clone();
    let request = json!({"operation_id":Uuid::new_v4(),"expected_revision":0,"binding":env.body});
    let task =
        tokio::spawn(async move { call(&app, &path(), Some(request), "owner@localhost").await });
    wait_for_blocked(&env.pool, "aq-project-binding-write-v1").await;
    env.time.fetch_add(30_001, Ordering::SeqCst);
    blocker.commit().await.unwrap();
    let response = task.await.unwrap();
    assert_eq!(response.0, StatusCode::CONFLICT, "{}", response.1);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM project_execution_bindings")
        .fetch_one(&env.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}
#[tokio::test]
async fn final_local_fence_rechecks_node_revocation_after_lock_wait() {
    let env = setup(false).await;
    bind(&env).await;
    let mut blocker = env.pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(env.node)
        .execute(&mut *blocker)
        .await
        .unwrap();
    let app = env.app.clone();
    let r = admission(&env);
    let task = tokio::spawn(async move {
        call(
            &app,
            &format!("{}/eligibility", path()),
            Some(r),
            "owner@localhost",
        )
        .await
    });
    wait_for_blocked(&env.pool, "SELECT revoked_at,last_seen_at FROM nodes").await;
    sqlx::query("UPDATE nodes SET revoked_at=now() WHERE id=$1")
        .bind(env.node)
        .execute(&mut *blocker)
        .await
        .unwrap();
    blocker.commit().await.unwrap();
    let response = task.await.unwrap();
    assert_eq!(response.0, StatusCode::OK);
    assert_eq!(response.1["eligible"], false);
    assert!(response.1["frozen_binding"].is_null());
}
/// A launch names the active role snapshot: after a document is saved, a
/// request that still names the earlier snapshot is refused, and one that
/// names the new snapshot is eligible on the same project revision.
#[tokio::test]
async fn a_saved_role_document_refuses_a_launch_that_names_the_earlier_snapshot() {
    use aq_profiles::roles::Kind;
    use common::roles;
    let mut env = setup(false).await;
    bind(&env).await;
    let eligibility = format!("{}/eligibility", path());
    let earlier = admission(&env);
    let old = call(
        &env.app,
        &eligibility,
        Some(earlier.clone()),
        "owner@localhost",
    )
    .await
    .1;
    assert_eq!(old["eligible"], true, "{old}");
    let owner: Uuid = sqlx::query_scalar("SELECT id FROM users WHERE role='owner'")
        .fetch_one(&env.pool)
        .await
        .unwrap();
    let level = roles::level("standard-high", &[("fixture-a", "model-b", "low")]);
    roles::save(
        &env.pool,
        owner,
        Kind::Intelligence,
        "standard-high",
        &level,
    )
    .await;
    let decision = call(&env.app, &eligibility, Some(earlier), "owner@localhost")
        .await
        .1;
    assert_eq!(decision["eligible"], false);
    assert_eq!(decision["reasons"], json!(["role_snapshot_changed"]));
    assert!(decision["frozen_binding"].is_null());
    env.snapshot = roles::active(&env.pool).await;
    let current = call(
        &env.app,
        &eligibility,
        Some(admission(&env)),
        "owner@localhost",
    )
    .await
    .1;
    assert_eq!(current["eligible"], true, "{current}");
    assert_eq!(current["frozen_binding"]["project"]["revision"], 1);
    assert_eq!(
        current["frozen_binding"]["snapshot_digest"],
        env.snapshot.digest()
    );
    assert_ne!(
        old["frozen_binding"]["snapshot_digest"],
        current["frozen_binding"]["snapshot_digest"]
    );
}

#[tokio::test]
async fn exact_replay_survives_group_configuration_changes_but_new_mutations_do_not() {
    let env = setup(false).await;
    let request = json!({"operation_id":Uuid::new_v4(),"expected_revision":0,"binding":env.body});
    let first = call(&env.app, &path(), Some(request.clone()), "owner@localhost").await;
    assert_eq!(first.0, StatusCode::OK);
    let mut state = env.state.clone();
    let mut config = (*state.config).clone();
    config.projects.worker_groups.clear();
    state.config = Arc::new(config);
    let reconfigured = aq_server::build_admin_router(state);
    let replay = call(
        &reconfigured,
        &path(),
        Some(request.clone()),
        "owner@localhost",
    )
    .await;
    assert_eq!(replay.0, StatusCode::OK);
    assert_eq!(replay.1, first.1);
    let mut changed = request.clone();
    changed["binding"]["name"] = "Changed".into();
    assert_eq!(
        call(&reconfigured, &path(), Some(changed), "owner@localhost")
            .await
            .0,
        StatusCode::CONFLICT
    );
    let mut new = request;
    new["operation_id"] = json!(Uuid::new_v4());
    new["expected_revision"] = 1.into();
    assert_eq!(
        call(&reconfigured, &path(), Some(new), "owner@localhost")
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(
            &reconfigured,
            &path(),
            Some(json!({"operation_id":Uuid::new_v4(),"expected_revision":1,"binding":env.body})),
            "viewer@localhost"
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}

// AQ-20 composes the already-qualified project seam with synthetic selection
// facts. No provider or worker execution is started by this test.
#[tokio::test]
async fn a_republished_identical_inventory_keeps_a_decision_and_a_changed_one_refuses_it() {
    use aq_server::intelligence::resolver::{self, ResolveRequest};
    let SelectionFixture {
        env,
        snapshot,
        frozen,
        actor,
        lineage,
        operation,
        ..
    } = selection_fixture().await;
    let req = ResolveRequest {
        operation_id: operation,
        lineage,
        project: frozen,
        launches: vec![common::roles::prepared_launch()],
        replacement: None,
        record_refused: true,
        route: None,
    };
    let decision = resolver::resolve_trusted(&env.state, actor, req, &snapshot)
        .await
        .unwrap();
    assert!(decision.outcome.resolved.is_some());
    let fence = || async {
        let mut tx = env.pool.begin().await.unwrap();
        sqlx::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
            .execute(&mut *tx)
            .await
            .unwrap();
        let result = resolver::fence_for_offer(&env.state, &mut tx, &decision).await;
        tx.rollback().await.unwrap();
        result.map_err(|e| format!("{e:?}"))
    };
    let store = |snapshot: serde_json::Value, revision: u64| {
        sqlx::query("UPDATE intelligence_node_snapshots SET sequence=sequence+1,revision=$2,snapshot=$3 WHERE node_id=$1")
            .bind(env.node)
            .bind(revision as i64)
            .bind(snapshot)
            .execute(&env.pool)
    };
    // A sibling slot republishes the same installation: later sequence and
    // revision, a later validity window, nothing else.
    let mut same = decision.inputs.node.clone();
    same.revision += 1;
    same.observed_at_ms += 1;
    same.expires_at_ms += 1;
    for binding in &mut same.credential_bindings {
        binding.observed_at_ms += 1;
        binding.expires_at_ms += 1;
    }
    store(serde_json::json!(same), same.revision).await.unwrap();
    assert_eq!(fence().await, Ok(()));
    // The same content under an older revision is not the report that carries it.
    let mut older = same.clone();
    older.revision = decision.inputs.node.revision - 1;
    store(serde_json::json!(older), same.revision)
        .await
        .unwrap();
    let changed = Err(r#"Conflict("installed capability snapshot changed")"#.to_string());
    assert_eq!(fence().await, changed);
    // A changed installation refuses the decision, whatever its revision.
    let mut other = same.clone();
    other.harnesses[0].cli_version.push_str("-changed");
    store(serde_json::json!(other), other.revision)
        .await
        .unwrap();
    assert_eq!(fence().await, changed);
    let mut removed = same.clone();
    removed.credential_bindings.clear();
    store(serde_json::json!(removed), removed.revision)
        .await
        .unwrap();
    assert_eq!(fence().await, changed);
}

#[tokio::test]
async fn intelligence_decision_is_immutable_and_final_fence_rechecks_scoped_facts() {
    use aq_server::intelligence::resolver::{self, ResolveRequest};
    let SelectionFixture {
        env,
        snapshot,
        frozen,
        actor,
        lineage,
        manual,
        unrelated,
        time,
        operation,
        ..
    } = selection_fixture().await;
    let req = ResolveRequest {
        operation_id: operation,
        lineage: lineage.clone(),
        project: frozen.clone(),
        launches: vec![common::roles::prepared_launch()],
        replacement: None,
        record_refused: true,
        route: None,
    };
    let decision = resolver::resolve_trusted(&env.state, actor, req, &snapshot)
        .await
        .unwrap();
    assert!(
        decision.outcome.resolved.is_some(),
        "{:?}",
        decision.outcome.reason_codes
    );
    let mut tx = env.pool.begin().await.unwrap();
    sqlx::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
        .execute(&mut *tx)
        .await
        .unwrap();
    resolver::fence_for_offer(&env.state, &mut tx, &decision)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    let mut tx = env.pool.begin().await.unwrap();
    assert!(
        resolver::fence_for_offer(&env.state, &mut tx, &decision)
            .await
            .is_err()
    );
    tx.rollback().await.unwrap();
    let replay = resolver::resolve_trusted(
        &env.state,
        actor,
        ResolveRequest {
            operation_id: operation,
            lineage: lineage.clone(),
            project: frozen.clone(),
            launches: vec![common::roles::prepared_launch()],
            replacement: None,
            record_refused: true,
            route: None,
        },
        &snapshot,
    )
    .await
    .unwrap();
    assert_eq!(replay.decision_id, decision.decision_id);
    // Expiry is checked after an observed lock wait, not only before entry.
    let mut held = env.pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(env.node)
        .execute(&mut *held)
        .await
        .unwrap();
    let (pid_tx, pid_rx) = tokio::sync::oneshot::channel();
    let pool = env.pool.clone();
    let state = env.state.clone();
    let frozen_decision = decision.clone();
    let waiting = tokio::spawn(async move {
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
            .execute(&mut *tx)
            .await
            .unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        pid_tx.send(pid).unwrap();
        let result = resolver::fence_for_offer(&state, &mut tx, &frozen_decision).await;
        tx.rollback().await.unwrap();
        result
    });
    let pid = pid_rx.await.unwrap();
    let observer = env._db.pool().await;
    let mut observed = false;
    for _ in 0..100 {
        let blocked: bool = sqlx::query_scalar(
            "SELECT COALESCE(wait_event_type='Lock',false) FROM pg_stat_activity WHERE pid=$1",
        )
        .bind(pid)
        .fetch_one(&observer)
        .await
        .unwrap();
        if blocked {
            observed = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    assert!(
        observed,
        "fence must be observed waiting on the retained node lock"
    );
    let expires = decision.outcome.resolved.as_ref().unwrap().expires_at_ms;
    env.time.store(expires as i64, Ordering::SeqCst);
    held.rollback().await.unwrap();
    let error = waiting.await.unwrap().unwrap_err();
    assert!(format!("{error:?}").contains("expired"));
    env.time.store(time as i64, Ordering::SeqCst);
    // Other-account observations do not poison this decision's scope fence.
    let unrelated_observation = json!({"operation_id":Uuid::new_v4(),"expected_revision":2,"sequence":1,"scope":{"credential_revision":1,"provider":unrelated.provider,"credential_profile":unrelated.profile,"account_reference":unrelated.account_reference,"endpoint_reference":unrelated.endpoint_reference,"model":null,"node_id":null},"state":"quota_unavailable","ttl_ms":30_000,"cooldown_ms":30_000});
    assert_eq!(
        call(
            &env.app,
            "/api/intelligence/observations",
            Some(unrelated_observation),
            "owner@localhost"
        )
        .await
        .0,
        StatusCode::OK
    );
    let mut tx = env.pool.begin().await.unwrap();
    sqlx::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
        .execute(&mut *tx)
        .await
        .unwrap();
    resolver::fence_for_offer(&env.state, &mut tx, &decision)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    // An applicable change does, even while the original artifact stays intact.
    let mut changed = manual;
    changed["operation_id"] = json!(Uuid::new_v4());
    changed["expected_revision"] = 3.into();
    changed["sequence"] = 2.into();
    changed["state"] = "quota_unavailable".into();
    assert_eq!(
        call(
            &env.app,
            "/api/intelligence/observations",
            Some(changed),
            "owner@localhost"
        )
        .await
        .0,
        StatusCode::OK
    );
    let mut tx = env.pool.begin().await.unwrap();
    sqlx::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
        .execute(&mut *tx)
        .await
        .unwrap();
    assert!(
        resolver::fence_for_offer(&env.state, &mut tx, &decision)
            .await
            .is_err()
    );
    tx.rollback().await.unwrap();
    let mut tampered = decision.clone();
    tampered.inputs.budget.spent_microunits = Some(0);
    let mut tx = env.pool.begin().await.unwrap();
    assert!(
        resolver::fence_for_offer(&env.state, &mut tx, &tampered)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn project_policy_decisions_have_no_human_actor_and_recheck_installed_policy() {
    use aq_server::{
        intelligence::resolver::{self, ResolveRequest},
        profiled_execution::config::{ProfiledConfig, ProjectPolicy},
    };
    let SelectionFixture {
        mut env,
        snapshot,
        frozen,
        actor,
        lineage,
        operation,
        ..
    } = selection_fixture().await;
    let request = || ResolveRequest {
        operation_id: operation,
        lineage: lineage.clone(),
        project: frozen.clone(),
        launches: vec![common::roles::prepared_launch()],
        replacement: None,
        record_refused: true,
        route: None,
    };
    assert!(
        resolver::resolve_for_project_policy(&env.state, request(), &snapshot)
            .await
            .is_err()
    );
    let policy = ProfiledConfig {
        definition_bodies: vec![],
        revision: 1,
        qualified_runtimes: vec![],
        projects: vec![ProjectPolicy {
            project_id: lineage.project_id,
            enabled: true,
            priority: 0,
            source: aq_admission::SourceSelection::Immutable {
                object: aq_admission::ObjectId {
                    format: aq_admission::ObjectFormat::Sha1,
                    hex: "1".repeat(40),
                },
            },
            retention_handle: Uuid::new_v4(),
            placement: None,
            agent_grants: None,
        }],
    };
    let record = policy.project_policy_record(lineage.project_id).unwrap();
    let reference = record.artifact().unwrap();
    let mut config = (*env.state.config).clone();
    config.profiled_execution = Some(Arc::new(policy.clone()));
    env.state.config = Arc::new(config);
    assert!(
        resolver::resolve_for_project_policy(&env.state, request(), &snapshot)
            .await
            .is_err(),
        "configured alone is not installed policy"
    );
    let installed = aq_server::profiled_execution::policies::install(&env.state)
        .await
        .unwrap();
    assert_eq!(installed.installed, 1);
    let human = resolver::resolve_trusted(&env.state, actor, request(), &snapshot)
        .await
        .unwrap();
    let decision = resolver::resolve_for_project_policy(&env.state, request(), &snapshot)
        .await
        .unwrap();
    assert!(decision.outcome.resolved.is_some());
    assert_ne!(human.decision_id, decision.decision_id);
    let row:(Option<Uuid>,String,Option<Uuid>,Option<Value>,Option<i64>)=sqlx::query_as("SELECT actor_id,authority_kind,authority_project_id,authority_policy_ref,authority_policy_revision FROM intelligence_decisions WHERE decision_id=$1").bind(decision.decision_id).fetch_one(&env.pool).await.unwrap();
    assert_eq!(
        row,
        (
            None,
            "project_policy".into(),
            Some(lineage.project_id),
            Some(json!(reference)),
            Some(1)
        )
    );
    let replay = resolver::resolve_for_project_policy(&env.state, request(), &snapshot)
        .await
        .unwrap();
    assert_eq!(replay.decision_id, decision.decision_id);
    let mut changed = request();
    changed.launches.clear();
    assert!(
        resolver::resolve_for_project_policy(&env.state, changed, &snapshot)
            .await
            .is_err()
    );
    let fence = |state: AppState, decision: resolver::Decision| {
        let pool = env.pool.clone();
        async move {
            let mut tx = pool.begin().await.unwrap();
            sqlx::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
                .execute(&mut *tx)
                .await
                .unwrap();
            let result = resolver::fence_for_offer(&state, &mut tx, &decision).await;
            tx.rollback().await.unwrap();
            result
        }
    };
    fence(env.state.clone(), decision.clone()).await.unwrap();
    // The policy lock is acquired after the selection/project fence; expiry
    // still has to be checked after this last authorization wait.
    let mut held = env.pool.begin().await.unwrap();
    sqlx::query("SELECT project_id FROM admission_source_policies WHERE project_id=$1 FOR UPDATE")
        .bind(lineage.project_id)
        .execute(&mut *held)
        .await
        .unwrap();
    let (pid_tx, pid_rx) = tokio::sync::oneshot::channel();
    let pool = env.pool.clone();
    let state = env.state.clone();
    let captured = decision.clone();
    let waiting = tokio::spawn(async move {
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
            .execute(&mut *tx)
            .await
            .unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        pid_tx.send(pid).unwrap();
        let result = resolver::fence_for_offer(&state, &mut tx, &captured).await;
        tx.rollback().await.unwrap();
        result
    });
    let pid = pid_rx.await.unwrap();
    let observer = env._db.pool().await;
    let mut blocked = false;
    for _ in 0..100 {
        blocked = sqlx::query_scalar(
            "SELECT COALESCE(wait_event_type='Lock',false) FROM pg_stat_activity WHERE pid=$1",
        )
        .bind(pid)
        .fetch_one(&observer)
        .await
        .unwrap();
        if blocked {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    assert!(
        blocked,
        "policy fence must actually wait for the locked policy"
    );
    let current_time = env.time.load(Ordering::SeqCst);
    let expires = decision.outcome.resolved.as_ref().unwrap().expires_at_ms;
    env.time.store(expires as i64, Ordering::SeqCst);
    held.rollback().await.unwrap();
    assert!(format!("{:?}", waiting.await.unwrap().unwrap_err()).contains("expired"));
    env.time.store(current_time, Ordering::SeqCst);
    // Current human-role revocation affects the human namespace only.
    sqlx::query("UPDATE users SET role='viewer' WHERE id=$1")
        .bind(actor)
        .execute(&env.pool)
        .await
        .unwrap();
    assert!(
        resolver::resolve_trusted(&env.state, actor, request(), &snapshot)
            .await
            .is_err()
    );
    assert!(fence(env.state.clone(), human.clone()).await.is_err());
    fence(env.state.clone(), decision.clone()).await.unwrap();
    // A future installed policy defeats both stale daemon state and old decisions.
    let mut newer = policy.clone();
    newer.revision = 2;
    newer.projects[0].enabled = false;
    let mut future = env.state.clone();
    let mut config = (*future.config).clone();
    config.profiled_execution = Some(Arc::new(newer.clone()));
    future.config = Arc::new(config);
    assert_eq!(
        aq_server::profiled_execution::policies::install(&future)
            .await
            .unwrap()
            .disabled,
        1
    );
    assert!(
        resolver::resolve_for_project_policy(&env.state, request(), &snapshot)
            .await
            .is_err()
    );
    assert!(fence(env.state.clone(), decision.clone()).await.is_err());
    let mut config = (*env.state.config).clone();
    config.profiled_execution = Some(Arc::new(newer));
    env.state.config = Arc::new(config);
    assert!(
        resolver::resolve_for_project_policy(&env.state, request(), &snapshot)
            .await
            .is_err()
    );
    let retained: Value =
        sqlx::query_scalar("SELECT decision FROM intelligence_decisions WHERE decision_id=$1")
            .bind(decision.decision_id)
            .fetch_one(&env.pool)
            .await
            .unwrap();
    assert_eq!(retained, json!(decision));
    assert!(sqlx::query("UPDATE intelligence_decisions SET authority_kind='human_owner',actor_id=$2 WHERE decision_id=$1").bind(decision.decision_id).bind(actor).execute(&env.pool).await.is_err());
}

#[tokio::test]
async fn policy_install_is_atomic_retains_tombstones_and_refuses_configuration_rollback() {
    use aq_server::profiled_execution::{
        config::{ProfiledConfig, ProjectPolicy},
        policies,
    };
    let SelectionFixture {
        mut env, lineage, ..
    } = selection_fixture().await;
    let source = aq_admission::SourceSelection::Immutable {
        object: aq_admission::ObjectId {
            format: aq_admission::ObjectFormat::Sha1,
            hex: "a".repeat(40),
        },
    };
    let configured = ProfiledConfig {
        definition_bodies: vec![],
        revision: 1,
        qualified_runtimes: vec![],
        projects: vec![
            ProjectPolicy {
                project_id: lineage.project_id,
                enabled: true,
                priority: 0,
                source: source.clone(),
                retention_handle: Uuid::new_v4(),
                placement: None,
                agent_grants: None,
            },
            ProjectPolicy {
                project_id: Uuid::new_v4(),
                enabled: true,
                priority: 0,
                source,
                retention_handle: Uuid::new_v4(),
                placement: None,
                agent_grants: None,
            },
        ],
    };
    let mut config = (*env.state.config).clone();
    config.profiled_execution = Some(Arc::new(configured.clone()));
    env.state.config = Arc::new(config);
    let first = policies::install(&env.state).await.unwrap();
    assert_eq!((first.installed, first.disabled, first.unbound), (1, 0, 1));
    let retry = policies::install(&env.state).await.unwrap();
    assert_eq!((retry.installed, retry.disabled, retry.unbound), (0, 0, 1));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM admission_source_policies")
        .fetch_one(&env.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    let newer = ProfiledConfig {
        definition_bodies: vec![],
        revision: 2,
        qualified_runtimes: vec![],
        projects: vec![],
    };
    let mut future = env.state.clone();
    let mut config = (*future.config).clone();
    config.profiled_execution = Some(Arc::new(newer));
    future.config = Arc::new(config);
    sqlx::raw_sql("CREATE FUNCTION test_policy_failure() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'synthetic policy write failure'; END $$; CREATE TRIGGER test_policy_failure BEFORE UPDATE ON admission_source_policies FOR EACH ROW EXECUTE FUNCTION test_policy_failure();").execute(&env.pool).await.unwrap();
    assert!(policies::install(&future).await.is_err());
    let meta: String =
        sqlx::query_scalar("SELECT value FROM meta WHERE key='profiled_policy_configuration_v1'")
            .fetch_one(&env.pool)
            .await
            .unwrap();
    assert_eq!(serde_json::from_str::<Value>(&meta).unwrap()["revision"], 1);
    let enabled: bool = sqlx::query_scalar(
        "SELECT (policy->>'enabled')::boolean FROM admission_source_policies WHERE project_id=$1",
    )
    .bind(lineage.project_id)
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert!(enabled);
    sqlx::query("DROP TRIGGER test_policy_failure ON admission_source_policies")
        .execute(&env.pool)
        .await
        .unwrap();
    assert_eq!(policies::install(&future).await.unwrap().disabled, 1);
    let enabled: bool = sqlx::query_scalar(
        "SELECT (policy->>'enabled')::boolean FROM admission_source_policies WHERE project_id=$1",
    )
    .bind(lineage.project_id)
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert!(!enabled);
    assert!(
        policies::install(&env.state).await.is_err(),
        "older daemon cannot reinstall removed projects"
    );
    let mut changed = configured;
    changed.revision = 2;
    let mut config = (*env.state.config).clone();
    config.profiled_execution = Some(Arc::new(changed));
    env.state.config = Arc::new(config);
    assert!(
        policies::install(&env.state).await.is_err(),
        "same revision changed content refuses"
    );
    assert_eq!(policies::install(&future).await.unwrap().disabled, 0);
}
#[tokio::test]
async fn empty_policy_configuration_still_retains_global_revision_owner() {
    use aq_server::profiled_execution::{config::ProfiledConfig, policies};
    let mut env = setup(false).await;
    let mut config = (*env.state.config).clone();
    config.profiled_execution = Some(Arc::new(ProfiledConfig {
        definition_bodies: vec![],
        revision: 10,
        qualified_runtimes: vec![],
        projects: vec![],
    }));
    env.state.config = Arc::new(config);
    assert_eq!(
        policies::install(&env.state).await.unwrap().revision,
        Some(10)
    );
    let mut config = (*env.state.config).clone();
    config.profiled_execution = Some(Arc::new(ProfiledConfig {
        definition_bodies: vec![],
        revision: 9,
        qualified_runtimes: vec![],
        projects: vec![],
    }));
    env.state.config = Arc::new(config);
    assert!(policies::install(&env.state).await.is_err());
}

#[tokio::test]
async fn committed_binding_installs_pending_policy_and_historical_ack_survives_retirement() {
    use aq_server::profiled_execution::{
        config::{ProfiledConfig, ProjectPolicy},
        policies,
    };
    let mut env = setup(false).await;
    let configured = ProfiledConfig {
        definition_bodies: vec![],
        revision: 1,
        qualified_runtimes: vec![],
        projects: vec![ProjectPolicy {
            project_id: Uuid::from_u128(1),
            enabled: true,
            priority: 0,
            source: aq_admission::SourceSelection::Immutable {
                object: aq_admission::ObjectId {
                    format: aq_admission::ObjectFormat::Sha1,
                    hex: "a".repeat(40),
                },
            },
            retention_handle: Uuid::new_v4(),
            placement: None,
            agent_grants: None,
        }],
    };
    let mut cfg = (*env.state.config).clone();
    cfg.profiled_execution = Some(Arc::new(configured));
    env.state.config = Arc::new(cfg);
    env.app = aq_server::build_admin_router(env.state.clone());
    assert_eq!(policies::install(&env.state).await.unwrap().unbound, 1);
    let request = json!({"operation_id":Uuid::new_v4(),"expected_revision":0,"binding":env.body});
    let (status, first) = call(&env.app, &path(), Some(request.clone()), "owner@localhost").await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let enabled: bool = sqlx::query_scalar(
        "SELECT (policy->>'enabled')::boolean FROM admission_source_policies WHERE project_id=$1",
    )
    .bind(Uuid::from_u128(1))
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert!(
        enabled,
        "post-commit installer did not publish the previously unbound policy"
    );
    let mut newer = env.state.clone();
    let mut cfg = (*newer.config).clone();
    cfg.profiled_execution = Some(Arc::new(ProfiledConfig {
        definition_bodies: vec![],
        revision: 2,
        qualified_runtimes: vec![],
        projects: vec![],
    }));
    newer.config = Arc::new(cfg);
    policies::install(&newer).await.unwrap();
    let (status, retry) = call(&env.app, &path(), Some(request), "owner@localhost").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(retry, first);
    let enabled: bool = sqlx::query_scalar(
        "SELECT (policy->>'enabled')::boolean FROM admission_source_policies WHERE project_id=$1",
    )
    .bind(Uuid::from_u128(1))
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert!(!enabled, "historical ack reinstalled a retired policy");
}

/// Spec 0014 §5.4: the catalog digest and the three allowlists are gone from
/// the binding. A save that still sends one is refused and the answer names
/// the field; a binding with only the remaining fields saves and reads back.
#[tokio::test]
async fn a_binding_that_names_a_removed_catalog_field_is_refused_by_name() {
    let env = setup(false).await;
    for (field, value) in [
        ("catalog_digest", json!("a".repeat(64))),
        ("allowed_agent_profiles", json!(["implementer@1"])),
        ("allowed_intelligence_profiles", json!(["engineering@1"])),
        (
            "allowed_execution_profiles",
            json!(["confined-development@1"]),
        ),
    ] {
        let mut binding = env.body.clone();
        binding[field] = value;
        let request =
            json!({"operation_id":Uuid::new_v4(),"expected_revision":0,"binding":binding});
        let (status, body) = call(&env.app, &path(), Some(request), "owner@localhost").await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{field}: {body}");
        let text = body.to_string();
        assert!(
            text.contains(&format!("unknown field `{field}`")),
            "{field}: {text}"
        );
    }
    for field in [
        "catalog_digest",
        "allowed_agent_profiles",
        "allowed_intelligence_profiles",
        "allowed_execution_profiles",
    ] {
        assert!(env.body.get(field).is_none(), "{field}");
    }
    let saved = bind(&env).await;
    assert_eq!(saved["record"]["revision"], 1, "{saved}");
    let (status, read) = call(&env.app, &path(), None, "owner@localhost").await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(read["binding"], saved["record"]["binding"], "{read}");
    assert_eq!(read["binding"]["default_role"], "developer");
}
