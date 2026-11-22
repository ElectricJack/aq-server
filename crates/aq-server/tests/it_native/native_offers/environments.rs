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

const TEMPLATE: &str = "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
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
    assert_eq!(
        nodes[0],
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
        json!({"max_install_steps": 64, "max_step_args": 32, "max_arg_bytes": 4096, "max_prefetch": 16, "max_recipe_bytes": 1048576, "build_wall_ms": 3600000, "build_cpu_millis": 4000, "build_memory_bytes": 8589934592u64, "log_bytes": 65536, "try_concurrency": 2, "keep_versions": 3})
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
