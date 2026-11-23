use crate::common;
use aq_playbook_host::runner::{Intent, IntentStatus};
use aq_routing::{
    CompactRoute, ProviderIntent, Route, Source, Written,
    classify::{Offer, Options},
};
use aq_server::playbooks::{
    domain::*,
    host_adapters::{HostResult, JudgmentProvider, RoutingPort, WorkGraph},
    review_domain::{CatalogProfile, HOST9_CAPABILITIES},
    service::*,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};
use uuid::Uuid;

mod graph_ports;
mod routing_events;
pub(super) use graph_ports::{FakeGraph, FakeRouting};

mod harness;
use harness::*;

mod assessment;
use assessment::*;

mod emissions;
mod failures;
mod judgments;

#[tokio::test]
async fn work_read_returns_the_facts_of_a_task_in_another_project_on_the_router() {
    let h = host(true, None).await;
    let task = h.graph.task(
        "Fix the parser",
        json!({"aq:task_type": "bugfix", "aq:intelligence_hint": "complex-high", "aq:priority": 9}),
    );
    // The server asked a route for it in another project (R1).
    (h.routing.located.lock().unwrap()).insert(task, (Uuid::new_v4(), h.other_project));
    *h.routing.attempts.lock().unwrap() = (1, 2);
    // The revision asked for does not matter: the task is read as it is.
    let read = h.read(task, 1).await;
    assert_eq!(
        (
            read.outcome.as_str(),
            &read.output["subject"],
            &read.output["reference"]
        ),
        (
            "observed",
            &json!({"id": task.to_string(), "revision": 3}),
            &json!(format!("{task}@3"))
        )
    );
    assert_eq!(
        read.output["facts"],
        json!({"kind": "bugfix", "title": "Fix the parser", "description": "Fix the parser body",
            "origin": "unknown", "level_hint": "complex-high", "charged_failures": 1,
            "ended_attempts": 2, "route": "none"})
    );
    // Without installation routing only the namespace's project is served.
    let own = host(false, None).await;
    let task = own.graph.task("Elsewhere", json!({}));
    (own.routing.located.lock().unwrap()).insert(task, (Uuid::new_v4(), own.other_project));
    let refused = own.read(task, 3).await;
    assert_eq!(
        (refused.outcome.as_str(), &refused.output["reason"]),
        ("refused", &json!("not_on_router"))
    );
    let mine = own.graph.task("Mine", json!({}));
    assert_eq!(own.read(mine, 3).await.outcome, "observed");
}

#[tokio::test]
async fn work_read_refuses_a_task_whose_binding_is_pinned_to_a_profile() {
    let h = host(true, None).await;
    let task = h.graph.task("Pinned", json!({}));
    // The namespace's own project is not on the automatic router.
    h.routing.on_router.lock().unwrap().clear();
    let refused = h.read(task, 3).await;
    assert_eq!(refused.outcome, "refused");
    assert_eq!(refused.output["reason"], "not_on_router");
    let routed = h.route(task, 3, json!({})).await;
    assert_eq!(routed.output["reason"], "not_on_router");
    assert!(h.routing.writes.lock().unwrap().is_empty());
    // A task that does not exist, or an id that is no task id.
    (h.routing.on_router.lock().unwrap()).insert(h.scope.project_id);
    let missing = h.read(Uuid::new_v4(), 1).await;
    assert_eq!(missing.output["reason"], "unknown_task");
    let input = json!({"subject": {"id": "not-a-task", "revision": 1}});
    let odd = h.run("odd", "work.read", input).await;
    assert_eq!(
        (odd.outcome.as_str(), &odd.output["reason"]),
        ("refused", &json!("unknown_task"))
    );
}

#[tokio::test]
async fn a_route_for_an_older_task_revision_is_stale_and_writes_nothing() {
    let h = host(true, None).await;
    let task = h
        .graph
        .task("Changed since the read", json!({"aq:priority": 4}));
    let stale = h.route(task, 2, json!({})).await;
    assert_eq!(
        (stale.outcome.as_str(), &stale.output["subject"]),
        ("stale", &json!({"id": task.to_string(), "revision": 3}))
    );
    assert!(h.routing.writes.lock().unwrap().is_empty());
    assert_eq!((h.graph.stored(task), h.emission(task).await), (None, None));
    // At the version read, it is written.
    let routed = h.route(task, 3, json!({})).await;
    assert_eq!(routed.outcome, "routed");
    assert_eq!(h.graph.node_of(task)["metadata"]["aq:priority"], 4);
    assert_eq!(h.routing.writes.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_route_naming_an_unoffered_level_or_harness_is_refused() {
    let h = host(true, None).await;
    let task = h.graph.task("Nothing can take it", json!({}));
    for change in [
        json!({"level": "deep-high"}),
        json!({"harnesses": ["cursor"]}),
        json!({"providers": ["google"]}),
        json!({"level": "complex-high", "harnesses": ["codex"]}),
    ] {
        let refused = h.route(task, 3, change.clone()).await;
        assert_eq!(refused.outcome, "refused", "{change}");
        assert_eq!(refused.output["reason"], "not_offered", "{change}");
    }
    assert!(h.routing.writes.lock().unwrap().is_empty());
    // Recorded, so the server does not ask again until something changes.
    let (version, outcome, reasons) = h.emission(task).await.unwrap();
    assert_eq!((version, outcome.as_deref()), (3, Some("not_offered")));
    assert_eq!(reasons.unwrap()[0]["code"], "level_from_kind");
    // The host keeps what the level offers and reports it.
    let kept = h
        .route(
            task,
            3,
            json!({"harnesses": ["cursor", "codex"], "prefer": ["codex", "claude"]}),
        )
        .await;
    assert_eq!(kept.outcome, "routed");
    assert_eq!(
        (&kept.output["harnesses"], &kept.output["prefer"]),
        (&json!(["codex"]), &json!(["codex"]))
    );
    assert_eq!(h.graph.stored(task).unwrap().h, ["codex"]);
}

#[tokio::test]
async fn an_owner_route_is_never_replaced() {
    let h = host(true, None).await;
    let owner = json!({"aq:route": {"c": "complex-high", "l": null, "i": "any", "d": null, "s": "owner", "n": 1}});
    let task = h.graph.task("The owner chose", owner.clone());
    assert_eq!(h.read(task, 3).await.output["facts"]["route"], "owner");
    let refused = h.route(task, 3, json!({})).await;
    assert_eq!(
        (refused.outcome.as_str(), &refused.output["reason"]),
        ("refused", &json!("owner_route"))
    );
    let input = json!({"subject": {"id": task.to_string(), "revision": 3},
        "reasons": [{"code": "policy", "detail": "Nobody may take it."}]});
    let unroutable = h.run("unroutable", "work.unroutable", input).await;
    assert_eq!(unroutable.output["reason"], "owner_route");
    assert_eq!(h.graph.node_of(task)["metadata"], owner);
    assert!(h.routing.writes.lock().unwrap().is_empty());
}

#[tokio::test]
async fn the_route_records_the_artifact_digest_and_hold_pins_the_provider() {
    let h = host(true, None).await;
    let digest = format!("sha256:{}", "ab".repeat(32));
    h.activated(&digest).await;
    *h.routing.attempts.lock().unwrap() = (1, 2);
    let task = h.graph.task("Held for one agent", json!({}));
    let classification = json!({"reference": "judgment:7", "fields": ["risk"], "labels": ["high"],
        "flags": ["hard"], "rationale": "Touches billing."});
    let change = json!({"harnesses": ["claude"], "hold": true, "classification": classification,
        "reasons": [{"code": "raised_for_risk", "detail": "High risk is at least Standard, high."}, {"code": "held_for_harness"}]});
    let routed = h.route(task, 3, change).await;
    assert_eq!(routed.outcome, "routed");
    let stored = h.graph.stored(task).unwrap();
    assert_eq!(stored.d.as_deref(), Some(&digest[..16]));
    assert_eq!(
        (stored.s, stored.i, stored.a),
        (Source::Router, ProviderIntent::Pinned, 2)
    );
    let (_, route) = h.routing.writes.lock().unwrap()[0].clone();
    let route = route.unwrap();
    assert_eq!(
        (route.policy_digest.as_deref(), route.judgment.as_ref()),
        (Some(digest.as_str()), Some(&classification))
    );
    assert_eq!(route.reasons.len(), 2);
    assert_eq!(
        (
            route.reasons[1].code.as_str(),
            route.reasons[1].detail.as_str()
        ),
        ("held_for_harness", "")
    );
    assert_eq!(h.read(task, 4).await.output["facts"]["route"], "router");
}

#[tokio::test]
async fn work_unroutable_leaves_the_task_unrouted_with_its_reasons() {
    let h = host(true, None).await;
    let task = h.graph.task("The document allows nobody", json!({}));
    assert_eq!(h.route(task, 3, json!({})).await.outcome, "routed");
    let unroutable = |revision: i64, reasons: Value| json!({"subject": {"id": task.to_string(), "revision": revision}, "reasons": reasons});
    let none = h
        .run("none", "work.unroutable", unroutable(4, json!([])))
        .await;
    assert_eq!(
        (none.outcome.as_str(), &none.output["reason"]),
        ("refused", &json!("no_reasons"))
    );
    let reasons = json!([{"code": "policy", "detail": "Never route payroll work."}]);
    let stale = h
        .run("stale", "work.unroutable", unroutable(3, reasons.clone()))
        .await;
    assert_eq!(stale.outcome, "stale");
    assert!(h.graph.stored(task).is_some());
    let recorded = h
        .run(
            "recorded",
            "work.unroutable",
            unroutable(4, reasons.clone()),
        )
        .await;
    assert_eq!(recorded.outcome, "recorded");
    // The router's route is gone, and the row is for the task as left.
    assert_eq!(
        (
            h.graph.stored(task),
            &recorded.output["subject"]["revision"]
        ),
        (None, &json!(5))
    );
    assert_eq!(
        h.emission(task).await.unwrap(),
        (5, Some("unroutable".into()), Some(reasons.clone()))
    );
    // The same intent again: the record, no second write.
    let again = h
        .run("recorded", "work.unroutable", unroutable(4, reasons))
        .await;
    assert_eq!(again, recorded);
    assert_eq!(h.routing.writes.lock().unwrap().len(), 2);
}
