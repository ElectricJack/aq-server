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
