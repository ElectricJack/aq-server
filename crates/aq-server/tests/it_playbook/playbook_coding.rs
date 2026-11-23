//! Fake Host8 peer only: synchronous answers in each command's exact contract
//! shape; no daemon, no database, no sleeps.
use aq_playbook_host::model::*;
use aq_playbook_host::{
    DomainHost, HostMode, MemoryStorage, Owner, Request,
    runner::{Authority, DispatchResult, Host, Intent, Storage},
};
use aq_server::playbooks::{
    coding::*,
    review_domain::{CatalogProfile, HOST7_CAPABILITIES},
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

mod evidence;
mod peer;
mod scenario;

use peer::{Completions, Log, Peer, candidate_source};
use scenario::{authority, checks, launch, owner, request, root_outcomes, settle};

const DEADLINE: Duration = Duration::from_secs(5);

#[tokio::test]
async fn coding_policy_review_rejection_creates_rework_attempt() {
    let policy = CodingPolicy::project_default(checks());
    let artifacts = policy.build().unwrap();
    let store = MemoryStorage::default();
    let log: Log = Arc::default();
    let owner = owner(
        "coding",
        store.clone(),
        log.clone(),
        vec!["changes_requested"],
    )
    .await;
    launch(
        &owner,
        &artifacts,
        authority(),
        policy.input(&request("task-1")),
    )
    .await;
    let roots = settle(&owner, &store, "coding").await;
    assert_eq!(
        roots,
        vec![(artifacts.root.digest.clone(), Some("published".into()))]
    );

    evidence::rework(&log.lock().unwrap());
}

/// Delivers one trusted aq.operation.updated event exactly as the driver
/// does (event, then fanout).
async fn deliver_completion(owner: &Owner, handle: &str, payload: Value, now: i64) {
    let registry = CatalogProfile::Host8.registry();
    let contract = &registry.events["aq.operation.updated"];
    let event = EventEnvelope {
        producer: "aq.host.bridge@1".into(),
        event_id: format!("completion-{handle}"),
        contract: ContractRef {
            name: contract.name.clone(),
            fingerprint: contract.fingerprint().unwrap(),
        },
        scope: "project".into(),
        correlation: BTreeMap::from([("operation".into(), handle.to_string())]),
        subject: Some(payload["output"]["subject"]["id"].as_str().unwrap().into()),
        revision: payload["output"]["subject"]["revision"].as_u64(),
        causation: None,
        occurred_at_ms: Some(now),
        observed_at_ms: Some(now),
        payload,
    };
    let receipt = owner
        .call(
            Request::Event {
                authenticated_producer: "aq.host.bridge@1".into(),
                epoch: 0,
                event: Box::new(event),
                requested_at_ms: Some(now),
                now_ms: now,
            },
            DEADLINE,
        )
        .await
        .unwrap();
    owner
        .call(
            Request::Fanout {
                event_key: receipt["key"].as_str().unwrap().into(),
                limit: 64,
                now_ms: now,
            },
            DEADLINE,
        )
        .await
        .unwrap();
}

/// The real Host7 work.ensure is asynchronous: aq.coding awaits each work
/// item's aq.operation.updated completion (payload from the host's terminal
/// task mapping) and converges, including a review rejection, to publish.
#[tokio::test]
async fn coding_policy_consumes_async_work_completion() {
    let policy = CodingPolicy::project_default(checks());
    let artifacts = policy.build().unwrap();
    let store = MemoryStorage::default();
    let log: Log = Arc::default();
    let queue: Completions = Arc::default();
    let mut peer = Peer::new(log.clone(), vec!["changes_requested"]);
    peer.async_work = Some(queue.clone());
    let owner = peer.start("coding-async", store.clone()).await;
    launch(
        &owner,
        &artifacts,
        authority(),
        policy.input(&request("task-1")),
    )
    .await;
    let mut roots = None;
    for now in 10..400 {
        owner
            .call(Request::Tick { now_ms: now }, DEADLINE)
            .await
            .unwrap();
        let pending: Vec<_> = queue.lock().unwrap().drain(..).collect();
        for (handle, payload) in pending {
            deliver_completion(&owner, &handle, payload, now).await;
        }
        let state = store.clone().load("coding-async").unwrap().state;
        if state.runs.values().all(|r| r.status.terminal()) {
            roots = Some(root_outcomes(&owner).await);
            break;
        }
    }
    assert_eq!(
        roots.expect("coding runs did not settle"),
        vec![(artifacts.root.digest.clone(), Some("published".into()))]
    );
    let kinds: Vec<_> = log
        .lock()
        .unwrap()
        .iter()
        .filter(|(c, _)| c == "work.ensure")
        .map(|(_, v)| v["kind"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(kinds, ["implementation", "review", "repair", "review"]);
}

#[test]
fn same_family_review_is_refused() {
    let mut p = CodingPolicy::project_default(checks());
    p.reviewer_family = " Anthropic ".into();
    assert_eq!(p.build().unwrap_err(), CodingError::SameFamily);
}

fn decision_policy() -> CodingPolicy {
    let mut p = CodingPolicy::project_default(checks());
    p.human_decision = Some(HumanDecision {
        question: "Ship this reviewed change?".into(),
        responders: vec!["user:owner".into()],
    });
    p
}
fn decision_grant() -> BTreeSet<String> {
    HOST7_CAPABILITIES
        .iter()
        .map(|c| c.to_string())
        .chain([DECISION_CAPABILITY.to_string()])
        .collect()
}

#[test]
fn human_decision_is_refused_without_the_decision_grant() {
    // A grant lacking decision.request: refuse, never fall back to a
    // review-only policy.
    let without: BTreeSet<String> = HOST7_CAPABILITIES
        .iter()
        .filter(|c| **c != DECISION_CAPABILITY)
        .map(|c| c.to_string())
        .collect();
    assert_eq!(
        decision_policy().build_for(&without).unwrap_err(),
        CodingError::NotGranted(DECISION_CAPABILITY.into())
    );
    // The default policy needs no decision capability.
    let default = CodingPolicy::project_default(checks()).build().unwrap();
    assert!(default.decision.is_none());
    assert_eq!(default.install_order().len(), 4);
    assert!(
        !default
            .root
            .content
            .playbook
            .required_capabilities
            .contains(DECISION_CAPABILITY)
    );
    let mut blank = decision_policy();
    blank.human_decision.as_mut().unwrap().responders.clear();
    assert!(matches!(
        blank.build_for(&decision_grant()),
        Err(CodingError::Invalid(_))
    ));
}

#[tokio::test]
async fn coding_policy_human_decision_follows_review_and_gates_publish() {
    let policy = decision_policy();
    let artifacts = policy.build_for(&decision_grant()).unwrap();
    assert_eq!(artifacts.install_order().len(), 5);
    assert!(
        artifacts
            .root
            .content
            .playbook
            .required_capabilities
            .contains(DECISION_CAPABILITY)
    );
    let store = MemoryStorage::default();
    let log: Log = Arc::default();
    // Attempt 0: review requests changes (no decision asked). Attempt 1:
    // review approves, the human rejects. Attempt 2: both approve.
    let mut peer = Peer::new(
        log.clone(),
        vec!["changes_requested", "accepted", "accepted"],
    );
    peer.decisions = vec!["rejected", "approved"];
    let owner = peer.start("decision", store.clone()).await;
    let authority = Authority {
        capabilities: decision_grant(),
        ..authority()
    };
    let mut req = request("task-2");
    req.decision_expires_at_ms = Some(86_400_000);
    launch(&owner, &artifacts, authority, policy.input(&req)).await;
    let roots = settle(&owner, &store, "decision").await;
    assert_eq!(
        roots,
        vec![(artifacts.root.digest.clone(), Some("published".into()))]
    );

    evidence::decision_gate(&log.lock().unwrap());
}

mod artifacts;

/// The coding artifacts keep their digests whatever fills in their registry
/// and abilities (plan task D4: one identity filler).
#[test]
fn seal_keeps_every_pinned_digest() {
    let built = CodingPolicy::project_default(checks()).build().unwrap();
    let mut strict = CodingPolicy::project_default(checks());
    strict.max_repairs = 0;
    let strict = strict.build().unwrap();
    let digests = [
        built.root.digest.as_str(),
        strict.root.digest.as_str(),
        built.attempt.digest.as_str(),
        built.work.digest.as_str(),
        built.check.digest.as_str(),
    ];
    assert_eq!(digests, PINNED, "{digests:#?}");
}
/// `aq.coding` (default), `aq.coding` with no repairs and `aq.coding.attempt`
/// as docs/implementation/D11.md records them, then `aq.coding.work` and
/// `aq.coding.check`.
const PINNED: [&str; 5] = [
    "sha256:d6f7502ca9e157c28feb734511f70b676e533d2260f450f8c9f59a2a961cec21",
    "sha256:d77c03d8994eb9f3583233257d78813f5606b2189dc162cfbf21a8837e777c47",
    "sha256:6e9af4fce7f687782b35b4554235e1d610f2e63d43d079e69edb56e3034aa9c8",
    "sha256:2326ca48c9ab8526bd79ced3f5fc806d0d42be13c161d280e41b895c902e3288",
    "sha256:7b11ebdcffae86483950f366fa8db6e50b9d6e3ae93943751dfca9c40796e5d3",
];
