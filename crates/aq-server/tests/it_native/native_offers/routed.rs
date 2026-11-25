//! Spec 0011 §4 and spec 0014 §5.2: placement honors a task's route. A route names its
//! level by class id, so a routed task is offered on the level document of that id within
//! the route's limits; an unrouted one waits while routing is on and otherwise runs on the
//! project's default level; a route that cannot be read is cleared so the task is routed
//! again, and a class with no level document holds the task. The actual mounted poll,
//! selection and publication run; the Graph peer and the routing switch are synthetic owner
//! facts (no routing policy is compiled and no provider is called).
use crate::common::native_offer::{Case, assembled, fail_and_release, poll, poll_offered, setup};
use crate::common::roles;
use aq_admission::control::{ControlReply, decode_reply};
use aq_profiles::roles::Kind;
use serde_json::{Value, json};
use std::sync::atomic::Ordering;
use uuid::Uuid;

/// The level the routed tasks of these tests run on.
const LEVEL: &str = "intelligence:standard-high";
/// The level of an unrouted task: the project's default.
const DEFAULT: &str = "intelligence:standard-medium";

/// The routed level's own model; no other level of the fixture names it.
const CLASS_MODEL: &str = "model-c";

/// Saves the level document `standard-high` with its own model ([`CLASS_MODEL`]) and
/// unknown availability allowed, as the owner would in Settings.
async fn on_the_router(c: &mut Case) {
    let level = roles::open_level("standard-high", &[("fixture-a", CLASS_MODEL, "high")]);
    let pool = &c.env.env.pool;
    roles::save(
        pool,
        c.env.actor,
        Kind::Intelligence,
        "standard-high",
        &level,
    )
    .await;
    c.env.snapshot = roles::active(pool).await;
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
        // What the assembly records lands when it ends, not when the poll answers.
        assembled(c).await;
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
async fn routed_task_is_offered_on_the_level_document_of_its_class() {
    let mut c = setup().await;
    on_the_router(&mut c).await;
    routing(&c, true).await;
    route(&c, "standard-high", &[]);
    assert_eq!(offered_on(&c).await, LEVEL);
    // The decision names the level itself: nothing stands in for it.
    assert_eq!(decided(&c).await, (json!("standard-high"), None));
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
    // Off, an unrouted task runs on the project's default level.
    routing(&c, false).await;
    assert_eq!(offered_on(&c).await, DEFAULT);
    assert_eq!(decided(&c).await, (json!("standard-medium"), None));
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
async fn an_unreadable_route_is_cleared_and_a_class_with_no_level_document_holds_the_task() {
    let mut c = setup().await;
    on_the_router(&mut c).await;
    routing(&c, true).await;
    // A route that cannot be read is cleared, keeping the task's other metadata.
    *c.metadata.lock().unwrap() = json!({"aq:route": {"c": "simple-wrong"}, "aq:priority": 1});
    assert_eq!(not_offered(&c).await, 0, "a route to redo is not a refusal");
    assert_eq!(*c.metadata.lock().unwrap(), json!({"aq:priority": 1}));
    assert_eq!(
        history(&c).await,
        [None],
        "the clear is in the route history"
    );
    // `deep-max` is a class the router may name, and no level document has that id: the
    // task is held with the reason. It is not routed again and not run on another level.
    route(&c, "deep-max", &[]);
    assert_eq!(not_offered(&c).await, 0, "a hold is not a refusal");
    assert!(stored_route(&c).is_some(), "the route is left alone");
    assert_eq!(history(&c).await, [None]);
    assert_eq!((skips(&c).await, holds(&c).await), (0, 0), "read live");
    let readiness = super::holds::work(&c, c.env.lineage.task_id).await;
    assert_eq!(readiness["state"], "held", "{readiness}");
    let reason = &readiness["reasons"][0];
    assert_eq!(reason["code"], "level_missing", "{readiness}");
    assert_eq!(reason["level"], "deep-max");
    assert_eq!(
        reason["message"],
        "AQ does not offer this task: it was routed to `deep-max`, and no intelligence profile `deep-max` exists. Add that intelligence profile in Settings or route the task again."
    );
    // Once the level exists the task is offered on it.
    let level = roles::level("deep-max", &[("fixture-a", "model-a", "high")]);
    let pool = &c.env.env.pool;
    roles::save(pool, c.env.actor, Kind::Intelligence, "deep-max", &level).await;
    assert_eq!(offered_on(&c).await, "intelligence:deep-max");
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
    assert_eq!(offered_on(&c).await, LEVEL);
}

/// Review finding 2: a running routed attempt is counted in its class profile's capacity
/// cell, not the router's, so a cell limit holds for routed work. The class profile's model
/// is not among the router's candidates.
#[tokio::test]
async fn a_running_routed_attempt_occupies_the_cell_of_its_level() {
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
        {"provider":"fixture-a","intelligence_class":"standard-medium","limit":1}]});
    sqlx::query("UPDATE swarm_settings SET limits=$1 WHERE singleton")
        .bind(limits)
        .execute(&pool)
        .await
        .unwrap();
    route(&c, "standard-high", &[]);
    assert_eq!(offered_on(&c).await, LEVEL);
    let mut conn = pool.acquire().await.unwrap();
    // The admission check the offer path makes for the next task of each cell.
    let routed = check(&mut conn, Some(&cell("standard-high")))
        .await
        .unwrap();
    assert!(
        routed.is_some(),
        "a second routed task of the class is not admitted"
    );
    let unrouted = check(&mut conn, Some(&cell("standard-medium")))
        .await
        .unwrap();
    assert_eq!(unrouted, None, "a task on another level still is");
}

/// An attempt recorded before roles and profiles has no launches in its decision. It still
/// holds capacity: it is counted, unclassified, and nothing of an old catalog is read.
#[tokio::test]
async fn an_attempt_recorded_before_roles_is_counted_unclassified() {
    use aq_swarm_settings::admission::counts;
    let mut c = setup().await;
    on_the_router(&mut c).await;
    routing(&c, true).await;
    let pool = c.env.env.pool.clone();
    route(&c, "standard-high", &[]);
    assert_eq!(offered_on(&c).await, LEVEL);
    let mut tx = pool.begin().await.unwrap();
    let running = |all: Vec<aq_swarm_settings::admission::Count>| -> Vec<(String, u32)> {
        (all.into_iter().filter(|n| n.running > 0))
            .map(|n| (n.intelligence_class, n.running))
            .collect()
    };
    assert_eq!(
        running(counts(&mut tx).await.unwrap()),
        [("standard-high".to_owned(), 1)]
    );
    // The decision as an attempt from before roles recorded it: it names a
    // catalog, not a role snapshot. A decision is sealed, so the guard is
    // off for this rolled-back transaction only.
    sqlx::raw_sql(
        "ALTER TABLE intelligence_decisions DISABLE TRIGGER USER;
         UPDATE intelligence_decisions SET decision = jsonb_set(
             decision #- '{inputs,launches}' #- '{inputs,project,role_snapshot_digest}',
             '{inputs,project,catalog_digest}', to_jsonb(repeat('a',64)))",
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    assert_eq!(
        running(counts(&mut tx).await.unwrap()),
        [("__unclassified".to_owned(), 1)]
    );
    // Its node revoked, the attempt can never report a stop: it holds no capacity
    // and no longer blocks every limited cell of its provider.
    sqlx::query(
        "UPDATE nodes SET revoked_at=now() WHERE id IN (SELECT node_id FROM execution_attempts)",
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    assert_eq!(running(counts(&mut tx).await.unwrap()), []);
    tx.rollback().await.unwrap();
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

async fn two_provider_capacity_case(pinned: bool) -> Case {
    use crate::common::native_offer::setup_with_secondary_provider;

    let c = setup_with_secondary_provider().await;
    let pool = &c.env.env.pool;
    let level = format!(
        "{}| anthropic | model-b | high | claude |\n",
        roles::level("standard-high", &[("fixture-a", "model-a", "high")])
    );
    roles::save(
        pool,
        c.env.actor,
        Kind::Intelligence,
        "standard-high",
        &level,
    )
    .await;
    // Availability of both provider accounts is independent of the swarm caps.
    let mut scope = c.env.input.observations[1].scope.clone();
    scope.provider = "anthropic".into();
    let revision: i64 =
        sqlx::query_scalar("SELECT revision FROM intelligence_registry WHERE singleton")
            .fetch_one(pool)
            .await
            .unwrap();
    let observation = json!({"operation_id":Uuid::new_v4(),"expected_revision":revision,"sequence":1,
        "scope":scope,"state":"available","ttl_ms":30_000,"cooldown_ms":0});
    let (status, body) = crate::common::selection::call(
        &c.env.env.app,
        "/api/intelligence/observations",
        Some(observation),
        "owner@localhost",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    if pinned {
        pinned_route(&c, "standard-high");
    } else {
        route(&c, "standard-high", &[]);
    }
    c
}

async fn limits(c: &Case, preferred: u32, secondary: u32) {
    let limits = json!({"cell_limits": [
        {"provider":"fixture-a", "intelligence_class":"standard-high", "limit":preferred},
        {"provider":"anthropic", "intelligence_class":"standard-high", "limit":secondary},
    ]});
    sqlx::query("UPDATE swarm_settings SET limits=$1,revision=revision+1 WHERE singleton")
        .bind(limits)
        .execute(&c.env.env.pool)
        .await
        .unwrap();
}

async fn provider_offer(
    c: &Case,
    provider: &str,
    model: &str,
) -> aq_admission::startup::NativeOffer {
    let (status, body) = poll_offered(c).await;
    assert_eq!(status, 200, "{body}");
    let ControlReply::Poll(mut page) = decode_reply(&serde_json::to_vec(&body).unwrap()).unwrap()
    else {
        panic!("{body}");
    };
    page.validate_for(&c.request).unwrap();
    assert_eq!(page.offers.len(), 1, "{body}");
    let offer = page.offers.remove(0);
    let manifest = &offer.offered.offer.manifest;
    assert_eq!(manifest.bindings.task_id, c.env.lineage.task_id);
    assert_eq!(manifest.intelligence_profile.id.as_str(), LEVEL);
    assert_eq!(manifest.model.provider.as_str(), provider);
    assert_eq!(manifest.model.model.as_str(), model);
    let selection: Value = sqlx::query_scalar(
        "SELECT selection->'choice' FROM intelligence_attempt_selections WHERE attempt_id=$1",
    )
    .bind(manifest.bindings.attempt_id)
    .fetch_one(&c.env.env.pool)
    .await
    .unwrap();
    assert_eq!(selection["provider"], provider);
    assert_eq!(selection["model"], model);
    offer
}

async fn assert_capacity_wait(c: &Case) {
    let (status, body) = poll(c).await;
    assert_eq!(status, 200, "{body}");
    assert!(offered(&body).is_empty(), "{body}");
    assembled(c).await;
    assert_eq!(
        attempts(c).await,
        0,
        "capacity wait cannot publish an attempt"
    );
    assert_eq!(holds(c).await, 0, "capacity wait is transient");
    assert_eq!(
        skips(c).await,
        0,
        "capacity wait cannot consume a successor retry"
    );
    let denied: Option<String> =
        sqlx::query_scalar("SELECT reason FROM swarm_waiting WHERE task_id=$1")
            .bind(c.env.lineage.task_id)
            .fetch_optional(&c.env.env.pool)
            .await
            .unwrap();
    assert_eq!(
        denied.as_deref(),
        Some(aq_swarm_settings::admission::CEILING)
    );
}

#[tokio::test]
async fn provider_capacity_keeps_the_preferred_choice_when_both_cells_are_open() {
    let c = two_provider_capacity_case(false).await;
    limits(&c, 1, 1).await;
    provider_offer(&c, "fixture-a", "model-a").await;
}

#[tokio::test]
async fn provider_capacity_waits_for_the_secondary_without_consuming_attempts() {
    use crate::common::native_offer::prepare_request;
    use aq_server::{profiled_execution::initial_grant, worker_api};

    let mut c = two_provider_capacity_case(false).await;
    limits(&c, 0, 0).await;
    assert_capacity_wait(&c).await;
    limits(&c, 0, 1).await;
    let offer = provider_offer(&c, "anthropic", "model-b").await;
    assert_eq!(offer.offered.offer.binding.assignment_generation, 1);
    assert_eq!(attempts(&c).await, 1);
    assert_eq!(skips(&c).await, 0);
    assert_eq!(holds(&c).await, 0);
    let (_, _, request, _) = prepare_request(&mut c, &offer).await;
    limits(&c, 1, 1).await;
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "authorization",
        format!("Bearer {}", c.env.env.credential).parse().unwrap(),
    );
    let auth = worker_api::authenticate_context(&c.env.env.state, &headers)
        .await
        .unwrap();
    initial_grant::prepare(&c.env.env.state, &auth, &request, Uuid::new_v4())
        .await
        .expect("the sealed secondary selection succeeds after preferred capacity reopens");
    let _: uuid::Uuid = sqlx::query_scalar(
        "SELECT attempt_id FROM profiled_authority_hot WHERE attempt_id=$1 AND state->>'status'='granted'",
    )
    .bind(offer.offered.offer.binding.attempt_id)
    .fetch_one(&c.env.env.pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn provider_capacity_waits_on_a_pinned_route_even_with_a_free_alternate() {
    let c = two_provider_capacity_case(true).await;
    limits(&c, 0, 1).await;
    assert_capacity_wait(&c).await;
    limits(&c, 1, 1).await;
    provider_offer(&c, "fixture-a", "model-a").await;
}

#[tokio::test]
async fn provider_capacity_wait_does_not_charge_a_blocked_successor() {
    let c = two_provider_capacity_case(false).await;
    limits(&c, 1, 1).await;
    let first = provider_offer(&c, "fixture-a", "model-a").await;
    fail_and_release(&c, first.offered.offer.binding.attempt_id).await;
    limits(&c, 0, 0).await;
    for _ in 0..2 {
        let (status, body) = poll(&c).await;
        assert_eq!(status, 200, "{body}");
        assert!(offered(&body).is_empty(), "{body}");
        assembled(&c).await;
    }
    assert_eq!(attempts(&c).await, 1, "only the stopped predecessor exists");
    assert_eq!(skips(&c).await, 0);
    assert_eq!(holds(&c).await, 0);
    limits(&c, 0, 1).await;
    let successor = provider_offer(&c, "anthropic", "model-b").await;
    assert_eq!(successor.offered.offer.binding.assignment_generation, 2);
    assert_eq!(attempts(&c).await, 2);
}

#[tokio::test]
async fn provider_capacity_lost_at_publication_leaves_the_rest_of_the_poll_eligible() {
    use std::time::Duration;

    let c = two_provider_capacity_case(false).await;
    limits(&c, 1, 1).await;
    c.mode.store(13, Ordering::SeqCst);
    let fill = async {
        tokio::time::timeout(Duration::from_secs(15), c.observation_entered.acquire())
            .await
            .expect("the first task reached its final retention inspection")
            .unwrap()
            .forget();
        // This class can lose its provider slot while the second task's
        // default class remains available.
        limits(&c, 0, 1).await;
        c.mode.store(7, Ordering::SeqCst);
        c.observation_resume.add_permits(1);
    };
    let ((status, body), ()) = tokio::join!(poll(&c), fill);
    assert_eq!(status, 200, "{body}");
    // The request can return before its background assembler. Do not poll
    // again: only continuation of this assembly can publish the second task.
    assembled(&c).await;
    let claimed: Vec<Uuid> = sqlx::query_scalar("SELECT task_id FROM execution_attempts")
        .fetch_all(&c.env.env.pool)
        .await
        .unwrap();
    assert_eq!(claimed, [c.second_task]);
    assert_eq!(holds(&c).await, 0);
    assert_eq!(skips(&c).await, 0);
}
