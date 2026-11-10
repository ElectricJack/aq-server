//! Immutable catalog activation through the real owner router and PostgreSQL.
use aq_profiles::{ValidatedCatalog, load, validate};
use aq_server::{AppState, config::Config, identity::verifier_from_config, test_support::TestDb};
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use chrono::Utc;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::{path::Path, sync::Arc};
use tower::ServiceExt;
use uuid::Uuid;

fn seed() -> ValidatedCatalog {
    load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../aq-profiles/examples/catalog")).unwrap()
}
async fn setup() -> (TestDb, Router, sqlx::PgPool) {
    setup_with("").await
}
async fn setup_with(extra: &str) -> (TestDb, Router, sqlx::PgPool) {
    let db = TestDb::create().await;
    let store = db.store().await;
    store
        .bootstrap_owner("owner@localhost", "dev", "owner@localhost", Utc::now())
        .await
        .unwrap()
        .unwrap();
    let config: Config = toml::from_str(&format!(
        r#"format_version=1
environment="development"
{extra}
[server]
admin_bind="127.0.0.1:0"
worker_bind="127.0.0.1:0"
public_origin="http://127.0.0.1:8080"
[auth]
mode="dev"
dev_email="owner@localhost"
[bootstrap]
owner_email="owner@localhost"
enabled=true
"#
    ))
    .unwrap();
    let state = AppState {
        identity: verifier_from_config(&config.auth, reqwest::Client::new()),
        config: Arc::new(config),
        store,
        clock: Arc::new(Utc::now),
        agent_access: None,
        source_retention: None,
    };
    let pool = db.pool().await;
    (db, aq_server::build_admin_router(state), pool)
}
async fn call(app: &Router, path: &str, body: Option<Value>, csrf: bool) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .uri(path)
        .method(if body.is_some() { "POST" } else { "GET" })
        .header("content-type", "application/json");
    if csrf {
        req = req.header("X-AQ-Request", "1");
    }
    let response = app
        .clone()
        .oneshot(
            req.body(Body::from(body.map(|v| v.to_string()).unwrap_or_default()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!({"raw":String::from_utf8_lossy(&bytes)})),
    )
}
fn activation(catalog: &ValidatedCatalog, revision: u64) -> Value {
    json!({"operation_id":Uuid::new_v4(),"expected_revision":revision,"artifact_json":String::from_utf8(catalog.canonical_bytes().unwrap()).unwrap()})
}
const ACTIVATE: &str = "/api/profiles/catalogs/activate";
const STATE: &str = "/api/profiles/catalogs";

#[tokio::test]
async fn missing_registry_is_an_explicit_conflict() {
    let (_db, app, pool) = setup().await;
    sqlx::query("DELETE FROM profile_catalog_registry")
        .execute(&pool)
        .await
        .unwrap();
    let (status, body) = call(&app, STATE, None, false).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("registry has not been installed")
    );
}

#[tokio::test]
async fn exact_retry_survives_reactivation_and_changed_payload_conflicts() {
    let (_db, app, pool) = setup().await;
    let catalog = seed();
    assert_eq!(
        call(&app, STATE, None, false).await.1,
        json!({"revision":0,"active_digest":null})
    );
    let request = activation(&catalog, 0);
    let first = call(&app, ACTIVATE, Some(request.clone()), true).await;
    assert_eq!(first.0, StatusCode::OK, "{}", first.1);
    assert_eq!(first.1["revision"], 1);
    assert_eq!(
        call(&app, ACTIVATE, Some(activation(&catalog, 1)), true)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        call(&app, ACTIVATE, Some(request.clone()), true).await,
        first
    );
    let mut changed = request;
    changed["artifact_json"] = json!(format!("{}\n", changed["artifact_json"].as_str().unwrap()));
    assert_eq!(
        call(&app, ACTIVATE, Some(changed), true).await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(call(&app, STATE, None, false).await.1["revision"], 2);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM profile_catalog_artifacts")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM profile_catalog_operations")
            .fetch_one(&pool)
            .await
            .unwrap(),
        2
    );
}

#[tokio::test]
async fn activation_pins_environment_template_or_refuses() {
    let dir = tempfile::tempdir().unwrap();
    let digest = |c: char| std::iter::repeat_n(c, 64).collect::<String>();
    let write = |name: &str, entries: Value| {
        let path = dir.path().join(name);
        std::fs::write(
            &path,
            json!({"schema_version":1,"entries":entries}).to_string(),
        )
        .unwrap();
        path
    };
    let entry = |name: &str, c: char| json!({"name":name,"version":"1","image_digest":format!("sha256:{}",digest(c))});
    let catalog = seed();
    let pinned = write(
        "pinned.json",
        json!([entry("confined-worktree", 'a'), entry("other", 'b')]),
    );
    let (_db, app, pool) = setup_with(&format!(
        "[templates]\nallowlist={:?}\n",
        pinned.display().to_string()
    ))
    .await;
    let ok = call(&app, ACTIVATE, Some(activation(&catalog, 0)), true).await;
    assert_eq!(ok.0, StatusCode::OK, "{}", ok.1);
    assert_eq!(
        ok.1["environment_templates"],
        json!([{"environment":"confined-worktree","template_digest":digest('a')}])
    );
    let stored: Vec<(String, String)> = sqlx::query_as(
        "SELECT environment,template_digest FROM profile_catalog_environment_templates",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(stored, [("confined-worktree".into(), digest('a'))]);
    for (name, entries, reason) in [
        (
            "missing.json",
            json!([entry("other", 'b')]),
            "environment_template_missing",
        ),
        (
            "ambiguous.json",
            json!([
                entry("confined-worktree", 'a'),
                entry("confined-worktree", 'c')
            ]),
            "environment_template_ambiguous",
        ),
    ] {
        let path = write(name, entries);
        let (_db, app, pool) = setup_with(&format!(
            "[templates]\nallowlist={:?}\n",
            path.display().to_string()
        ))
        .await;
        let refused = call(&app, ACTIVATE, Some(activation(&catalog, 0)), true).await;
        assert_eq!(refused.0, StatusCode::CONFLICT, "{}", refused.1);
        assert!(refused.1.to_string().contains(reason), "{}", refused.1);
        assert_eq!(call(&app, STATE, None, false).await.1["revision"], 0);
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM profile_catalog_environment_templates"
            )
            .fetch_one(&pool)
            .await
            .unwrap(),
            0
        );
    }
}

#[tokio::test]
async fn concurrent_activation_has_one_revision_winner() {
    let (_db, app, _pool) = setup().await;
    let catalog = seed();
    let (a, b) = tokio::join!(
        call(&app, ACTIVATE, Some(activation(&catalog, 0)), true),
        call(&app, ACTIVATE, Some(activation(&catalog, 0)), true)
    );
    assert!(
        (a.0 == StatusCode::OK && b.0 == StatusCode::CONFLICT)
            || (b.0 == StatusCode::OK && a.0 == StatusCode::CONFLICT)
    );
    assert_eq!(call(&app, STATE, None, false).await.1["revision"], 1);
}

#[tokio::test]
async fn invalid_edits_preserve_last_good_and_rollback_retains_exact_artifacts() {
    let (_db, app, pool) = setup().await;
    let catalog = seed();
    assert_eq!(
        call(&app, ACTIVATE, Some(activation(&catalog, 0)), true)
            .await
            .0,
        StatusCode::OK
    );
    let mut malformed = activation(&catalog, 1);
    malformed["artifact_json"] = json!("{broken");
    assert_eq!(
        call(&app, ACTIVATE, Some(malformed), true).await.0,
        StatusCode::BAD_REQUEST
    );
    let mut texts = catalog.artifact().instructions.clone();
    texts.insert(
        "engineering@1".into(),
        "Changed instructions under existing identity.".into(),
    );
    let changed = validate(catalog.artifact().catalog.clone(), texts).unwrap();
    assert_eq!(
        call(&app, ACTIVATE, Some(activation(&changed, 1)), true)
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        call(&app, STATE, None, false).await.1["active_digest"],
        catalog.digest()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM profile_catalog_artifacts")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    // A new identity can activate; old bytes remain readable and rollback is CAS.
    let mut next_catalog = catalog.artifact().catalog.clone();
    let mut new_module = next_catalog.modules[0].clone();
    new_module.id = "new-policy".into();
    new_module.instruction = "instructions/new-policy.md".into();
    next_catalog.modules.push(new_module);
    let mut texts = catalog.artifact().instructions.clone();
    texts.insert(
        "new-policy@1".into(),
        "New policy for future compositions.".into(),
    );
    let next = validate(next_catalog, texts).unwrap();
    assert_eq!(
        call(&app, ACTIVATE, Some(activation(&next, 1)), true)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        call(&app, ACTIVATE, Some(activation(&catalog, 2)), true)
            .await
            .0,
        StatusCode::OK
    );
    for c in [&catalog, &next] {
        let fetched = call(&app, &format!("{STATE}/{}", c.digest()), None, false).await;
        assert_eq!(fetched.0, StatusCode::OK);
        let restored =
            ValidatedCatalog::from_artifact(&serde_json::to_vec(&fetched.1["artifact"]).unwrap())
                .unwrap();
        assert_eq!(
            restored.canonical_bytes().unwrap(),
            c.canonical_bytes().unwrap()
        );
    }
    // Identity history outlives rollback.
    let mut texts = next.artifact().instructions.clone();
    texts.insert(
        "new-policy@1".into(),
        "Reused version with different bytes.".into(),
    );
    let invalid = validate(next.artifact().catalog.clone(), texts).unwrap();
    assert_eq!(
        call(&app, ACTIVATE, Some(activation(&invalid, 3)), true)
            .await
            .0,
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn catalogs_require_owner_and_csrf_and_reject_bad_ids() {
    let (_db, app, pool) = setup().await;
    let catalog = seed();
    assert_eq!(
        call(&app, ACTIVATE, Some(activation(&catalog, 0)), false)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let mut nil = activation(&catalog, 0);
    nil["operation_id"] = json!(Uuid::nil());
    assert_eq!(
        call(&app, ACTIVATE, Some(nil), true).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(&app, &format!("{STATE}/not-a-digest"), None, false)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    sqlx::query("UPDATE users SET role='viewer'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(&app, STATE, None, false).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(&app, ACTIVATE, Some(activation(&catalog, 0)), true)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(&app, &format!("{STATE}/{}", catalog.digest()), None, false)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT revision FROM profile_catalog_registry")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}

// Intelligence Settings: real owner routes and transaction boundaries.
const INTELLIGENCE_SETTINGS: &str = "/api/settings/intelligence";
async fn settings_fixture() -> (TestDb, Router, sqlx::PgPool, Value, Uuid) {
    let (db, app, pool) = setup().await;
    let catalog = seed();
    assert_eq!(
        call(&app, ACTIVATE, Some(activation(&catalog, 0)), true)
            .await
            .0,
        StatusCode::OK
    );
    let project = Uuid::new_v4();
    let binding = json!({"schema_version":1,"project_id":project,"name":"Example project","enabled":false,
        "repository_id":Uuid::new_v4(),"repository_binding_generation":1,"repository_adapter_reference":"aq.git.configured",
        "graph_adapter_reference":"aq.graph.configured","graph_workspace_id":Uuid::new_v4(),"catalog_digest":catalog.digest(),
        "allowed_agent_profiles":["implementer@1"],"allowed_intelligence_profiles":["engineering@1"],
        "allowed_execution_profiles":["confined-development@1"],"allowed_node_ids":[],"allowed_worker_groups":[],
        "allowed_environments":["confined-worktree"],"policy":{"identity":"example","revision":"1","digest":"a".repeat(64)},
        "context":[],"ceilings":{"permissions":[],"context_bytes":65536,"wall_time_ms":900000,"memory_bytes":1073741824,
        "cpu_millis":1000,"process_limit":512,"cost_limit_microunits":null,"currency":null,"require_hard_cost_enforcement":false}});
    let observation = json!({"repository_id":binding["repository_id"],"graph_workspace_id":binding["graph_workspace_id"],
        "repository_adapter_reference":"aq.git.configured","graph_adapter_reference":"aq.graph.configured","repository":"verified","graph":"verified",
        "repository_generation":1,"observed_at_ms":1,"expires_at_ms":30001});
    sqlx::query("INSERT INTO project_execution_bindings(project_id,revision,binding,validated_at_ms,binding_observation) VALUES($1,1,$2,1,$3)")
        .bind(project).bind(binding).bind(observation).execute(&pool).await.unwrap();
    let profile = &catalog.artifact().catalog.intelligence[0];
    let req = json!({"operation_id":Uuid::new_v4(),"expected_revision":"1","profile_id":profile.id,"profile_version":profile.version,
        "name":"Standard engineering","description":"The owner's engineering profile","candidates":profile.candidates});
    (db, app, pool, req, project)
}

#[tokio::test]
async fn intelligence_settings_updates_catalog_and_project_pins_and_replays() {
    let (_db, app, pool, request, project) = settings_fixture().await;
    let old = seed();
    let (status, snapshot) = call(&app, INTELLIGENCE_SETTINGS, None, false).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(snapshot["revision"], "1");
    assert_eq!(
        snapshot["profiles"][0]["projects"][0]["id"],
        project.to_string()
    );
    assert_eq!(
        call(&app, INTELLIGENCE_SETTINGS, Some(request.clone()), false)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let (status, saved) = call(&app, INTELLIGENCE_SETTINGS, Some(request.clone()), true).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["disposition"], "saved");
    assert_eq!(saved["revision"], "2");
    assert_eq!(saved["profile"]["version"], 2);
    assert_eq!(saved["profile"]["name"], "Standard engineering");
    let binding: Value =
        sqlx::query_scalar("SELECT binding FROM project_execution_bindings WHERE project_id=$1")
            .bind(project)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(binding["enabled"], false);
    assert_eq!(binding["allowed_agent_profiles"], json!(["implementer@2"]));
    assert_eq!(
        binding["allowed_intelligence_profiles"],
        json!(["engineering@2"])
    );
    assert_eq!(binding["catalog_digest"], saved["active_digest"]);
    let raw: Vec<u8> =
        sqlx::query_scalar("SELECT artifact FROM profile_catalog_artifacts WHERE digest=$1")
            .bind(saved["active_digest"].as_str().unwrap())
            .fetch_one(&pool)
            .await
            .unwrap();
    let catalog = ValidatedCatalog::from_artifact(&raw).unwrap();
    assert_eq!(
        catalog.artifact().catalog.agents[0].intelligence,
        "engineering@2"
    );
    assert_eq!(catalog.artifact().instructions, old.artifact().instructions);
    let retained: Vec<u8> =
        sqlx::query_scalar("SELECT artifact FROM profile_catalog_artifacts WHERE digest=$1")
            .bind(old.digest())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(retained, old.canonical_bytes().unwrap());
    assert_eq!(
        call(&app, INTELLIGENCE_SETTINGS, Some(request.clone()), true)
            .await
            .1,
        saved
    );
    assert_eq!(
        call(
            &app,
            &format!(
                "{INTELLIGENCE_SETTINGS}/operations/{}",
                request["operation_id"].as_str().unwrap()
            ),
            None,
            false
        )
        .await
        .1,
        saved
    );
    let mut stale = request.clone();
    stale["operation_id"] = json!(Uuid::new_v4());
    assert_eq!(
        call(&app, INTELLIGENCE_SETTINGS, Some(stale), true).await.0,
        StatusCode::CONFLICT
    );
    let mut changed = request;
    changed["description"] = json!("Different request under the same identity");
    assert_eq!(
        call(&app, INTELLIGENCE_SETTINGS, Some(changed), true)
            .await
            .0,
        StatusCode::CONFLICT
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM profile_catalog_artifacts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 2);
}

#[tokio::test]
async fn intelligence_settings_binding_failure_rolls_back_activation_and_artifacts() {
    let (_db, app, pool, request, _) = settings_fixture().await;
    sqlx::query("CREATE FUNCTION fail_settings_test() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'simulated binding storage failure'; END $$").execute(&pool).await.unwrap();
    sqlx::query("CREATE TRIGGER settings_failure BEFORE UPDATE ON project_execution_bindings FOR EACH ROW EXECUTE FUNCTION fail_settings_test()").execute(&pool).await.unwrap();
    assert_eq!(
        call(&app, INTELLIGENCE_SETTINGS, Some(request), true)
            .await
            .0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(call(&app, STATE, None, false).await.1["revision"], 1);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM profile_catalog_artifacts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM project_execution_binding_history")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn intelligence_settings_rejects_unconfigured_models_and_keeps_drafts_unchanged() {
    let (_db, app, _pool, mut request, _) = settings_fixture().await;
    request["candidates"][0]["model"] = json!("unconfigured-model");
    let (status, body) = call(&app, INTELLIGENCE_SETTINGS, Some(request), true).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body["message"].as_str().unwrap().contains("not configured"));
    assert_eq!(call(&app, STATE, None, false).await.1["revision"], 1);
}

#[tokio::test]
async fn intelligence_settings_two_editors_cannot_overwrite_each_other() {
    let (_db, app, _pool, first, _) = settings_fixture().await;
    let mut second = first.clone();
    second["operation_id"] = json!(Uuid::new_v4());
    second["name"] = json!("Other editor");
    let (a, b) = tokio::join!(
        call(&app, INTELLIGENCE_SETTINGS, Some(first), true),
        call(&app, INTELLIGENCE_SETTINGS, Some(second), true)
    );
    let mut statuses = [a.0.as_u16(), b.0.as_u16()];
    statuses.sort();
    assert_eq!(statuses, [200, 409]);
}

#[tokio::test]
async fn intelligence_settings_noop_save_does_not_republish_or_repin() {
    let (_db, app, pool, mut request, _) = settings_fixture().await;
    request["name"] = json!("engineering");
    request["description"] = json!("");
    let (status, response) = call(&app, INTELLIGENCE_SETTINGS, Some(request), true).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["disposition"], "unchanged");
    assert_eq!(response["revision"], "1");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM project_execution_binding_history")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[test]
fn intelligence_settings_configured_choice_reaches_future_composition_and_keeps_old_artifacts() {
    use aq_profile_settings::{ModelChoice, SaveRequest};
    use sha2::{Digest, Sha256};
    let current = seed();
    let state = aq_profiles::ActivationState {
        revision: 1,
        active_digest: Some(current.digest().into()),
        known_identities: current.artifact().identities.clone(),
        retained: Default::default(),
    };
    let mut candidate = current.artifact().catalog.intelligence[0].candidates[0].clone();
    candidate.provider = "configured-provider".into();
    candidate.model = "configured-model".into();
    candidate.reasoning_configuration_reference = "deep@1".into();
    candidate.harnesses = vec!["codex".into()];
    let choice = ModelChoice {
        provider: candidate.provider.clone(),
        model: candidate.model.clone(),
        reasoning_configuration_reference: candidate.reasoning_configuration_reference.clone(),
        reasoning: "deep".into(),
        harness: "codex".into(),
        preference_only: false,
        options: Default::default(),
        source: None,
        observed_at: None,
    };
    let request = SaveRequest {
        operation_id: Uuid::new_v4(),
        expected_revision: "1".into(),
        profile_id: "engineering".into(),
        profile_version: 1,
        name: "Engineering".into(),
        description: "".into(),
        candidates: vec![candidate],
    };
    let prepared = aq_profile_settings::prepare(&current, &state, &request, &[choice]).unwrap();
    let catalog = &prepared.catalog;
    let context = |id: &str| json!({"source":{"identity":id,"digest":format!("{:x}",Sha256::digest(b"Rules")),"revision":"1"},"text":"Rules","delivery":"startup","required":true,"omission_priority":0});
    let p = &catalog.artifact().catalog.execution[0].permissions;
    let composition:aq_profiles::CompileRequest=serde_json::from_value(json!({
        "profile":{"catalog_digest":catalog.digest(),"agent_profile":"implementer@2","project_binding_id":Uuid::new_v4(),"expected_project_revision":2,"task_context_revision":"task:1"},
        "ceilings":{"installation":p,"project":p,"execution":p,"caller":p},"available_tools":["task.read","knowledge.read","source.write"],"available_adapters":["codex"],"harness":"codex","environment":"confined-worktree",
        "platform":context("platform:1"),"project":context("project:1"),"assignment":context("assignment:1"),"context":[],"overrides":[]
    })).unwrap();
    let compiled = aq_profiles::compile(catalog, &composition, &"a".repeat(64)).unwrap();
    assert!(
        compiled.composition_valid,
        "{:?}",
        compiled.blocking_reasons
    );
    assert_eq!(
        compiled.explanation.selection.candidates[0].model,
        "configured-model"
    );
    assert_eq!(
        compiled.explanation.selection.candidates[0].reasoning_configuration_reference,
        "deep@1"
    );
    assert_eq!(current.artifact().catalog.intelligence[0].version, 1);
}

#[tokio::test]
async fn intelligence_settings_adds_spreadsheet_rows_without_assigning_them_or_losing_existing_choices()
 {
    let (_db, app, pool, mut request, project) = settings_fixture().await;
    request["profile_id"] = json!("new-profile");
    request["profile_version"] = json!(0);
    request["name"] = json!("New profile");
    request["candidates"] = json!([]);
    let (status, saved) = call(&app, INTELLIGENCE_SETTINGS, Some(request.clone()), true).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["profile"]["version"], 1);
    assert_eq!(saved["profile"]["name"], "New profile");
    assert_eq!(
        saved["profile"]["candidates"][0]["provider"],
        "configuration-required"
    );
    let binding: Value =
        sqlx::query_scalar("SELECT binding FROM project_execution_bindings WHERE project_id=$1")
            .bind(project)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        binding["allowed_intelligence_profiles"],
        json!(["engineering@1"])
    );
    assert_eq!(binding["allowed_agent_profiles"], json!(["implementer@1"]));
    assert_eq!(binding["catalog_digest"], saved["active_digest"]);
    let (status, read) = call(&app, INTELLIGENCE_SETTINGS, None, false).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(read["profiles"].as_array().unwrap().len(), 2);
    assert_eq!(
        read["profiles"][0]["candidates"],
        serde_json::to_value(&seed().artifact().catalog.intelligence[0].candidates).unwrap()
    );
    assert_eq!(
        call(&app, INTELLIGENCE_SETTINGS, Some(request.clone()), true)
            .await
            .1,
        saved
    );
    request["expected_revision"] = saved["revision"].clone();
    request["operation_id"] = json!(Uuid::new_v4());
    assert_eq!(
        call(&app, INTELLIGENCE_SETTINGS, Some(request), true)
            .await
            .0,
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn intelligence_settings_imports_preferences_without_qualifying_a_runtime() {
    let directory = tempfile::tempdir().unwrap();
    let cache = directory.path().join("models.json");
    let cached = json!([{"provider":"openai","model":"legacy-default-model","harness":"codex",
        "reasoning_configuration_reference":"v1-low-fast@1","reasoning":"low (fast service)","preference_only":false,
        "options":{"reasoning_effort":"low","service_tier":"fast"}}, {"provider":"google","model":"discovered-model","harness":"gemini",
        "reasoning_configuration_reference":"discovered-medium@1","reasoning":"medium","preference_only":true,
        "source":"Gemini CLI account catalog","observed_at":"2026-11-09T00:00:00Z","options":{"thinking_level":"medium"}}]);
    std::fs::write(&cache, serde_json::to_vec(&cached).unwrap()).unwrap();
    let extra = format!(
        "intelligence_preferences_file={}\n",
        serde_json::to_string(cache.to_str().unwrap()).unwrap()
    );
    let (_db, app, pool) = setup_with(&extra).await;
    let catalog = seed();
    assert_eq!(
        call(&app, ACTIVATE, Some(activation(&catalog, 0)), true)
            .await
            .0,
        StatusCode::OK
    );
    let (status, read) = call(&app, INTELLIGENCE_SETTINGS, None, false).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(read["model_choices"].as_array().unwrap().len(), 2);
    assert_eq!(read["model_choices"][1]["model"], "discovered-model");
    std::fs::write(&cache, serde_json::to_vec(&json!([cached[0]])).unwrap()).unwrap();
    let refreshed = call(&app, INTELLIGENCE_SETTINGS, None, false).await.1;
    assert_eq!(
        refreshed["model_choices"].as_array().unwrap().len(),
        1,
        "Cache refresh requires no daemon restart"
    );
    assert_eq!(read["model_choices"][0]["preference_only"], true);
    assert_eq!(read["model_choices"][0]["options"]["service_tier"], "fast");
    let request = json!({"operation_id":Uuid::new_v4(),"expected_revision":"1","profile_id":"legacy-default",
        "profile_version":0,"name":"Legacy default","description":"Imported preference",
        "candidates":[{"id":"codex","provider":"openai","model":"legacy-default-model",
            "reasoning_configuration_reference":"v1-low-fast@1","harnesses":["codex"]}]});
    let (status, saved) = call(&app, INTELLIGENCE_SETTINGS, Some(request), true).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(
        saved["profile"]["candidates"][0]["model"],
        "legacy-default-model"
    );
    assert_eq!(saved["updated_projects"], json!([]));
    let registry_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM intelligence_registry")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        registry_rows, 0,
        "Preferences must not register credentials or qualified harnesses"
    );
}
