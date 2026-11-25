//! Spec 0017 §5.4 (feature `project_environments`): the offer path places a
//! task in an environment. The profile's pinned environment wins; otherwise a
//! replacement keeps its predecessor's; otherwise the binding's
//! `default_environment` takes a task whose `aq:needs` it provides; otherwise
//! the allowed environment that provides them with the fewest extra
//! capabilities. A task no environment can serve is held
//! (`environment_missing_capability`), and one placed in an environment no
//! node lists waits (`no_node_has_environment`). The fixture node lists only
//! `confined-worktree`, which provides nothing; the other environments are
//! definitions with `provides` that no node runs.
use super::holds::full_work;
use crate::common::native_offer::{Case, fail_and_release, poll, poll_offered, setup};
use crate::common::{profiled, roles, selection};
use aq_admission::{
    ArtifactKind,
    control::{ControlReply, decode_reply},
};
use aq_profiles::roles::Kind;
use aq_server::profiled_execution::definitions::Definition;
use serde_json::{Value, json};
use std::sync::Arc;
use uuid::Uuid;

/// The binding allows `confined-worktree`, `web` (node) and `full` (node,
/// playwright, rust), with `default` as its default environment.
async fn environments(c: &mut Case, default: &str) {
    let env = &mut c.env.env;
    let mut config = (*env.state.config).clone();
    let mut profiled = (**config.profiled_execution.as_ref().unwrap()).clone();
    for (id, provides) in [
        ("web", json!(["node"])),
        ("full", json!(["node", "playwright", "rust"])),
    ] {
        let body = json!({"schema_version":1,"provides":provides}).to_string();
        let artifact = profiled::artifact(id, body.as_bytes());
        profiled
            .definition_bodies
            .push(Definition::new(artifact, ArtifactKind::Definition, body).unwrap());
    }
    // A changed definition catalog is a new configuration revision.
    profiled.revision += 1;
    config.profiled_execution = Some(Arc::new(profiled));
    env.state.config = Arc::new(config);
    aq_server::profiled_execution::policies::install(&env.state)
        .await
        .unwrap();
    env.app = aq_server::build_admin_router(env.state.clone());
    env.body["allowed_environments"] = json!([roles::ENVIRONMENT, "web", "full"]);
    env.body["default_environment"] = json!(default);
    let body = json!({"operation_id":Uuid::new_v4(),"expected_revision":2,"binding":env.body});
    let (status, v) =
        selection::call(&env.app, &selection::path(), Some(body), "owner@localhost").await;
    assert_eq!(status, 200, "{v}");
}
/// The sandbox profile with no `environment` line: it pins none.
async fn unpin(c: &Case) {
    let unpinned =
        roles::execution(8192).replace(&format!("environment: {}\n", roles::ENVIRONMENT), "");
    roles::save(
        &c.env.env.pool,
        c.env.actor,
        Kind::Execution,
        "sandbox",
        &unpinned,
    )
    .await;
}
fn needs(c: &Case, needs: &[&str]) {
    *c.metadata.lock().unwrap() = json!({"aq:needs": needs});
}
/// The attempt ids a poll page offers.
fn offered(body: &Value) -> Vec<Uuid> {
    let ControlReply::Poll(page) = decode_reply(&serde_json::to_vec(body).unwrap()).unwrap() else {
        panic!("{body}")
    };
    page.offers
        .iter()
        .map(|o| o.offered.offer.binding.attempt_id)
        .collect()
}
/// The environment the attempt's selection decision placed it in.
async fn placed(c: &Case, attempt: Uuid) -> String {
    sqlx::query_scalar(
        "SELECT d.decision#>>'{project,environment}' FROM profiled_launches l JOIN intelligence_decisions d USING(decision_id) WHERE l.attempt_id=$1",
    )
    .bind(attempt)
    .fetch_one(&c.env.env.pool)
    .await
    .unwrap()
}
/// A poll that offers nothing; the task's work read then holds it with `code`.
async fn refused(c: &mut Case, code: &str) -> Value {
    c.request.operation_id = Uuid::new_v4();
    let (status, body) = poll(c).await;
    assert_eq!(status, 200, "{body}");
    assert!(offered(&body).is_empty(), "{body}");
    let work = full_work(c, c.env.lineage.task_id).await;
    assert_eq!(work["readiness"]["state"], "held", "{work}");
    let reason = work["readiness"]["reasons"][0].clone();
    assert_eq!(reason["code"], code, "{work}");
    reason
}
async fn offer(c: &mut Case) -> Uuid {
    c.request.operation_id = Uuid::new_v4();
    let (status, body) = poll_offered(c).await;
    assert_eq!(status, 200, "{body}");
    let attempts = offered(&body);
    assert_eq!(attempts.len(), 1, "{body}");
    attempts[0]
}

/// The default environment takes a task that needs nothing it lacks.
#[tokio::test]
async fn the_default_environment_takes_a_task_it_serves() {
    let mut c = setup().await;
    environments(&mut c, roles::ENVIRONMENT).await;
    unpin(&c).await;
    let attempt = offer(&mut c).await;
    assert_eq!(placed(&c, attempt).await, roles::ENVIRONMENT);
}

/// A need no environment provides holds the task, naming the labels the
/// default lacks; a need the default lacks goes to the best fit (`web`, not
/// `full`), which no node lists, so the task waits.
#[tokio::test]
async fn missing_capability_is_held_and_the_best_fit_waits_for_a_node() {
    let mut c = setup().await;
    environments(&mut c, roles::ENVIRONMENT).await;
    unpin(&c).await;
    needs(&c, &["node", "python"]);
    let reason = refused(&mut c, "environment_missing_capability").await;
    assert_eq!(reason["environment"], roles::ENVIRONMENT, "{reason}");
    assert_eq!(reason["missing"], json!(["node", "python"]), "{reason}");
    assert!(
        reason["message"].as_str().unwrap().contains("node, python"),
        "{reason}"
    );

    needs(&c, &["node"]);
    let reason = refused(&mut c, "no_node_has_environment").await;
    assert_eq!(reason["environment"], "web", "{reason}");
    // Needing nothing again, it is offered in the default and the reason goes.
    needs(&c, &[]);
    let attempt = offer(&mut c).await;
    assert_eq!(placed(&c, attempt).await, roles::ENVIRONMENT);
    let work = full_work(&c, c.env.lineage.task_id).await;
    assert!(
        work["readiness"]["reasons"].as_array().unwrap().is_empty(),
        "{work}"
    );
}

/// The profile's pinned environment wins over the binding's default; without
/// the pin the default (`web`, on no node) is chosen and the task waits.
#[tokio::test]
async fn a_pinned_profile_environment_wins_over_the_default() {
    let mut c = setup().await;
    environments(&mut c, "web").await;
    let attempt = offer(&mut c).await;
    assert_eq!(placed(&c, attempt).await, roles::ENVIRONMENT);
    drop(c);

    let mut c = setup().await;
    environments(&mut c, "web").await;
    unpin(&c).await;
    let reason = refused(&mut c, "no_node_has_environment").await;
    assert_eq!(reason["environment"], "web", "{reason}");
    assert!(
        reason["message"].as_str().unwrap().contains("`web`"),
        "{reason}"
    );
}

/// A replacement keeps its predecessor's environment. The task's metadata
/// (written in Graph directly, not by `task.needs.set`, which refuses after a
/// grant) comes to need `node`: a fresh choice would be `web` and wait for a
/// node, while the replacement stays in `confined-worktree` and is held for
/// lacking `node`. Needing nothing again, it runs there.
#[tokio::test]
async fn a_retry_keeps_the_environment_of_the_attempt_it_replaces() {
    let mut c = setup().await;
    environments(&mut c, roles::ENVIRONMENT).await;
    unpin(&c).await;
    let first = offer(&mut c).await;
    fail_and_release(&c, first).await;
    needs(&c, &["node"]);
    let reason = refused(&mut c, "environment_missing_capability").await;
    assert_eq!(reason["environment"], roles::ENVIRONMENT, "{reason}");
    assert_eq!(reason["missing"], json!(["node"]), "{reason}");
    needs(&c, &[]);
    let second = offer(&mut c).await;
    assert_ne!(second, first);
    assert_eq!(placed(&c, second).await, roles::ENVIRONMENT);
}

/// A router route of the default level that chose `environment`, on a task
/// that needs `needs`.
fn routed(c: &Case, environment: &str, needs: &[&str]) {
    let route = json!({"c":roles::DEFAULT_LEVEL,"h":[],"p":[],"pf":[],"l":null,"i":"any","d":null,
        "s":"router","n":1,"e":environment});
    *c.metadata.lock().unwrap() = json!({"aq:route": route, "aq:needs": needs});
}

/// Spec 0017 §5.4 step 4: the routing document's environment is used when the
/// profile pins none (`web`, which no node lists, over the default that
/// would be offered), and the profile's pin still wins over it.
#[tokio::test]
async fn a_routed_environment_is_used_when_the_profile_pins_none() {
    let mut c = setup().await;
    environments(&mut c, roles::ENVIRONMENT).await;
    routed(&c, "web", &[]);
    let attempt = offer(&mut c).await;
    assert_eq!(placed(&c, attempt).await, roles::ENVIRONMENT);
    drop(c);

    let mut c = setup().await;
    environments(&mut c, roles::ENVIRONMENT).await;
    unpin(&c).await;
    routed(&c, "web", &[]);
    let reason = refused(&mut c, "no_node_has_environment").await;
    assert_eq!(reason["environment"], "web", "{reason}");
}

/// A retry keeps the environment of the attempt it continues, whatever the
/// route says: routed to `web` (on no node) after the first attempt ran in
/// `confined-worktree`, the replacement is offered there.
#[tokio::test]
async fn a_continuation_keeps_its_environment_over_the_route() {
    let mut c = setup().await;
    environments(&mut c, roles::ENVIRONMENT).await;
    unpin(&c).await;
    let first = offer(&mut c).await;
    fail_and_release(&c, first).await;
    routed(&c, "web", &[]);
    let second = offer(&mut c).await;
    assert_ne!(second, first);
    assert_eq!(placed(&c, second).await, roles::ENVIRONMENT);
}

/// A routed environment that lacks a need holds the task naming it; it is
/// never swapped for the environment that would fit (`full`).
#[tokio::test]
async fn a_routed_environment_missing_a_need_holds_the_task() {
    let mut c = setup().await;
    environments(&mut c, roles::ENVIRONMENT).await;
    unpin(&c).await;
    routed(&c, "web", &["rust"]);
    let reason = refused(&mut c, "environment_missing_capability").await;
    assert_eq!(reason["environment"], "web", "{reason}");
    assert_eq!(reason["missing"], json!(["rust"]), "{reason}");
}

pub(crate) const TEMPLATE: &str =
    "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
/// Admit `id` version `version` providing `provides`, as `environment.admit` will.
async fn admit(c: &Case, id: &str, version: &str, provides: Value) {
    admit_on(&c.env.env.pool, c, id, version, provides).await
}
async fn admit_on<'e>(
    db: impl sqlx::PgExecutor<'e>,
    c: &Case,
    id: &str,
    version: &str,
    provides: Value,
) {
    sqlx::query(
        "INSERT INTO environment_versions(environment_id,version,project_id,template_digest,image,provides,platform_text,harnesses,warm,admitted_at) \
         VALUES($1,$2,$3,$4,'registry.example/web@'||$4,$5,'Admitted platform.','[{\"harness\":\"codex\",\"version\":\"1\"}]','[\"pnpm install\"]',now())",
    )
    .bind(id)
    .bind(version)
    .bind(c.env.lineage.project_id)
    .bind(TEMPLATE)
    .bind(provides)
    .execute(db)
    .await
    .unwrap();
}

/// Spec 0017 §6.3: the boot config's definitions and template allowlist and
/// the admitted versions in the database are read through one `Catalog`.
#[tokio::test]
async fn boot_definitions_and_admitted_versions_read_through_one_catalog() {
    let mut c = setup().await;
    environments(&mut c, roles::ENVIRONMENT).await;
    let allowlist = json!({"schema_version":1,"entries":[{"name":"web","version":"1","image_digest":format!("sha256:{}", "e".repeat(64))}]});
    let mut config = (*c.env.env.state.config).clone();
    config.templates = Some(Arc::new(
        aq_environment_templates::Allowlist::parse(allowlist.to_string().as_bytes()).unwrap(),
    ));
    c.env.env.state.config = Arc::new(config);
    admit(&c, "example.web", "1", json!(["node", "python"])).await;
    admit(&c, "example.web", "2", json!(["node", "python", "rust"])).await;

    let state = &c.env.env.state;
    let catalog = aq_server::profiled_execution::definitions::catalog(state, &c.env.env.pool)
        .await
        .unwrap();
    let ids: Vec<(&str, &str)> = catalog.definitions().map(|(i, v, _)| (i, v)).collect();
    for id in [
        ("web", "1"),
        ("full", "1"),
        ("example.web", "1"),
        ("example.web", "2"),
    ] {
        assert!(ids.contains(&id), "{id:?} in {ids:?}");
    }
    let provided = |e| aq_project_policy::environments::provided(e, catalog.definitions());
    assert_eq!(provided("web"), ["node"]);
    assert_eq!(provided("example.web@2"), ["node", "python", "rust"]);
    // The boot allowlist still pins `web`; a bare admitted id is its newest version.
    assert_eq!(
        catalog.template("web").unwrap().digest.as_str(),
        "e".repeat(64)
    );
    let newest = catalog.template("example.web").unwrap();
    assert_eq!(
        (newest.version.as_str(), newest.digest.as_str()),
        ("2", &"d".repeat(64)[..])
    );
    assert_eq!(catalog.template("example.web@1").unwrap().version, "1");
    assert!(catalog.template("absent").is_none());
    assert_eq!(
        catalog.platform_text("example.web"),
        Some("Admitted platform.")
    );
    assert_eq!(catalog.platform_text("web"), None);
    // A version's definition body is retained by its exact reference.
    let (_, _, body) = (catalog.definitions())
        .find(|(i, v, _)| *i == "example.web" && *v == "1")
        .unwrap();
    let reference = aq_admission::ArtifactRef {
        id: aq_admission::Text::new("example.web").unwrap(),
        version: aq_admission::Text::new("1").unwrap(),
        digest: aq_admission::Digest::of(body.as_bytes()),
    };
    assert_eq!(catalog.definition(&reference), Some(body));
    // Nodes are told the admitted versions, not the boot allowlist.
    let nodes = serde_json::to_value(catalog.admitted_for_nodes()).unwrap();
    assert_eq!(nodes.as_array().unwrap().len(), 2, "{nodes}");
    // Each names the definition a node lists for it, which the catalog resolves.
    let mut first = nodes[0].clone();
    let definition = first.as_object_mut().unwrap().remove("definition").unwrap();
    let definition: aq_admission::ArtifactRef = serde_json::from_value(definition).unwrap();
    assert_eq!(
        (definition.id.as_str(), definition.version.as_str()),
        ("example.web", "2")
    );
    assert!(catalog.definition(&definition).is_some());
    assert_eq!(
        first,
        json!({"id":"example.web","version":"2","digest":TEMPLATE,"image":format!("registry.example/web@{TEMPLATE}"),"harnesses":[{"harness":"codex","version":"1"}],"warm":["pnpm install"]})
    );
}

/// A need only an admitted version provides holds the task until that
/// version is admitted; then placement chooses it (no node runs it yet).
#[tokio::test]
async fn an_admitted_version_provides_its_labels_to_placement() {
    let mut c = setup().await;
    environments(&mut c, roles::ENVIRONMENT).await;
    let env = &mut c.env.env;
    env.body["allowed_environments"] = json!([roles::ENVIRONMENT, "web", "full", "example.web"]);
    let body = json!({"operation_id":Uuid::new_v4(),"expected_revision":3,"binding":env.body});
    let (status, v) =
        selection::call(&env.app, &selection::path(), Some(body), "owner@localhost").await;
    assert_eq!(status, 200, "{v}");
    unpin(&c).await;
    needs(&c, &["python"]);
    let reason = refused(&mut c, "environment_missing_capability").await;
    assert_eq!(reason["missing"], json!(["python"]), "{reason}");

    admit(&c, "example.web", "1", json!(["node", "python"])).await;
    let reason = refused(&mut c, "no_node_has_environment").await;
    assert_eq!(reason["environment"], "example.web", "{reason}");
}

/// Spec 0017 §6.7: the environment limits are a Settings value whose
/// defaults are the documented numbers, and the owner's save replaces them.
#[tokio::test]
async fn environment_limits_default_to_the_documented_values() {
    let c = setup().await;
    let pool = &c.env.env.pool;
    let (limits, revision) = aq_role_settings::environment_limits(pool).await.unwrap();
    assert_eq!(revision, 0);
    assert_eq!(
        serde_json::to_value(limits).unwrap(),
        json!({"max_install_steps": 64, "max_step_args": 32, "max_arg_bytes": 4096, "max_prefetch": 16, "max_recipe_bytes": 1048576, "build_wall_ms": 3600000, "build_cpu_millis": 4000, "build_memory_bytes": 8589934592u64, "log_bytes": 65536, "try_concurrency": 2, "keep_versions": 3, "max_job_claims": 3})
    );
    let changed = aq_role_settings::environments::EnvironmentLimits {
        keep_versions: 5,
        ..limits
    };
    let receipt = aq_role_settings::save_environment_limits(
        pool,
        c.env.actor,
        &changed,
        0,
        Uuid::new_v4(),
        1,
    )
    .await
    .unwrap();
    let (stored, revision) = aq_role_settings::environment_limits(pool).await.unwrap();
    assert_eq!((stored, revision), (changed, receipt.revision));
    let zero = aq_role_settings::environments::EnvironmentLimits {
        try_concurrency: 0,
        ..changed
    };
    let refused = aq_role_settings::save_environment_limits(
        pool,
        c.env.actor,
        &zero,
        revision,
        Uuid::new_v4(),
        2,
    )
    .await;
    assert!(
        matches!(&refused, Err(aq_role_settings::Error::Invalid(m)) if m.starts_with("try_concurrency is 0")),
        "{refused:?}"
    );
}

/// The fence re-derives the template inside its own transaction: the catalog
/// is read on the connection it is given, so a version admitted in that
/// transaction (and not yet committed) is seen there and nowhere else.
#[tokio::test]
async fn the_template_is_read_through_the_callers_transaction() {
    let c = setup().await;
    let state = &c.env.env.state;
    let mut tx = c.env.env.pool.begin().await.unwrap();
    admit_on(&mut *tx, &c, "example.web", "1", json!(["node"])).await;
    use aq_server::profiled_execution::placement::template;
    let inside = template(state, &mut *tx, "example.web").await.unwrap();
    assert_eq!(inside.as_deref(), Some(&"d".repeat(64)[..]));
    let outside = template(state, &c.env.env.pool, "example.web")
        .await
        .unwrap();
    assert_eq!(
        outside, None,
        "uncommitted: another connection does not see it"
    );
    tx.rollback().await.unwrap();
}

// Task 11c: a node lists an admitted environment it reported ready, and the
// offer names that environment's definition and the result adapter derived
// for it from the node's boot docker-local Claude adapter.

const STARTUP_DIGEST: &str = "667d48a537baa35335fb3da1e589846e9d294a2bc24a8df58cf682181b15229a";
const CONTRACT_DIGEST: &str = "4fe711c405ead85b55a54de2d7446b2c106c4b8809b16bc57497ba2af1d35ef9";
/// The fields a derived Definition keeps from the boot one: the node refuses
/// anything else.
const KEPT: [&str; 9] = [
    "schema_version",
    "profile",
    "runtime",
    "client_manifest",
    "client_executable",
    "contract",
    "recipe",
    "permission_digest",
    "reserved_client_bytes",
];
fn reference(id: &str, digest: &str) -> aq_admission::ArtifactRef {
    serde_json::from_value(json!({"id": id, "version": "1", "digest": digest})).unwrap()
}
/// The fixture node's one harness runs with the docker-local Claude result
/// adapter: its canonical Definition (pinning the node's boot environment)
/// and the result startup instruction are in the boot definitions, and the
/// configured and installed harness name it. Returns the boot entry.
async fn docker_local(c: &mut Case) -> aq_selection::InstalledHarness {
    let profile = &aq_admission::DOCKER_LOCAL_PROFILES[0];
    let mut harness = c.env.input.node.harnesses[0].clone();
    let old = harness.adapter.clone();
    let mut body = json!({"schema_version": 1, "profile": profile.profile,
        "runtime": profiled::artifact("runtime-code", b"runtime"), "environment": harness.environment,
        "client_manifest": profiled::artifact("aq.agent-result-client-installation", b"manifest"),
        "client_executable": profiled::artifact("client", b"client"),
        "contract": reference("aq.agent-result-local", CONTRACT_DIGEST),
        "recipe": reference(profile.recipe_id, profile.recipe_digest.unwrap()),
        "permission_digest": aq_admission::Digest::of(b"plan"), "reserved_client_bytes": 67108864});
    body.sort_all_objects();
    let body = body.to_string();
    harness.adapter = profiled::artifact(profile.adapter, body.as_bytes());
    let env = &mut c.env.env;
    let mut config = (*env.state.config).clone();
    let mut profiled = (**config.profiled_execution.as_ref().unwrap()).clone();
    profiled.definition_bodies.retain(|d| *d.artifact() != old);
    let startup = include_str!("../../fixtures/agent-result-startup-v1.md");
    for (artifact, body) in [
        (harness.adapter.clone(), body),
        (
            reference("aq.agent-result-startup", STARTUP_DIGEST),
            startup.into(),
        ),
    ] {
        let definition = Definition::new(artifact, ArtifactKind::Definition, body).unwrap();
        profiled.definition_bodies.push(definition);
    }
    profiled.revision += 1;
    config.profiled_execution = Some(Arc::new(profiled));
    let mut intelligence = config.intelligence.as_ref().unwrap().config.clone();
    intelligence.revision += 1;
    intelligence.qualified_harnesses = vec![harness.clone()];
    config.intelligence = Some(Arc::new(intelligence.load().unwrap()));
    env.state.config = Arc::new(config);
    aq_server::profiled_execution::policies::install(&env.state)
        .await
        .unwrap();
    env.app = aq_server::build_admin_router(env.state.clone());
    let (status, body) = selection::call(
        &env.app,
        "/api/intelligence/register",
        Some(json!({})),
        "owner@localhost",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    c.env.input.node.harnesses = vec![harness.clone()];
    let node = &c.env.input.node;
    sqlx::query("UPDATE intelligence_node_snapshots SET snapshot=$2 WHERE node_id=$1")
        .bind(c.env.env.node)
        .bind(json!(node))
        .execute(&c.env.env.pool)
        .await
        .unwrap();
    harness
}
/// Admit `example.web` version `version`, whose template carries the
/// fixture's harness, as `environment.admit` will.
async fn admit_web(c: &Case, version: &str, harness: &aq_selection::InstalledHarness) {
    let carried = json!([{"harness": harness.harness, "version": harness.cli_version}]);
    sqlx::query(
        "INSERT INTO environment_versions(environment_id,version,project_id,template_digest,image,provides,harnesses,warm,admitted_at) \
         VALUES('example.web',$1,$2,$3,'registry.example/web@'||$3,'[]',$4,'[]',now())",
    )
    .bind(version)
    .bind(c.env.lineage.project_id)
    .bind(TEMPLATE)
    .bind(carried)
    .execute(&c.env.env.pool)
    .await
    .unwrap();
}
/// The node reports `digests` ready, as its poll does.
pub(crate) async fn ready(c: &Case, digests: Value) {
    let body = serde_json::to_vec(&json!({"ready": digests})).unwrap();
    let state = &c.env.env.state;
    let path = "/v1/worker/environments/ready";
    let response =
        crate::common::native_offer::post(state, &c.env.env.credential, path, body).await;
    assert_eq!(response.status(), 200);
}
/// The definition reference nodes are told for the admitted `example.web`.
pub(super) async fn definition_of(c: &Case) -> aq_admission::ArtifactRef {
    let catalog =
        aq_server::profiled_execution::definitions::catalog(&c.env.env.state, &c.env.env.pool)
            .await
            .unwrap();
    let nodes = serde_json::to_value(catalog.admitted_for_nodes()).unwrap();
    serde_json::from_value(nodes[0]["definition"].clone()).unwrap()
}
/// What the inventory writer stores for the node's installed `harnesses`.
pub(super) async fn republish(c: &Case) -> Vec<aq_selection::InstalledHarness> {
    let state = &c.env.env.state;
    let mut conn = c.env.env.pool.acquire().await.unwrap();
    let configured = c.env.input.node.harnesses.clone();
    let listed = aq_server::profiled_execution::definitions::listed(
        state,
        &mut conn,
        c.env.env.node,
        configured,
    )
    .await
    .unwrap();
    sqlx::query("UPDATE intelligence_node_snapshots SET snapshot=jsonb_set(snapshot,'{harnesses}',$2) WHERE node_id=$1")
        .bind(c.env.env.node)
        .bind(json!(listed))
        .execute(&c.env.env.pool)
        .await
        .unwrap();
    listed
}
/// The binding allows and defaults to `example.web`; the profile pins none.
pub(crate) async fn on_web(c: &mut Case) -> aq_selection::InstalledHarness {
    let boot = docker_local(c).await;
    environments(c, roles::ENVIRONMENT).await;
    let env = &mut c.env.env;
    env.body["allowed_environments"] = json!([roles::ENVIRONMENT, "example.web"]);
    env.body["default_environment"] = json!("example.web");
    let body = json!({"operation_id":Uuid::new_v4(),"expected_revision":3,"binding":env.body});
    let (status, v) =
        selection::call(&env.app, &selection::path(), Some(body), "owner@localhost").await;
    assert_eq!(status, 200, "{v}");
    unpin(c).await;
    admit_web(c, "1", &boot).await;
    boot
}
fn listed_now(c: &Case) -> impl std::future::Future<Output = bool> + '_ {
    let now = (c.env.env.state.clock)().timestamp_millis();
    aq_role_settings::environments::listed(&c.env.env.pool, "example.web", now)
}

/// Spec 0017 §6.3: the inventory writer lists an admitted environment for a
/// node only once the node has reported its template ready, and only from
/// its next inventory on.
#[tokio::test]
async fn a_node_lists_an_admitted_environment_only_after_reporting_it_ready() {
    let mut p = crate::common::presence::setup().await;
    let boot = docker_local(&mut p.c).await;
    admit_web(&p.c, "1", &boot).await;
    let state = p.c.env.env.state.clone();
    let mut h = axum::http::HeaderMap::new();
    let bearer = format!("Bearer {}", p.c.env.env.credential);
    h.insert("authorization", bearer.parse().unwrap());
    let auth = aq_server::worker_api::authenticate_context(&state, &h)
        .await
        .unwrap();
    let stored = || async {
        let snapshot: Value =
            sqlx::query_scalar("SELECT snapshot FROM intelligence_node_snapshots WHERE node_id=$1")
                .bind(p.c.env.env.node)
                .fetch_one(&p.c.env.env.pool)
                .await
                .unwrap();
        serde_json::from_value::<Vec<aq_selection::InstalledHarness>>(snapshot["harnesses"].clone())
            .unwrap()
    };
    use aq_server::profiled_execution::presence::inventory::record;
    record(&state, &auth, &p.request).await.unwrap();
    assert_eq!(stored().await.len(), 1, "nothing reported ready");
    assert!(!listed_now(&p.c).await);

    ready(&p.c, json!([TEMPLATE])).await;
    assert!(!listed_now(&p.c).await, "listed from the next inventory on");
    let mut next = p.request.clone();
    (next.operation_id, next.sequence) = (Uuid::new_v4(), next.sequence + 1);
    record(&state, &auth, &next).await.unwrap();
    let harnesses = stored().await;
    assert_eq!(harnesses.len(), 2, "{harnesses:?}");
    assert_eq!(harnesses[1].environment, definition_of(&p.c).await);
    assert_eq!(harnesses[1].adapter.id, boot.adapter.id);
    assert!(listed_now(&p.c).await);

    // A report without it takes it off the next inventory.
    ready(&p.c, json!([])).await;
    (next.operation_id, next.sequence) = (Uuid::new_v4(), next.sequence + 1);
    record(&state, &auth, &next).await.unwrap();
    assert_eq!(stored().await.len(), 1);
    assert!(!listed_now(&p.c).await);
}

/// The offer for an admitted environment carries its definition as
/// `harness.environment` and the derived adapter as `harness.adapter`, with
/// both bodies retained in the offer.
#[tokio::test]
async fn an_offer_for_an_admitted_environment_names_its_definition_and_adapter() {
    let mut c = setup().await;
    let boot = on_web(&mut c).await;
    ready(&c, json!([TEMPLATE])).await;
    let listed = republish(&c).await;
    c.request.operation_id = Uuid::new_v4();
    let (status, body) = poll_offered(&c).await;
    assert_eq!(status, 200, "{body}");
    let ControlReply::Poll(page) = decode_reply(&serde_json::to_vec(&body).unwrap()).unwrap()
    else {
        panic!("{body}")
    };
    assert_eq!(page.offers.len(), 1, "{body}");
    let offered = &page.offers[0].offered;
    assert_eq!(
        placed(&c, offered.offer.binding.attempt_id).await,
        "example.web"
    );
    let harness = &offered.offer.manifest.harness;
    assert_eq!(harness.environment, definition_of(&c).await);
    assert_eq!(harness.adapter, listed[1].adapter);
    assert_eq!(harness.adapter.id, boot.adapter.id);
    assert_eq!(
        (harness.harness.as_str(), harness.cli_version.as_str()),
        (boot.harness.as_str(), boot.cli_version.as_str())
    );
    for artifact in [&harness.adapter, &harness.environment] {
        let retained = offered.artifacts.iter().filter(|a| a.artifact == *artifact);
        assert_eq!(retained.count(), 1, "{artifact:?} retained once");
    }
}

/// The derived Definition is the boot one with only `environment` replaced
/// by the admitted definition: field by field, in canonical bytes.
#[tokio::test]
async fn the_derived_definition_differs_from_the_boot_one_only_in_environment() {
    let mut c = setup().await;
    let boot = on_web(&mut c).await;
    let state = &c.env.env.state;
    let catalog = aq_server::profiled_execution::definitions::catalog(state, &c.env.env.pool)
        .await
        .unwrap();
    let qualified = catalog.qualified(std::slice::from_ref(&boot));
    assert_eq!(qualified.len(), 2, "{qualified:?}");
    let derived = &qualified[1].adapter;
    let body = |r| serde_json::from_str::<Value>(catalog.definition(r).unwrap()).unwrap();
    let (old, new) = (body(&boot.adapter), body(derived));
    let keys = |v: &Value| v.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
    assert_eq!(keys(&old), keys(&new));
    for field in KEPT {
        assert_eq!(old[field], new[field], "{field}");
    }
    assert_eq!(
        keys(&old).len(),
        KEPT.len() + 1,
        "only `environment` besides"
    );
    assert_eq!(old["environment"], json!(boot.environment));
    assert_eq!(new["environment"], json!(definition_of(&c).await));
    // Canonical bytes (sorted keys, compact) under the boot id, at a revision.
    let bytes = catalog.definition(derived).unwrap();
    let mut canonical = new.clone();
    canonical.sort_all_objects();
    assert_eq!(bytes, canonical.to_string());
    assert_eq!(derived.digest, aq_admission::Digest::of(bytes.as_bytes()));
    assert_eq!(derived.id, boot.adapter.id);
    assert_eq!(derived.version.as_str(), "1+example.web@1");
}

/// The admission fence accepts the offer, and the renewal and phase fences'
/// qualified harnesses hold the entry it chose: a counterpart of the
/// configured qualified harness for an admitted environment.
#[tokio::test]
async fn the_fence_accepts_an_offer_for_an_admitted_environment() {
    let mut c = setup().await;
    on_web(&mut c).await;
    ready(&c, json!([TEMPLATE])).await;
    republish(&c).await;
    let attempt = offer(&mut c).await;
    let choice: Value = sqlx::query_scalar(
        "SELECT d.decision#>'{outcome,resolved,choice}' FROM profiled_launches l JOIN intelligence_decisions d USING(decision_id) WHERE l.attempt_id=$1",
    )
    .bind(attempt)
    .fetch_one(&c.env.env.pool)
    .await
    .unwrap();
    assert_eq!(
        choice["environment"],
        json!(definition_of(&c).await),
        "{choice}"
    );
    let qualified = || async {
        let state = &c.env.env.state;
        let configured = state
            .config
            .intelligence
            .as_ref()
            .unwrap()
            .config
            .qualified_harnesses
            .clone();
        let mut conn = c.env.env.pool.acquire().await.unwrap();
        aq_server::profiled_execution::definitions::qualified(state, &mut conn, &configured)
            .await
            .unwrap()
    };
    let chosen = |h: &aq_selection::InstalledHarness| {
        json!(h.adapter) == choice["adapter"]
            && json!(h.environment) == choice["environment"]
            && json!(h.qualification) == choice["qualification"]
    };
    assert!(qualified().await.iter().any(chosen));
    sqlx::query("DELETE FROM environment_versions")
        .execute(&c.env.env.pool)
        .await
        .unwrap();
    assert!(
        !qualified().await.iter().any(chosen),
        "not once its version is gone"
    );
}

/// Spec 0017 §6.7: a pruned version takes its entries and derived Definition
/// with it: the node no longer lists it and the task waits for a node.
#[tokio::test]
async fn a_pruned_environment_is_no_longer_listed_or_offered() {
    let mut c = setup().await;
    on_web(&mut c).await;
    ready(&c, json!([TEMPLATE])).await;
    let derived = republish(&c).await[1].adapter.clone();
    assert!(listed_now(&c).await);
    sqlx::query("DELETE FROM environment_versions")
        .execute(&c.env.env.pool)
        .await
        .unwrap();
    assert_eq!(republish(&c).await.len(), 1);
    assert!(!listed_now(&c).await);
    let state = &c.env.env.state;
    let catalog = aq_server::profiled_execution::definitions::catalog(state, &c.env.env.pool)
        .await
        .unwrap();
    assert!(catalog.definition(&derived).is_none());
    let reason = refused(&mut c, "no_node_has_environment").await;
    assert_eq!(reason["environment"], "example.web", "{reason}");
}

/// Requalifying a ticket after environment admission uses the current binding
/// and a worker that has pulled its image. The ticket keeps the exact branch
/// checkpoint that its stopped attempt already pushed.
#[cfg(feature = "delivery_playbook")]
#[tokio::test]
async fn a_fresh_retry_in_a_newly_admitted_environment_keeps_the_task_branch_lease() {
    use super::git_grant::granted_offer;
    use super::holds::{owner_actions, retry_with};
    use crate::common::native_offer::fail;
    use aq_delivery::git_grant::fake::{FakeGitHttp, git};

    let mut c = setup().await;
    let repository = c.env.frozen.project.binding.repository_id;
    let git_root = c
        .env
        .env
        .state
        .source_retention
        .as_ref()
        .unwrap()
        .config
        .git_root
        .clone();
    let fake = FakeGitHttp::start(&git_root, repository, "refs/heads/main").await;
    let (original, grant) = granted_offer(&mut c, &fake).await;
    let original = original.offered.lifecycle_binding;
    assert_eq!(placed(&c, original.attempt_id).await, roles::ENVIRONMENT);
    assert!(grant.lease.is_none());
    let tree = git(&fake.repository, &["rev-parse", "HEAD^{tree}"]);
    let checkpoint = git(
        &fake.repository,
        &["commit-tree", &tree, "-p", &fake.base, "-m", "saved work"],
    );
    git(
        &fake.repository,
        &["update-ref", &grant.task_ref, &checkpoint],
    );
    fail(&c, original.attempt_id).await;

    on_web(&mut c).await;
    owner_actions(&mut c).await;
    let (status, body) = retry_with(&c, Uuid::new_v4(), Some("qualified environment"), false).await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["code"], "replacement_project_revision_changed");
    let (status, body) = retry_with(&c, Uuid::new_v4(), Some("qualified environment"), true).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["data"]["disposition"], "confirmed");

    refused(&mut c, "no_node_has_environment").await;
    ready(&c, json!([TEMPLATE])).await;
    republish(&c).await;
    c.request.operation_id = Uuid::new_v4();
    let (successor, next) = granted_offer(&mut c, &fake).await;
    let successor_binding = successor.offered.lifecycle_binding;
    assert_eq!(successor_binding.task_id, original.task_id);
    assert_ne!(successor_binding.attempt_id, original.attempt_id);
    assert_eq!(
        successor.offered.offer.manifest.harness.environment,
        definition_of(&c).await
    );
    assert_eq!(
        (next.repository_id, next.task_ref),
        (repository, grant.task_ref)
    );
    assert_eq!(next.lease.as_deref(), Some(checkpoint.as_str()));
}
