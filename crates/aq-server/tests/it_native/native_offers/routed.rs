//! Spec 0011 §4: placement honors a task's route. Under a binding whose agent runs on the
//! automatic router, a routed task is offered on its class's assigned profile within the
//! route's limits, an unrouted one waits while routing is on, and a route that cannot be
//! placed is cleared so the task is routed again; it never runs on the router instead. A
//! binding pinned to a named profile ignores routes. The actual mounted poll, selection and
//! publication run; the Graph peer, the routing switch and the Intelligence structure are
//! synthetic owner facts (no routing policy is compiled and no provider is called).
use crate::common::native_offer::{Case, fail_and_release, poll, poll_offered, setup};
use aq_admission::control::{ControlReply, decode_reply};
use serde_json::{Value, json};
use std::sync::{Arc, atomic::Ordering};
use uuid::Uuid;

const ROUTER: &str = "engineering@3";
const CLASS_PROFILE: &str = "standard-high@1";

/// The class profile's own model for fixture-a; the router's candidates do not have it.
const CLASS_MODEL: &str = "model-c";

/// Activates a catalog whose agents run on an automatic router (`engineering@3`) and that
/// has a class profile `standard-high@1` whose fixture-a candidate is its own model
/// ([`CLASS_MODEL`], installed on the node and context-qualified here) with unknown
/// availability allowed, assigns that profile to the standard parent at high effort, and
/// binds the project to the router.
async fn on_the_router(c: &mut Case) {
    let env = &mut c.env.env;
    let original: Vec<u8> =
        sqlx::query_scalar("SELECT artifact FROM profile_catalog_artifacts WHERE digest=$1")
            .bind(env.body["catalog_digest"].as_str().unwrap())
            .fetch_one(&env.pool)
            .await
            .unwrap();
    let catalog = aq_profiles::ValidatedCatalog::from_artifact(&original).unwrap();
    let mut artifact = catalog.artifact().clone();
    let profiles = &mut artifact.catalog.intelligence;
    assert_eq!(
        profiles.len(),
        1,
        "the fixture has one intelligence profile"
    );
    let mut class = profiles[0].clone();
    profiles[0].version = 3;
    profiles[0].automatic_routing = Some(true);
    (class.id, class.version, class.automatic_routing) = ("standard-high".into(), 1, None);
    assert_eq!(class.candidates[0].provider, "fixture-a");
    class.candidates[0].model = CLASS_MODEL.into();
    class
        .selection_policy
        .as_mut()
        .unwrap()
        .allow_unknown_availability = true;
    profiles.push(class);
    for agent in &mut artifact.catalog.agents {
        agent.version = 3;
        agent.intelligence = ROUTER.into();
    }
    let updated = aq_profiles::validate(artifact.catalog, artifact.instructions).unwrap();
    let app = aq_server::build_admin_router(env.state.clone());
    let activate = json!({"operation_id":Uuid::new_v4(),"expected_revision":2,
        "artifact_json":String::from_utf8(updated.canonical_bytes().unwrap()).unwrap()});
    let path = "/api/profiles/catalogs/activate";
    let (status, body) =
        crate::common::selection::call(&app, path, Some(activate), "owner@localhost").await;
    assert_eq!(status, 200, "{body}");
    env.body["catalog_digest"] = json!(updated.digest());
    env.body["allowed_agent_profiles"] = json!(["reviewer@3"]);
    env.body["allowed_intelligence_profiles"] = json!([ROUTER]);
    let bind = json!({"operation_id":Uuid::new_v4(),"expected_revision":2,"binding":env.body});
    let path = crate::common::selection::path();
    let (status, body) =
        crate::common::selection::call(&app, &path, Some(bind), "owner@localhost").await;
    assert_eq!(status, 200, "{body}");
    let (mut document, _, _) = aq_profile_settings::structure::document(&env.pool)
        .await
        .unwrap();
    document.assignments = vec![aq_profile_settings::structure::Assignment {
        profile_id: "standard-high".into(),
        parent_id: "standard".into(),
        effort: "high".into(),
    }];
    sqlx::query(
        "UPDATE intelligence_structure SET revision=revision+1,document=$1 WHERE singleton",
    )
    .bind(serde_json::to_value(&document).unwrap())
    .execute(&env.pool)
    .await
    .unwrap();
    let model = json!([{"provider":"fixture-a","model":CLASS_MODEL,
        "reasoning_configuration_reference":"reasoning-a@1","native_reasoning":"high"}]);
    sqlx::query("UPDATE intelligence_node_snapshots SET snapshot=jsonb_set(snapshot,'{harnesses,0,models}',(snapshot#>'{harnesses,0,models}')||$1::jsonb)")
        .bind(model)
        .execute(&env.pool)
        .await
        .unwrap();
    let mut config = (*env.state.config).clone();
    let mut profiled = (**config.profiled_execution.as_ref().unwrap()).clone();
    let mut fact = profiled.context_qualifications[0].clone();
    assert_eq!(fact.subject.provider, "fixture-a");
    fact.subject.model = CLASS_MODEL.into();
    profiled.context_qualifications.push(fact);
    profiled.revision += 1;
    config.profiled_execution = Some(Arc::new(profiled));
    env.state.config = Arc::new(config);
    aq_server::profiled_execution::policies::install(&env.state)
        .await
        .unwrap();
}
/// The installation's routing switch, written directly (no compiled policy is needed here).
async fn routing(c: &Case, on: bool) {
    sqlx::query("UPDATE routing_state SET enabled=$1 WHERE singleton")
        .bind(on)
        .execute(&c.env.env.pool)
        .await
        .unwrap();
}
/// Puts a router route of `class` with harness allowlist `harnesses` on the task.
fn route(c: &Case, class: &str, harnesses: &[&str]) {
    let route = json!({"c":class,"h":harnesses,"p":[],"pf":[],"l":null,"i":"any","d":null,"s":"router","n":1});
    *c.metadata.lock().unwrap() = json!({"aq:route": route});
}
/// Puts a pinned router route of `class` on the task.
fn pinned_route(c: &Case, class: &str) {
    let route =
        json!({"c":class,"h":[],"p":[],"pf":[],"l":null,"i":"pinned","d":null,"s":"router","n":1});
    *c.metadata.lock().unwrap() = json!({"aq:route": route});
}
fn stored_route(c: &Case) -> Option<Value> {
    c.metadata.lock().unwrap().get("aq:route").cloned()
}
/// The offered manifests' intelligence profile identities.
fn offered(body: &Value) -> Vec<String> {
    let ControlReply::Poll(page) = decode_reply(&serde_json::to_vec(body).unwrap()).unwrap() else {
        panic!("{body}")
    };
    (page.offers.iter())
        .map(|o| {
            o.offered
                .offer
                .manifest
                .intelligence_profile
                .id
                .as_str()
                .to_owned()
        })
        .collect()
}
/// Polls until an offer arrives (a refused poll fails the test) and returns its task.
async fn offered_task(c: &Case) -> Uuid {
    let (status, body) = poll_offered(c).await;
    assert_eq!(status, 200, "{body}");
    let ControlReply::Poll(page) = decode_reply(&serde_json::to_vec(&body).unwrap()).unwrap()
    else {
        panic!("{body}")
    };
    assert_eq!(page.offers.len(), 1, "{body}");
    page.offers[0].offered.offer.manifest.bindings.task_id
}
/// Polls until an offer arrives and returns its intelligence profile identity.
async fn offered_on(c: &Case) -> String {
    let (status, body) = poll_offered(c).await;
    assert_eq!(status, 200, "{body}");
    let mut offered = offered(&body);
    assert_eq!(offered.len(), 1, "{body}");
    offered.remove(0)
}
/// Several polls, a few hundred milliseconds apart, none of which offers the task; the
/// number of them that were refused (a selection that refuses a first attempt fails the
/// poll, as for any task no candidate can serve).
async fn not_offered(c: &Case) -> usize {
    let mut refused = 0;
    for _ in 0..4 {
        let (status, body) = poll(c).await;
        if status == 409 && body["body"]["code"] == "conflict" {
            refused += 1;
        } else {
            assert_eq!(status, 200, "{body}");
            assert!(offered(&body).is_empty(), "{body}");
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    assert_eq!(attempts(c).await, 0, "no attempt was published");
    refused
}
async fn attempts(c: &Case) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM execution_attempts WHERE task_id=$1")
        .bind(c.env.lineage.task_id)
        .fetch_one(&c.env.env.pool)
        .await
        .unwrap()
}
/// The intelligence profile and override the task's latest selection decision used.
async fn decided(c: &Case) -> (Value, Option<Value>) {
    sqlx::query_as(
        "SELECT decision#>'{inputs,project,intelligence_profile}',decision#>'{inputs,intelligence_override}' \
         FROM intelligence_decisions WHERE decision#>>'{inputs,project,lineage,task_id}'=$1::TEXT \
         ORDER BY decision#>'{inputs,now_ms}' DESC LIMIT 1",
    )
    .bind(c.env.lineage.task_id)
    .fetch_one(&c.env.env.pool)
    .await
    .unwrap()
}
async fn history(c: &Case) -> Vec<Option<aq_routing::Route>> {
    let pool = &c.env.env.pool;
    let rows = aq_server::routing::history(pool, c.env.lineage.task_id)
        .await
        .unwrap();
    rows.into_iter().map(|r| r.route).collect()
}
async fn skips(c: &Case) -> i64 {
    sqlx::query_scalar(
        "SELECT COALESCE(SUM(skips),0)::BIGINT FROM successor_skips WHERE task_id=$1",
    )
    .bind(c.env.lineage.task_id)
    .fetch_one(&c.env.env.pool)
    .await
    .unwrap()
}
async fn holds(c: &Case) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM task_holds WHERE task_id=$1")
        .bind(c.env.lineage.task_id)
        .fetch_one(&c.env.env.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn routed_task_is_offered_on_the_class_profile_not_the_router() {
    let mut c = setup().await;
    on_the_router(&mut c).await;
    routing(&c, true).await;
    route(&c, "standard-high", &[]);
    assert_eq!(
        offered_on(&c).await,
        format!("intelligence:{CLASS_PROFILE}")
    );
    // Selection ran on the class profile; the frozen binding still names the router.
    assert_eq!(
        decided(&c).await,
        (json!(ROUTER), Some(json!(CLASS_PROFILE)))
    );
    assert!(stored_route(&c).is_some(), "a placed route is kept");
}

#[tokio::test]
async fn unrouted_task_is_not_offered_when_routing_is_on_and_is_offered_when_off() {
    let mut c = setup().await;
    on_the_router(&mut c).await;
    routing(&c, true).await;
    assert_eq!(not_offered(&c).await, 0, "a wait is not a refusal");
    assert!(history(&c).await.is_empty(), "waiting writes no route");
    assert_eq!(holds(&c).await, 0, "waiting is not a hold");
    // The wait is recorded once for the playbook owner, which asks for a route from it.
    let wanted: Vec<(i64, i64, i64, Uuid, Uuid)> = sqlx::query_as(
        "SELECT wanted_version,task_version,emitted_at_ms,workspace_id,project_id FROM route_needed_emissions WHERE task_id=$1",
    )
    .bind(c.env.lineage.task_id)
    .fetch_all(&c.env.env.pool)
    .await
    .unwrap();
    let l = &c.env.lineage;
    assert_eq!(wanted.len(), 1);
    assert!(wanted[0].0 >= 1, "the task version that waits");
    assert_eq!(wanted[0].1..=wanted[0].2, 0..=0, "nothing asked yet");
    assert_eq!((wanted[0].3, wanted[0].4), (l.workspace_id, l.project_id));
    // Off, placement is as before: the router's own profile.
    routing(&c, false).await;
    assert_eq!(offered_on(&c).await, format!("intelligence:{ROUTER}"));
    assert_eq!(decided(&c).await, (json!(ROUTER), None));
}

#[tokio::test]
async fn route_is_ignored_for_a_binding_pinned_to_a_named_profile() {
    let c = setup().await;
    routing(&c, true).await;
    // Unassigned and excluding every harness: honored, it would refuse the task.
    route(&c, "deep-max", &["no-such-harness"]);
    assert_eq!(offered_on(&c).await, "intelligence:engineering@2");
    assert_eq!(decided(&c).await, (json!("engineering@2"), None));
    assert!(stored_route(&c).is_some(), "the route is left alone");
    assert!(history(&c).await.is_empty());
    // And without a route: placed as before, and no route is asked for.
    assert_eq!(waiting(&c).await, 0);
}

/// Codex review 1: a task that waits at the head of the ready list does not hide the ready
/// tasks behind it. When the head page offers nothing, each poll also reads one further page.
#[tokio::test]
async fn a_waiting_task_at_the_head_does_not_hide_the_ready_tasks_behind_it() {
    let mut c = setup().await;
    on_the_router(&mut c).await;
    routing(&c, true).await;
    c.mode.store(11, Ordering::SeqCst);
    assert_eq!(not_offered(&c).await, 0);
    let wanted: Vec<Uuid> = sqlx::query_scalar(
        "SELECT task_id FROM route_needed_emissions ORDER BY wanted_at_ms,task_id",
    )
    .fetch_all(&c.env.env.pool)
    .await
    .unwrap();
    assert_eq!(
        wanted.len(),
        2,
        "the task on the second page waits for a route too"
    );
    assert!(wanted.contains(&c.env.lineage.task_id) && wanted.contains(&c.second_task));
}

/// Tasks recorded as waiting for a route.
async fn waiting(c: &Case) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM route_needed_emissions")
        .fetch_one(&c.env.env.pool)
        .await
        .unwrap()
}

/// A router route is for the attempts the task had when it was written. After a later
/// attempt ended, the task is routed again: a failure may change the level.
#[tokio::test]
async fn a_task_retried_after_routing_is_routed_again() {
    let mut c = setup().await;
    on_the_router(&mut c).await;
    routing(&c, true).await;
    route(&c, "standard-high", &[]);
    let (status, body) = poll_offered(&c).await;
    assert_eq!(status, 200, "{body}");
    let ControlReply::Poll(mut page) = decode_reply(&serde_json::to_vec(&body).unwrap()).unwrap()
    else {
        panic!("{body}")
    };
    assert_eq!(waiting(&c).await, 0, "a routed task does not wait");
    fail_and_release(&c, page.offers.remove(0).offered.offer.binding.attempt_id).await;
    // The route was written before that attempt ended (`a` 0 < 1 ended attempt).
    for _ in 0..3 {
        let (status, body) = poll(&c).await;
        assert_eq!(status, 200, "{body}");
        assert!(offered(&body).is_empty(), "{body}");
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    assert_eq!(attempts(&c).await, 1, "not offered on the outdated route");
    assert_eq!(stored_route(&c), None, "the outdated route is cleared");
    assert_eq!(history(&c).await, [None]);
    assert_eq!(
        (skips(&c).await, holds(&c).await),
        (0, 0),
        "a wait, not a refusal"
    );
    assert_eq!(waiting(&c).await, 1, "and a route is asked for");
    // A route written after that attempt ended (`a` 1) is placed.
    let again = json!({"c":"standard-high","h":[],"p":[],"pf":[],"l":null,"i":"any","d":null,"s":"router","n":2,"a":1});
    *c.metadata.lock().unwrap() = json!({"aq:route": again});
    let (status, body) = poll_offered(&c).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(attempts(&c).await, 2);
    // Codex review 6: placement saw it routed, so it no longer waits for a route.
    let still: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM route_needed_emissions WHERE wanted_at_ms IS NOT NULL",
    )
    .fetch_one(&c.env.env.pool)
    .await
    .unwrap();
    assert_eq!(still, 0);
}

#[tokio::test]
async fn route_whose_class_lost_its_profile_is_cleared_and_not_launched_on_the_router() {
    let mut c = setup().await;
    on_the_router(&mut c).await;
    routing(&c, true).await;
    // A route that cannot be read is cleared, keeping the task's other metadata.
    *c.metadata.lock().unwrap() = json!({"aq:route": {"c": "fast-high"}, "aq:priority": 1});
    assert_eq!(not_offered(&c).await, 0, "a route to redo is not a refusal");
    assert_eq!(*c.metadata.lock().unwrap(), json!({"aq:priority": 1}));
    // No profile is assigned to deep-max. The task never had an attempt: no refusal is recorded.
    route(&c, "deep-max", &[]);
    assert_eq!(not_offered(&c).await, 0, "a route to redo is not a refusal");
    assert_eq!(
        stored_route(&c),
        None,
        "cleared, so the router routes it again"
    );
    assert_eq!(
        history(&c).await,
        [None, None],
        "each clear is in the route history"
    );
    assert_eq!((skips(&c).await, holds(&c).await), (0, 0));

    // A task that has had an attempt: the refusal is recorded against that attempt.
    route(&c, "standard-high", &[]);
    let (status, body) = poll_offered(&c).await;
    assert_eq!(status, 200, "{body}");
    let ControlReply::Poll(mut page) = decode_reply(&serde_json::to_vec(&body).unwrap()).unwrap()
    else {
        panic!("{body}")
    };
    fail_and_release(&c, page.offers.remove(0).offered.offer.binding.attempt_id).await;
    // Routed after that attempt ended (`a` 1), to a class without a profile.
    let unassigned = json!({"c":"deep-max","h":[],"p":[],"pf":[],"l":null,"i":"any","d":null,"s":"router","n":1,"a":1});
    *c.metadata.lock().unwrap() = json!({"aq:route": unassigned});
    for _ in 0..4 {
        let (status, body) = poll(&c).await;
        assert_eq!(status, 200, "{body}");
        assert!(offered(&body).is_empty(), "{body}");
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    assert_eq!(attempts(&c).await, 1, "no attempt on the router");
    assert_eq!(stored_route(&c), None);
    assert_eq!(
        skips(&c).await,
        1,
        "route_class_unassigned counted against the attempt"
    );
    assert_eq!(holds(&c).await, 0, "a route to redo is not a hold");
}

#[tokio::test]
async fn route_excluding_every_candidate_is_refused_and_no_offer_is_published() {
    let mut c = setup().await;
    on_the_router(&mut c).await;
    routing(&c, true).await;
    route(&c, "standard-high", &["no-such-harness"]);
    // Selection refuses it for a route reason: skipped for the poll, not a failed poll.
    assert_eq!(not_offered(&c).await, 0);
    assert!(
        stored_route(&c).is_some(),
        "a refused route is not rerouted"
    );
    // Positive control: allowing the installed harness places it.
    let harness = c.env.input.node.harnesses[0].harness.clone();
    route(&c, "standard-high", &[harness.as_str()]);
    assert_eq!(
        offered_on(&c).await,
        format!("intelligence:{CLASS_PROFILE}")
    );
}

/// Review finding 2: a running routed attempt is counted in its class profile's capacity
/// cell, not the router's, so a cell limit holds for routed work. The class profile's model
/// is not among the router's candidates.
#[tokio::test]
async fn a_running_routed_attempt_occupies_its_class_profiles_cell_not_the_routers() {
    use aq_swarm_settings::admission::{Cell, check};
    let mut c = setup().await;
    on_the_router(&mut c).await;
    routing(&c, true).await;
    let pool = c.env.env.pool.clone();
    let cell = |class: &str| Cell {
        provider: "fixture-a".into(),
        intelligence_class: class.into(),
    };
    let limits = json!({"cell_limits":[
        {"provider":"fixture-a","intelligence_class":"standard-high","limit":1},
        {"provider":"fixture-a","intelligence_class":"engineering","limit":1}]});
    sqlx::query("UPDATE swarm_settings SET limits=$1 WHERE singleton")
        .bind(limits)
        .execute(&pool)
        .await
        .unwrap();
    route(&c, "standard-high", &[]);
    assert_eq!(
        offered_on(&c).await,
        format!("intelligence:{CLASS_PROFILE}")
    );
    let mut conn = pool.acquire().await.unwrap();
    // The admission check the offer path makes for the next task of each cell.
    let routed = check(&mut conn, Some(&cell("standard-high")))
        .await
        .unwrap();
    assert!(
        routed.is_some(),
        "a second routed task of the class is not admitted"
    );
    let unrouted = check(&mut conn, Some(&cell("engineering"))).await.unwrap();
    assert_eq!(unrouted, None, "an unrouted task under the router still is");
}

/// Review finding 3: a routed first attempt this node cannot serve (its route allows only a
/// harness the node lacks) is skipped for the poll; the project's next ready task is offered
/// instead of the poll failing.
#[tokio::test]
async fn a_routed_task_this_node_cannot_serve_does_not_block_its_project() {
    let mut c = setup().await;
    on_the_router(&mut c).await;
    c.mode.store(7, Ordering::SeqCst);
    route(&c, "standard-high", &["claude-code"]);
    assert_eq!(offered_task(&c).await, c.second_task);
    assert!(
        stored_route(&c).is_some(),
        "the route is valid, so it is kept"
    );
    assert_eq!(holds(&c).await, 0);
}

/// Review finding 3: a pinned route whose provider is unavailable here refuses fallback; the
/// task is skipped for the poll and the project's next ready task is offered.
#[tokio::test]
async fn a_pinned_route_whose_provider_is_unavailable_does_not_fail_the_poll() {
    let mut c = setup().await;
    on_the_router(&mut c).await;
    let mut scope = c.env.input.observations[0].scope.clone();
    scope.model = Some(CLASS_MODEL.into());
    let revision: i64 =
        sqlx::query_scalar("SELECT revision FROM intelligence_registry WHERE singleton")
            .fetch_one(&c.env.env.pool)
            .await
            .unwrap();
    let observation = json!({"operation_id":Uuid::new_v4(),"expected_revision":revision,"sequence":1,
        "scope":scope,"state":"quota_unavailable","ttl_ms":30_000,"cooldown_ms":20_000});
    let app = aq_server::build_admin_router(c.env.env.state.clone());
    let path = "/api/intelligence/observations";
    let (status, body) =
        crate::common::selection::call(&app, path, Some(observation), "owner@localhost").await;
    assert_eq!(status, 200, "{body}");
    c.mode.store(7, Ordering::SeqCst);
    pinned_route(&c, "standard-high");
    assert_eq!(offered_task(&c).await, c.second_task);
    assert!(
        stored_route(&c).is_some(),
        "the route is valid, so it is kept"
    );
    assert_eq!(holds(&c).await, 0);
}
