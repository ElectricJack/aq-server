//! Compiling the routing policy when it is saved (spec 0011 §6): the owner router, PostgreSQL,
//! and internal work answered by a local fake API endpoint. No real provider is contacted.
use super::*;
use crate::common::fake_api;
use aq_intelligence_config::{CredentialConfig, IntelligenceConfig, LoadedRegistry};
use aq_internal_work::{ApiEndpoint, InternalWorkConfig, Outcome, Way};
use aq_profiles::ModelCandidate;
use aq_routing::{fixtures, policy};
use aq_selection::{
    ArtifactRef, AuthorizedCredential, CredentialKind, Digest, InstalledHarness, NativeLimits,
    NativeModelSupport, Text,
};
use aq_server::{InternalWork, routing};
use std::{sync::Mutex, time::Duration};

const ROUTING: &str = "/api/settings/intelligence/routing";
const SOURCE: &str = "# Routing\n\nBug fixes are Standard, high effort.\n";
const UNRESOLVED: &str = "Art and visual design go to the plotter.";

fn artifact(name: &str) -> ArtifactRef {
    ArtifactRef {
        id: Text::new(name).unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(name.as_bytes()),
    }
}

/// Codex also offers the model of the `internal` profile, so internal work has a model choice.
fn harness(name: &str) -> InstalledHarness {
    let models = match name {
        "codex" => vec![NativeModelSupport {
            provider: "openai".into(),
            model: "openai-model".into(),
            reasoning_configuration_reference: "openai-reasoning:1".into(),
            native_reasoning: "low".into(),
        }],
        _ => vec![],
    };
    InstalledHarness {
        harness: name.into(),
        cli_version: "fixture-1".into(),
        adapter: artifact(&format!("{name}-adapter")),
        environment: artifact("confined-worktree"),
        qualification: artifact(&format!("{name}-qualification")),
        failure_classifier: None,
        models,
        capabilities: vec![],
        limits: NativeLimits {
            context_bytes: 8192,
            max_output_tokens: 512,
            wall_time_ms: 60000,
            memory_bytes: 268435456,
            cpu_millis: 60000,
            process_limit: 8,
            hard_cost_enforcer: None,
        },
    }
}

/// An OpenAI API key for internal work; Claude, Codex, Gemini and OpenCode qualified.
fn registry() -> Arc<LoadedRegistry> {
    let credential = AuthorizedCredential {
        profile: artifact("openai-key"),
        provider: "openai".into(),
        account_reference: "openai-key-account".into(),
        endpoint_reference: "openai-key-endpoint".into(),
        revision: 1,
        enabled: true,
        provisionable: false,
        material_owner: None,
        allowed_backends: aq_selection::default_allowed_backends(),
    };
    let config = IntelligenceConfig {
        accounting: Default::default(),
        material_store: None,
        revision: 1,
        credentials: vec![CredentialConfig {
            kind: CredentialKind::ApiKey,
            allowed_backends: aq_selection::default_allowed_backends(),
            credential,
            allowed_node_ids: vec![],
            allow_enrolled_nodes: true,
            secret_file: None,
            delivery_mode: None,
        }],
        qualified_harnesses: ["claude", "codex", "gemini", "opencode"]
            .map(harness)
            .to_vec(),
    };
    Arc::new(config.load().unwrap())
}

fn candidate(id: &str, provider: &str, harness: &str) -> ModelCandidate {
    ModelCandidate {
        id: id.into(),
        provider: provider.into(),
        model: format!("{provider}-model"),
        reasoning_configuration_reference: format!("{provider}-reasoning:1"),
        harnesses: vec![harness.into()],
    }
}

/// One profile per class of the reference options, named `level-<class>`, whose candidates are
/// that class's offers; Standard, high also lists a harness that is not qualified. `internal`
/// serves internal work through the OpenAI API.
fn catalog() -> ValidatedCatalog {
    let base = seed();
    let instructions = base.artifact().instructions.clone();
    let mut catalog = base.artifact().catalog.clone();
    let template = catalog.intelligence[0].clone();
    let profile = |id: &str, candidates: Vec<ModelCandidate>| {
        let mut p = template.clone();
        p.id = id.into();
        p.candidates = candidates;
        p
    };
    for (class, offers) in &fixtures::options().classes {
        let mut candidates: Vec<ModelCandidate> = (offers.iter())
            .map(|o| candidate(&o.harness, &o.provider, &o.harness))
            .collect();
        if class.to_string() == "standard-high" {
            candidates.push(candidate("cursor", "cursor-ai", "cursor"));
        }
        let id = format!("level-{class}");
        catalog.intelligence.push(profile(&id, candidates));
    }
    let internal = vec![candidate("api", "openai", "codex")];
    catalog.intelligence.push(profile("internal", internal));
    aq_profiles::validate(catalog, instructions).unwrap()
}

fn assignments(classes: &[&str]) -> Value {
    let items: Vec<Value> = (classes.iter())
        .map(|c| {
            let (parent, effort) = c.split_once('-').unwrap();
            json!({"profile_id": format!("level-{c}"), "parent_id": parent, "effort": effort})
        })
        .collect();
    Value::Array(items)
}

/// The compile answer the fake model gives.
fn answer(policy: &policy::Policy, unresolved: &[&str]) -> Value {
    let text = json!({"policy": policy, "unresolved": unresolved}).to_string();
    fake_api::completed(&text)
}

struct Server {
    app: Router,
    state: AppState,
    seen: Arc<Mutex<fake_api::Seen>>,
    _stop: tokio::sync::watch::Sender<bool>,
}

struct Env {
    db: TestDb,
    pool: sqlx::PgPool,
    dir: tempfile::TempDir,
    server: Server,
}

impl Env {
    /// A server whose internal work answers `replies` in turn; with `serving` false its loop
    /// is not running and its waiters poll once an hour, so a compile stays queued.
    async fn new(replies: Vec<Value>, serving: bool) -> Self {
        let db = TestDb::create().await;
        let store = db.store().await;
        store
            .bootstrap_owner("owner@localhost", "dev", "owner@localhost", Utc::now())
            .await
            .unwrap()
            .unwrap();
        let pool = db.pool().await;
        let owner: Uuid = sqlx::query_scalar("SELECT id FROM users WHERE email='owner@localhost'")
            .fetch_one(&pool)
            .await
            .unwrap();
        let catalog = catalog();
        sqlx::query(
            "INSERT INTO profile_catalog_artifacts(digest,artifact,created_by,created_at_ms)
             VALUES($1,$2,$3,1)",
        )
        .bind(catalog.digest())
        .bind(catalog.canonical_bytes().unwrap())
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("UPDATE profile_catalog_registry SET active_digest=$1 WHERE singleton")
            .bind(catalog.digest())
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE internal_work_settings SET document=$1 WHERE singleton")
            .bind(json!({"default_profile_id": "internal", "overrides": {}}))
            .execute(&pool)
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("key");
        std::fs::write(&secret, "sk-test-secret-value\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();
        let server = Self::server(&db, &secret, replies, serving).await;
        Env {
            db,
            pool,
            dir,
            server,
        }
    }

    async fn server(
        db: &TestDb,
        secret: &std::path::Path,
        replies: Vec<Value>,
        serving: bool,
    ) -> Server {
        let (endpoint, seen) = fake_api::serve_each(200, replies).await;
        let registry = registry();
        let work = InternalWork::new(
            db.pool().await,
            aq_server::system_clock(),
            Some(&InternalWorkConfig {
                api: vec![ApiEndpoint {
                    provider: "openai".into(),
                    endpoint,
                    secret_file: secret.to_path_buf(),
                }],
                poll_ms: if serving { 20 } else { 3_600_000 },
                enabled: serving,
            }),
            Some(registry.clone()),
            None,
        )
        .unwrap();
        let mut config: Config = toml::from_str(
            r#"format_version=1
environment="development"
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
"#,
        )
        .unwrap();
        config.intelligence = Some(registry);
        let state = AppState {
            internal_work: work.clone(),
            identity: verifier_from_config(&config.auth, reqwest::Client::new()),
            config: Arc::new(config),
            store: db.store().await,
            clock: Arc::new(Utc::now),
            agent_access: None,
            source_retention: None,
        };
        let (stop, rx) = tokio::sync::watch::channel(false);
        if serving {
            tokio::spawn(work.serve(rx));
        }
        Server {
            app: aq_server::build_admin_router(state.clone()),
            state,
            seen,
            _stop: stop,
        }
    }

    /// Drops this server and starts another against the same database, whose internal work
    /// runs and answers `replies`, and whose compile resumer runs.
    async fn restart(&mut self, replies: Vec<Value>) {
        let secret = self.dir.path().join("key");
        self.server = Self::server(&self.db, &secret, replies, true).await;
        let (_, rx) = (&self.server._stop, self.server._stop.subscribe());
        tokio::spawn(routing::compile::serve(self.server.state.clone(), rx));
    }

    /// Saves `markdown` and `assignments` (all reference classes when `None`).
    async fn save(&self, markdown: &str, assignments: Option<Value>) -> Value {
        let app = &self.server.app;
        let snapshot = call(app, INTELLIGENCE_STRUCTURE, None, false).await.1;
        let mut request = structure_request(&snapshot);
        request["routing_markdown"] = json!(markdown);
        request["assignments"] =
            assignments.unwrap_or_else(|| self::assignments(&fixtures::ASSIGNED_CLASSES));
        let (status, saved) = call(app, INTELLIGENCE_STRUCTURE, Some(request), true).await;
        assert_eq!(status, StatusCode::OK, "{saved}");
        saved
    }

    /// Saves `markdown` with every reference class assigned, without the server adapter: the
    /// save commits and no compile is requested, as when the request is late or lost.
    async fn save_without_compiling(&self, markdown: &str) -> Value {
        let snapshot = call(&self.server.app, INTELLIGENCE_STRUCTURE, None, false)
            .await
            .1;
        let mut request = structure_request(&snapshot);
        request["routing_markdown"] = json!(markdown);
        request["assignments"] = assignments(&fixtures::ASSIGNED_CLASSES);
        let owner: Uuid = sqlx::query_scalar("SELECT id FROM users WHERE email='owner@localhost'")
            .fetch_one(&self.pool)
            .await
            .unwrap();
        let request = serde_json::from_value(request).unwrap();
        aq_profile_settings::structure::save(&self.pool, owner, request)
            .await
            .unwrap()
    }

    async fn routing(&self) -> Value {
        let (status, body) = call(&self.server.app, ROUTING, None, false).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body
    }

    /// The routing read once the latest compile has finished.
    async fn settled(&self) -> Value {
        for _ in 0..400 {
            let body = self.routing().await;
            if body["latest"]["status"] != "compiling" {
                return body;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the compile did not finish: {}", self.routing().await);
    }

    async fn compiles(&self) -> Vec<(Uuid, String, String)> {
        sqlx::query_as(
            "SELECT id,affected_id,status FROM internal_tasks WHERE affected_kind='routing_policy'
             ORDER BY created_at_ms",
        )
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }

    async fn active_digest(&self) -> Option<String> {
        sqlx::query_scalar("SELECT active_policy_digest FROM routing_state WHERE singleton")
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }
}

fn answered(policy: &policy::Policy) -> Outcome {
    Outcome::Answered {
        answer: json!({"policy": policy, "unresolved": []}),
        provider: "openai".into(),
        model: "openai-model".into(),
        way: Way::Api,
        elapsed_ms: 5,
        usage: None,
        attempts: vec![],
    }
}

#[tokio::test]
async fn saving_changed_markdown_submits_one_compilation_and_an_unchanged_save_submits_none() {
    let (reference, _) = fixtures::reference();
    let e = Env::new(vec![answer(&reference, &[])], false).await;
    assert_eq!(
        e.routing().await,
        json!({"enabled": false, "active": null, "latest": null})
    );
    let saved = e.save(SOURCE, None).await;
    let compiles = e.compiles().await;
    assert_eq!(compiles.len(), 1, "{compiles:?}");
    assert_eq!(compiles[0].1, saved["revision"].as_str().unwrap());
    let request: Value = sqlx::query_scalar("SELECT request FROM internal_tasks WHERE id=$1")
        .bind(compiles[0].0)
        .fetch_one(&e.pool)
        .await
        .unwrap();
    assert_eq!(request["purpose"], "playbook_compilation");
    assert_eq!(
        (request["blocking"].clone(), request["escalate"].clone()),
        (json!(false), json!(true))
    );
    assert_eq!(request["deadline_ms"], 600_000);
    assert_eq!(request["max_output_bytes"], 262_144);
    assert_eq!(request["affected"]["kind"], "routing_policy");
    assert_eq!(
        request["instructions"],
        aq_routing::compile::COMPILER_INSTRUCTIONS
    );
    let input = request["input"].as_str().unwrap();
    assert!(
        input.contains(SOURCE) && input.contains("# What Settings offers"),
        "{input}"
    );
    let options = routing::compile::options(&e.server.state).await.unwrap();
    assert_eq!(
        request["answer"]["schema"],
        aq_routing::compile::answer_schema(&options)
    );
    let latest = &e.routing().await["latest"];
    assert_eq!(latest["status"], "compiling");
    assert_eq!(latest["internal_task_id"], json!(compiles[0].0));
    assert_eq!(latest["structure_revision"], 1);

    // The same document again, a parent description alone, and a replay submit nothing.
    let unchanged = e.save(SOURCE, None).await;
    assert_eq!(unchanged["disposition"], "unchanged");
    let snapshot = call(&e.server.app, INTELLIGENCE_STRUCTURE, None, false)
        .await
        .1;
    let mut described = structure_request(&snapshot);
    described["parents"][1]["description"] = json!("Everyday work");
    let (status, saved) = call(
        &e.server.app,
        INTELLIGENCE_STRUCTURE,
        Some(described.clone()),
        true,
    )
    .await;
    assert_eq!(
        (status, saved["disposition"].clone()),
        (StatusCode::OK, json!("saved"))
    );
    let (status, _) = call(&e.server.app, INTELLIGENCE_STRUCTURE, Some(described), true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(e.compiles().await.len(), 1);
    e.save("# Routing\n\nEverything is Standard.\n", None).await;
    assert_eq!(e.compiles().await.len(), 2);
}

#[tokio::test]
async fn a_valid_compiled_policy_becomes_active_immediately_with_its_summary() {
    let (reference, _) = fixtures::reference();
    let e = Env::new(vec![answer(&reference, &[])], true).await;
    let (status, _) = call(&e.server.app, ROUTING, Some(json!({"enabled": true})), true).await;
    assert_eq!(status, StatusCode::CONFLICT, "no policy is active yet");
    e.save(SOURCE, None).await;
    let body = e.settled().await;
    let digest = policy::digest(&reference);
    assert_eq!(body["latest"]["status"], "active", "{body}");
    assert_eq!(body["latest"]["error"], Value::Null);
    assert_eq!(body["active"]["digest"], digest);
    assert_eq!(body["active"]["structure_revision"], 1);
    assert_eq!(
        body["active"]["summary"],
        json!(policy::summary(&reference))
    );
    assert_eq!(body["active"]["problems"], json!([]));
    assert!(
        body["active"]["took_effect_at_ms"].as_i64().unwrap()
            >= body["active"]["created_at_ms"].as_i64().unwrap()
    );
    assert_eq!(e.active_digest().await, Some(digest.clone()));
    let (active, active_digest, revision) =
        routing::compile::active(&e.pool).await.unwrap().unwrap();
    assert_eq!((active, active_digest, revision), (reference, digest, 1));
    {
        let seen = e.server.seen.lock().unwrap();
        assert_eq!(seen.bodies.len(), 1, "one model call");
        let prompt = seen.bodies[0]["input"].as_str().unwrap();
        assert!(prompt.starts_with(aq_routing::compile::COMPILER_INSTRUCTIONS));
        assert!(prompt.contains(SOURCE));
    }
    // The switch: owner only, CSRF-guarded, and on now that a policy is active.
    let (status, _) = call(
        &e.server.app,
        ROUTING,
        Some(json!({"enabled": true})),
        false,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, body) = call(&e.server.app, ROUTING, Some(json!({"enabled": true})), true).await;
    assert_eq!(
        (status, body["enabled"].clone()),
        (StatusCode::OK, json!(true))
    );
    assert!(routing::enabled(&e.pool).await.unwrap());
    let (status, _) = call(
        &e.server.app,
        ROUTING,
        Some(json!({"enabled": "yes"})),
        true,
    )
    .await;
    assert!(status.is_client_error());
    sqlx::query("INSERT INTO users(id,email,role,issuer,subject,created_at) VALUES($1,'routing-viewer@localhost','viewer','dev','routing-viewer@localhost',now())")
        .bind(Uuid::new_v4()).execute(&e.pool).await.unwrap();
    let response = e
        .server
        .app
        .clone()
        .oneshot(
            Request::get(ROUTING)
                .header("x-aq-dev-email", "routing-viewer@localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn an_invalid_compiled_policy_is_stored_failed_with_paths_and_the_previous_stays_active() {
    let (reference, _) = fixtures::reference();
    // Fits the schema, fails validation.
    let mut invalid = reference.clone();
    invalid.kinds.get_mut("docs").unwrap().max_class = "standard-medium".parse().unwrap();
    invalid.risk.remove(&aq_routing::classify::Risk::Medium);
    // Names a level Settings does not offer, which the answer schema already refuses.
    let mut unoffered = reference.clone();
    unoffered.kinds.get_mut("chore").unwrap().class = "simple-medium".parse().unwrap();
    let replies = vec![
        answer(&reference, &[]),
        answer(&invalid, &[]),
        answer(&unoffered, &[]),
    ];
    let e = Env::new(replies, true).await;
    e.save(SOURCE, None).await;
    assert_eq!(e.settled().await["latest"]["status"], "active");
    e.save(
        "# Routing\n\nDocs are at most Standard, medium effort.\n",
        None,
    )
    .await;
    let body = e.settled().await;
    let latest = &body["latest"];
    assert_eq!(latest["status"], "failed", "{body}");
    assert_eq!(latest["structure_revision"], 2);
    let above =
        "Standard, high effort is above the highest level for this kind, Standard, medium effort";
    let medium =
        "there is no rule for medium risk, the risk assumed when a task cannot be classified";
    assert_eq!(
        latest["problems"],
        json!([{"path": "kinds.docs.class", "message": above}, {"path": "risk.medium", "message": medium}])
    );
    assert_eq!(
        latest["error"],
        format!("kinds.docs.class: {above}\nrisk.medium: {medium}")
    );
    assert_eq!(latest["unresolved"], json!([]));
    let digest = policy::digest(&reference);
    assert_eq!(body["active"]["digest"], digest);
    assert_eq!(body["active"]["structure_revision"], 1);
    assert_eq!(e.active_digest().await, Some(digest.clone()));

    e.save("# Routing\n\nChores are Simple, medium effort.\n", None)
        .await;
    let body = e.settled().await;
    assert_eq!(body["latest"]["status"], "failed", "{body}");
    let error = body["latest"]["error"].as_str().unwrap();
    assert!(
        error.starts_with("The compile did not finish (invalid output)"),
        "{error}"
    );
    assert!(error.contains("/policy/kinds/chore/class"), "{error}");
    assert_eq!(body["active"]["digest"], digest);
}

#[tokio::test]
async fn a_compile_whose_answer_lists_an_unresolved_sentence_is_stored_failed_and_does_not_activate()
 {
    let (reference, _) = fixtures::reference();
    let e = Env::new(vec![answer(&reference, &[UNRESOLVED])], true).await;
    e.save(&format!("{SOURCE}{UNRESOLVED}\n"), None).await;
    let body = e.settled().await;
    assert_eq!(body["latest"]["status"], "failed", "{body}");
    assert_eq!(body["latest"]["unresolved"], json!([UNRESOLVED]));
    assert_eq!(body["latest"]["problems"], json!([]));
    assert_eq!(
        body["latest"]["error"],
        format!("Could not be resolved: {UNRESOLVED}")
    );
    assert_eq!(body["active"], Value::Null);
    assert_eq!(e.active_digest().await, None);
    let (status, _) = call(&e.server.app, ROUTING, Some(json!({"enabled": true})), true).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn a_slow_compile_of_an_older_revision_never_replaces_a_newer_active_policy() {
    let (older, _) = fixtures::reference();
    let mut newer = older.clone();
    newer.classifier_instructions.push_str(" Answer briefly.");
    let e = Env::new(vec![answer(&older, &[])], false).await;
    e.save(SOURCE, None).await;
    e.save("# Routing\n\nEverything is Standard.\n", None).await;
    let compiles = e.compiles().await;
    let (first, second) = (compiles[0].0, compiles[1].0);
    let state = &e.server.state;
    routing::compile::completed(state, second, &answered(&newer))
        .await
        .unwrap();
    routing::compile::completed(state, first, &answered(&older))
        .await
        .unwrap();
    let body = e.routing().await;
    assert_eq!(body["active"]["digest"], policy::digest(&newer), "{body}");
    assert_eq!(body["active"]["structure_revision"], 2);
    assert_eq!(body["latest"]["status"], "active");
    let stored: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT status,digest FROM routing_policies WHERE internal_task_id=$1")
            .bind(first)
            .fetch_all(&e.pool)
            .await
            .unwrap();
    assert_eq!(
        stored,
        [("superseded".to_string(), Some(policy::digest(&older)))]
    );
    assert_eq!(e.active_digest().await, Some(policy::digest(&newer)));
    // A result is recorded once.
    let failed = Outcome::Failed {
        reason: aq_internal_work::Reason::DeadlineExceeded,
        detail: "late".into(),
        attempts: vec![],
    };
    routing::compile::completed(state, second, &failed)
        .await
        .unwrap();
    assert_eq!(
        e.routing().await["active"]["digest"],
        policy::digest(&newer)
    );
}

#[tokio::test]
async fn a_compile_submitted_before_a_restart_is_completed_afterwards() {
    let (reference, _) = fixtures::reference();
    let mut e = Env::new(vec![answer(&reference, &[])], false).await;
    e.save(SOURCE, None).await;
    let compiles = e.compiles().await;
    assert_eq!(compiles[0].2, "queued");
    assert_eq!(e.routing().await["latest"]["status"], "compiling");
    e.restart(vec![answer(&reference, &[])]).await;
    let body = e.settled().await;
    assert_eq!(body["latest"]["status"], "active", "{body}");
    assert_eq!(body["latest"]["internal_task_id"], json!(compiles[0].0));
    assert_eq!(body["active"]["digest"], policy::digest(&reference));
    assert_eq!(e.compiles().await.len(), 1, "nothing was submitted again");
}

#[tokio::test]
async fn a_retried_compile_is_recorded_and_takes_effect() {
    let (reference, _) = fixtures::reference();
    let e = Env::new(
        vec![fake_api::completed("not a policy"), answer(&reference, &[])],
        true,
    )
    .await;
    e.save(SOURCE, None).await;
    let body = e.settled().await;
    assert_eq!(body["latest"]["status"], "failed", "{body}");
    let first = e.compiles().await[0].0;
    let (status, retried) = call(
        &e.server.app,
        &format!("/api/internal-work/runs/{first}/retry"),
        Some(json!({})),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{retried}");
    for _ in 0..400 {
        if e.compiles().await.iter().any(|c| c.2 == "answered") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(routing::compile::resume(&e.server.state).await.unwrap(), 1);
    let body = e.routing().await;
    assert_eq!(body["latest"]["status"], "active", "{body}");
    assert_eq!(body["active"]["digest"], policy::digest(&reference));
    assert_eq!(routing::compile::resume(&e.server.state).await.unwrap(), 0);
}

#[tokio::test]
async fn options_list_only_assigned_classes_and_qualified_harnesses() {
    let e = Env::new(vec![fake_api::completed("{}")], false).await;
    e.save(
        SOURCE,
        Some(assignments(&["standard-high", "complex-high"])),
    )
    .await;
    let options = routing::compile::options(&e.server.state).await.unwrap();
    let reference = fixtures::options();
    let classes: Vec<String> = options.classes.keys().map(|c| c.to_string()).collect();
    assert_eq!(classes, ["standard-high", "complex-high"]);
    for (class, offers) in &options.classes {
        assert_eq!(offers, &reference.classes[class], "{class}");
        assert!(!offers.iter().any(|o| o.harness == "cursor"));
    }
}

#[tokio::test]
async fn get_routing_reports_active_problems_when_an_assignment_the_active_policy_needs_is_removed()
{
    let (reference, _) = fixtures::reference();
    let e = Env::new(vec![answer(&reference, &[])], true).await;
    e.save(SOURCE, None).await;
    let body = e.settled().await;
    assert_eq!(body["active"]["problems"], json!([]), "{body}");
    let without_deep: Vec<&str> = (fixtures::ASSIGNED_CLASSES.iter().copied())
        .filter(|c| !c.starts_with("deep-"))
        .collect();
    e.save(SOURCE, Some(assignments(&without_deep))).await;
    assert_eq!(
        e.compiles().await.len(),
        1,
        "a change of assignments alone compiles nothing"
    );
    let body = e.routing().await;
    assert_eq!(body["active"]["digest"], policy::digest(&reference));
    let problems = body["active"]["problems"].as_array().unwrap();
    assert!(
        problems.contains(&json!({"path": "reserved[0].classes[1]",
            "message": "Deep, high effort has no assigned profile in Settings"})),
        "{problems:?}"
    );
    assert!(problems.iter().any(|p| p["path"] == "kinds.spec.max_class"));
}

#[tokio::test]
async fn compiles_finishing_in_order_activate_only_the_newer_policy() {
    let (older, _) = fixtures::reference();
    let mut newer = older.clone();
    newer.classifier_instructions.push_str(" Answer briefly.");
    let e = Env::new(vec![answer(&older, &[])], false).await;
    e.save(SOURCE, None).await;
    e.save("# Routing\n\nEverything is Standard.\n", None).await;
    let compiles = e.compiles().await;
    let state = &e.server.state;
    // The older text was replaced before its compile finished, so it never takes effect.
    routing::compile::completed(state, compiles[0].0, &answered(&older))
        .await
        .unwrap();
    assert_eq!(e.active_digest().await, None);
    assert_eq!(e.routing().await["active"], Value::Null);
    routing::compile::completed(state, compiles[1].0, &answered(&newer))
        .await
        .unwrap();
    assert_eq!(e.active_digest().await, Some(policy::digest(&newer)));
    let body = e.routing().await;
    assert_eq!(body["active"]["structure_revision"], 2, "{body}");
}

#[tokio::test]
async fn a_markdown_save_followed_at_once_by_an_assignments_only_save_still_compiles_the_new_text()
{
    let (reference, _) = fixtures::reference();
    let e = Env::new(vec![answer(&reference, &[])], false).await;
    // The Markdown save commits; its compile request comes late.
    assert_eq!(e.save_without_compiling(SOURCE).await["revision"], "1");
    // Before it, a save of assignments alone, which compiles nothing itself.
    let without_deep: Vec<&str> = (fixtures::ASSIGNED_CLASSES.iter().copied())
        .filter(|c| !c.starts_with("deep-"))
        .collect();
    e.save(SOURCE, Some(assignments(&without_deep))).await;
    assert!(e.compiles().await.is_empty());
    let task = routing::compile::requested(&e.server.state, 1, SOURCE)
        .await
        .unwrap();
    let compiles = e.compiles().await;
    assert_eq!(compiles.len(), 1, "the new text is compiled");
    assert_eq!(Some(compiles[0].0), task);
    assert_eq!(compiles[0].1, "2", "at the current revision");
    let digest: String =
        sqlx::query_scalar("SELECT source_digest FROM routing_policies WHERE internal_task_id=$1")
            .bind(compiles[0].0)
            .fetch_one(&e.pool)
            .await
            .unwrap();
    assert_eq!(digest, aq_routing::compile::source_digest(SOURCE));
    // When a later save changed the text itself, that save compiles it instead.
    e.save("# Routing\n\nEverything is Standard.\n", None).await;
    assert_eq!(e.compiles().await.len(), 2);
    let late = routing::compile::requested(&e.server.state, 1, SOURCE)
        .await
        .unwrap();
    assert_eq!(late, None);
    assert_eq!(e.compiles().await.len(), 2);
}

#[tokio::test]
async fn a_compile_recorded_but_never_submitted_is_submitted_after_a_restart() {
    let (reference, _) = fixtures::reference();
    let mut e = Env::new(vec![answer(&reference, &[])], false).await;
    e.save_without_compiling(SOURCE).await;
    // The server stopped between recording a compile and submitting it; an older stranded
    // compile's text has since been replaced.
    let long_ago = Utc::now().timestamp_millis() - 60_000;
    sqlx::query(
        "INSERT INTO routing_policies(structure_revision,source_digest,status,created_at_ms)
         VALUES(1,$1,'compiling',$2),(0,'sha256:replaced','compiling',$2)",
    )
    .bind(aq_routing::compile::source_digest(SOURCE))
    .bind(long_ago)
    .execute(&e.pool)
    .await
    .unwrap();
    e.restart(vec![answer(&reference, &[])]).await;
    let body = e.settled().await;
    assert_eq!(body["latest"]["status"], "active", "{body}");
    assert_eq!(body["latest"]["structure_revision"], 1);
    assert_eq!(body["active"]["digest"], policy::digest(&reference));
    let compiles = e.compiles().await;
    assert_eq!(compiles.len(), 1, "{compiles:?}");
    assert_eq!(body["latest"]["internal_task_id"], json!(compiles[0].0));
    let replaced: String =
        sqlx::query_scalar("SELECT status FROM routing_policies WHERE structure_revision=0")
            .fetch_one(&e.pool)
            .await
            .unwrap();
    assert_eq!(replaced, "superseded");
}

#[tokio::test]
async fn a_finished_compile_no_save_matches_is_recorded_failed_once() {
    let (reference, _) = fixtures::reference();
    let e = Env::new(vec![answer(&reference, &[])], true).await;
    let state = &e.server.state;
    let request = aq_internal_work::Request {
        purpose: aq_internal_work::Purpose::PlaybookCompilation,
        instructions: "Compile.".into(),
        input: "# Routing".into(),
        answer: aq_internal_work::AnswerShape::Json {
            schema: json!({"type": "object"}),
        },
        blocking: false,
        affected: aq_internal_work::Affected {
            kind: "routing_policy".into(),
            id: "7".into(),
            project_id: None,
        },
        deadline_ms: 60_000,
        max_output_bytes: 262_144,
        escalate: false,
        profile_override: None,
    };
    let task = state.internal_work.submit(request).await.unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(20), state.internal_work.wait(task));
    assert!(matches!(
        outcome.await.unwrap().unwrap(),
        Outcome::Answered { .. }
    ));
    // Within the grace, a task with no row may be one whose save is still recording it.
    assert_eq!(routing::compile::resume(state).await.unwrap(), 0);
    sqlx::query("UPDATE internal_tasks SET created_at_ms=created_at_ms-60000 WHERE id=$1")
        .bind(task)
        .execute(&e.pool)
        .await
        .unwrap();
    assert_eq!(routing::compile::resume(state).await.unwrap(), 1);
    let (status, revision, error): (String, i64, String) = sqlx::query_as(
        "SELECT status,structure_revision,error FROM routing_policies WHERE internal_task_id=$1",
    )
    .bind(task)
    .fetch_one(&e.pool)
    .await
    .unwrap();
    assert_eq!((status.as_str(), revision), ("failed", 7));
    assert_eq!(error, "The compile result could not be matched to a save");
    assert_eq!(
        routing::compile::resume(state).await.unwrap(),
        0,
        "handled once"
    );
    assert_eq!(e.active_digest().await, None);
}

#[tokio::test]
async fn an_answer_past_the_unresolved_bounds_fails_with_a_bounded_error() {
    let (reference, _) = fixtures::reference();
    let long = "é".repeat(2000);
    let sentences: Vec<&str> = (0..80).map(|_| long.as_str()).collect();
    let e = Env::new(vec![answer(&reference, &sentences)], true).await;
    e.save(SOURCE, None).await;
    let body = e.settled().await;
    let latest = &body["latest"];
    assert_eq!(latest["status"], "failed", "{body}");
    let error = latest["error"].as_str().unwrap();
    assert!(
        error.starts_with("The compile did not finish (invalid output)"),
        "{error}"
    );
    assert!(error.len() <= aq_routing::compile::ERROR_MAX_BYTES);
    assert_eq!(latest["unresolved"], json!([]));
    assert_eq!(body["active"], Value::Null);
}
