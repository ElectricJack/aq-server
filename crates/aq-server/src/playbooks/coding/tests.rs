//! Fake Host8 peer only: synchronous answers in each command's exact contract
//! shape; no daemon, no database, no sleeps.
use super::*;
use crate::playbooks::review_domain::HOST7_CAPABILITIES;
use aq_playbook_host::{
    DomainHost, HostMode, MemoryStorage, Owner, Request,
    runner::{Authority, DispatchResult, Host, Intent, Storage},
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

const DEADLINE: Duration = Duration::from_secs(5);
pub(crate) type Log = Arc<Mutex<Vec<(String, Value)>>>;

/// Answers every Host8 command synchronously. `reviews` are the review
/// dispositions in order; `checks` the check outcomes in order (default pass).
pub(crate) struct Peer {
    pub log: Log,
    pub reviews: Vec<&'static str>,
    /// Human decision outcomes in order (default `approved`).
    pub decisions: Vec<&'static str>,
    pub registry: Registry,
    /// When set, work.ensure answers like the real Host7 adapter: `accepted`
    /// with a handle, its completion queued here as (handle, event payload).
    pub async_work: Option<Completions>,
}
pub(crate) type Completions = Arc<Mutex<Vec<(String, Value)>>>;
impl Peer {
    /// The host's completion for this work, from the production mapping of a
    /// terminal graph task to the aq.operation.updated payload.
    fn accept_work(&self, queue: &Completions, n: usize, input: &Value) -> DispatchResult {
        let review = self
            .log
            .lock()
            .unwrap()
            .iter()
            .filter(|(c, v)| c == "work.ensure" && v["kind"] == "review")
            .count();
        let mut node = json!({"version": 2, "task_state": "completed", "metadata": {}});
        if input["kind"] == "review" {
            node["metadata"]["aq:review_disposition"] =
                json!(self.reviews.get(review - 1).copied().unwrap_or("accepted"));
        }
        let task = format!("work-{n}");
        let request = json!({"kind": input["kind"], "reference": format!("aq.playbook.work/{task}"),
            "subject": {"id": task, "revision": 1}});
        let done = crate::playbooks::host_adapters::work_completion(&request, Some(&node))
            .unwrap()
            .unwrap();
        let handle = format!("operation-{n}");
        queue.lock().unwrap().push((
            handle.clone(),
            json!({"outcome": done.outcome, "output": done.output}),
        ));
        DispatchResult::Accepted { handle }
    }
    fn count(&self, name: &str) -> usize {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|(n, _)| n == name)
            .count()
    }
}
/// Catalog form of the n-th prepared candidate's OID (distinct from the
/// request's `source`, so a check bound to `input.source` is visible).
pub(crate) fn candidate_source(n: usize) -> Value {
    json!({"format": "sha1", "hex": format!("{n:040x}")})
}
fn subject(id: String) -> Value {
    json!({"id": id, "revision": 1})
}
impl Host for Peer {
    fn authorize(&self, _: &Authority, c: &CommandContract) -> bool {
        self.registry.commands.get(&c.name) == Some(c)
    }
    fn dispatch(&mut self, intent: &Intent) -> DispatchResult {
        let name = intent.command.name.clone();
        let n = self.count(&name) + 1;
        let input = intent.input.clone();
        self.log.lock().unwrap().push((name.clone(), input.clone()));
        if let (Some(queue), "work.ensure") = (&self.async_work, name.as_str()) {
            return self.accept_work(queue, n, &input);
        }
        let (outcome, output) = match name.as_str() {
            "work.ensure" if input["kind"] == "review" => {
                let i = self
                    .log
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(c, v)| c == "work.ensure" && v["kind"] == "review")
                    .count();
                let disposition = self.reviews.get(i - 1).copied().unwrap_or("accepted");
                (
                    "completed",
                    json!({"disposition": disposition,
                           "reference": format!("aq.review.decision/r{i}"),
                           "subject": input["subject"]}),
                )
            }
            "work.ensure" => (
                "completed",
                json!({"disposition": "accepted", "reference": format!("work:{n}"),
                       "subject": subject(format!("change-{n}"))}),
            ),
            "candidate.prepare" => (
                "prepared",
                json!({"reference": format!("candidate:{n}"),
                       "source": candidate_source(n),
                       "subject": subject(format!("candidate-{n}"))}),
            ),
            "check.request" => (
                "passed",
                json!({
                    "check": input["check"],
                    "disposition": "passed",
                    "receipt": {
                        "digest": hash_bytes(format!("receipt-{n}").as_bytes()),
                        "executor_version": "fake",
                        "exit": {"code": 0, "stop": "exited"},
                        "inputs_digest": hash_bytes(b"inputs"),
                        "result_artifacts": [],
                    },
                    "reference": format!("check-receipt:{n}"),
                    "source": input["source"],
                    "subject": input["subject"],
                }),
            ),
            "decision.request" => (
                self.decisions.get(n - 1).copied().unwrap_or("approved"),
                json!({"reference": format!("aq.decision/d{n}"),
                       "subject": input["subject"]}),
            ),
            "candidate.publish" => (
                "published",
                json!({"reference": format!("publication:{n}"),
                       "subject": input["destination"]}),
            ),
            _ => {
                return DispatchResult::Unknown {
                    reason: format!("unexpected {name}"),
                };
            }
        };
        DispatchResult::Completed {
            outcome: outcome.into(),
            output,
        }
    }
    fn reconcile(&mut self, intent: &Intent) -> DispatchResult {
        DispatchResult::Unknown {
            reason: format!("no reconcile for {}", intent.command.name),
        }
    }
}
impl DomainHost for Peer {
    fn set_mode(&mut self, _: HostMode) {}
}

pub(crate) fn authority() -> Authority {
    Authority {
        actor: "owner".into(),
        scope: "project".into(),
        capabilities: HOST7_CAPABILITIES.iter().map(|c| c.to_string()).collect(),
    }
}
pub(crate) fn checks() -> Vec<RequiredCheck> {
    vec![RequiredCheck {
        id: "cargo-test".into(),
        validator_id: "rust.cargo-test".into(),
        validator_version: "1".into(),
        validator_digest: hash_bytes(b"validator"),
    }]
}
pub(crate) fn request(key: &str) -> CodingRequest {
    CodingRequest {
        subject: json!({"id": "task-1", "revision": 1}),
        context: "Implement the thing".into(),
        base: json!({"id": "main", "revision": 1}),
        destination: json!({"id": "main", "revision": 1}),
        source: json!({"format": "sha1", "hex": "0".repeat(40)}),
        visibility: "project".into(),
        key: key.into(),
        decision_expires_at_ms: None,
    }
}
pub(crate) async fn owner(
    namespace: &str,
    store: MemoryStorage,
    log: Log,
    reviews: Vec<&'static str>,
) -> Owner {
    owner_with(namespace, store, log, reviews, vec![]).await
}
pub(crate) async fn owner_with(
    namespace: &str,
    store: MemoryStorage,
    log: Log,
    reviews: Vec<&'static str>,
    decisions: Vec<&'static str>,
) -> Owner {
    start(namespace, store, log, reviews, decisions, None).await
}
async fn start(
    namespace: &str,
    store: MemoryStorage,
    log: Log,
    reviews: Vec<&'static str>,
    decisions: Vec<&'static str>,
    async_work: Option<Completions>,
) -> Owner {
    let registry = CatalogProfile::Host8.registry();
    let peer_registry = registry.clone();
    Owner::start_registered(
        namespace.into(),
        aq_playbook_host::Limits::default(),
        registry,
        move || Ok(store),
        move |_| {
            Ok(Peer {
                log,
                reviews,
                decisions,
                registry: peer_registry,
                async_work,
            })
        },
    )
    .await
    .unwrap()
}
pub(crate) async fn install(owner: &Owner, artifacts: &CodingArtifacts) {
    for a in artifacts.install_order() {
        owner
            .call(
                Request::Install {
                    artifact: Box::new(a.clone()),
                },
                DEADLINE,
            )
            .await
            .unwrap();
    }
}
/// Ticks until every run is terminal; returns the root runs' outcomes.
pub(crate) async fn settle(
    owner: &Owner,
    store: &MemoryStorage,
    namespace: &str,
) -> Vec<(String, Option<String>)> {
    for now in 10..200 {
        owner
            .call(Request::Tick { now_ms: now }, DEADLINE)
            .await
            .unwrap();
        let state = store.clone().load(namespace).unwrap().state;
        if state.runs.values().all(|r| r.status.terminal()) {
            return state
                .runs
                .values()
                .filter(|r| r.parent.is_none())
                .map(|r| (r.artifact.clone(), r.outcome.clone()))
                .collect();
        }
    }
    panic!("coding runs did not settle");
}

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
    install(&owner, &artifacts).await;
    owner
        .call(
            Request::Activate {
                binding: ROOT.into(),
                artifact: artifacts.root.digest.clone(),
                authority: authority(),
            },
            DEADLINE,
        )
        .await
        .unwrap();
    owner
        .call(
            Request::Admit {
                binding: ROOT.into(),
                entry: "main".into(),
                input: policy.input(&request("task-1")),
                authority: authority(),
                now_ms: 1,
            },
            DEADLINE,
        )
        .await
        .unwrap();
    let roots = settle(&owner, &store, "coding").await;
    assert_eq!(
        roots,
        vec![(artifacts.root.digest.clone(), Some("published".into()))]
    );

    let log = log.lock().unwrap().clone();
    let work: Vec<_> = log
        .iter()
        .filter(|(c, _)| c == "work.ensure")
        .map(|(_, v)| v)
        .collect();
    let kinds: Vec<_> = work.iter().map(|v| v["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["implementation", "review", "repair", "review"]);
    // The rework attempt carries the rejecting review's evidence reference.
    assert_eq!(work[2]["context"], "aq.review.decision/r1");
    assert_eq!(work[2]["logical_key"], "task-1/a1/work");
    // Review is cross-family and keyed per attempt.
    assert!(
        work[1]["context"]
            .as_str()
            .unwrap()
            .contains("reviewer_family=openai")
    );
    assert_ne!(work[1]["logical_key"], work[3]["logical_key"]);
    // The repaired change is a new candidate: checks run again on it.
    let checks: Vec<_> = log
        .iter()
        .filter(|(c, _)| c == "check.request")
        .map(|(_, v)| v)
        .collect();
    assert_eq!(checks.len(), 2);
    assert_ne!(checks[0]["subject"], checks[1]["subject"]);
    // Each check binds the prepared candidate's source, not the request's.
    assert_eq!(checks[0]["source"], candidate_source(1));
    assert_eq!(checks[1]["source"], candidate_source(2));
    // Publish only once, with the check receipt and the review as evidence.
    let publish: Vec<_> = log
        .iter()
        .filter(|(c, _)| c == "candidate.publish")
        .map(|(_, v)| v)
        .collect();
    assert_eq!(publish.len(), 1);
    let evidence: Vec<_> = publish[0]["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(evidence, ["check-receipt:2", "aq.review.decision/r2"]);
    assert_eq!(publish[0]["candidate"], checks[1]["subject"]);
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
    let owner = start(
        "coding-async",
        store.clone(),
        log.clone(),
        vec!["changes_requested"],
        vec![],
        Some(queue.clone()),
    )
    .await;
    install(&owner, &artifacts).await;
    owner
        .call(
            Request::Activate {
                binding: ROOT.into(),
                artifact: artifacts.root.digest.clone(),
                authority: authority(),
            },
            DEADLINE,
        )
        .await
        .unwrap();
    owner
        .call(
            Request::Admit {
                binding: ROOT.into(),
                entry: "main".into(),
                input: policy.input(&request("task-1")),
                authority: authority(),
                now_ms: 1,
            },
            DEADLINE,
        )
        .await
        .unwrap();
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
            roots = Some(
                state
                    .runs
                    .values()
                    .filter(|r| r.parent.is_none())
                    .map(|r| (r.artifact.clone(), r.outcome.clone()))
                    .collect::<Vec<_>>(),
            );
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
    let owner = owner_with(
        "decision",
        store.clone(),
        log.clone(),
        vec!["changes_requested", "accepted", "accepted"],
        vec!["rejected", "approved"],
    )
    .await;
    install(&owner, &artifacts).await;
    let authority = Authority {
        capabilities: decision_grant(),
        ..authority()
    };
    owner
        .call(
            Request::Activate {
                binding: ROOT.into(),
                artifact: artifacts.root.digest.clone(),
                authority: authority.clone(),
            },
            DEADLINE,
        )
        .await
        .unwrap();
    let mut req = request("task-2");
    req.decision_expires_at_ms = Some(86_400_000);
    owner
        .call(
            Request::Admit {
                binding: ROOT.into(),
                entry: "main".into(),
                input: policy.input(&req),
                authority,
                now_ms: 1,
            },
            DEADLINE,
        )
        .await
        .unwrap();
    let roots = settle(&owner, &store, "decision").await;
    assert_eq!(
        roots,
        vec![(artifacts.root.digest.clone(), Some("published".into()))]
    );

    let log = log.lock().unwrap().clone();
    let steps: Vec<String> = log
        .iter()
        .filter(|(c, _)| c != "check.request" && c != "candidate.prepare")
        .map(|(c, v)| match c.as_str() {
            "work.ensure" => v["kind"].as_str().unwrap().to_string(),
            other => other.to_string(),
        })
        .collect();
    assert_eq!(
        steps,
        [
            "implementation",
            "review",
            "repair",
            "review",
            "decision.request",
            "repair",
            "review",
            "decision.request",
            "candidate.publish",
        ]
    );
    let of = |name: &str| -> Vec<Value> {
        log.iter()
            .filter(|(c, _)| c == name)
            .map(|(_, v)| v.clone())
            .collect()
    };
    let (decisions, reviews) = (of("decision.request"), of("work.ensure"));
    // Each decision is on the reviewed candidate, with the policy's question.
    assert_eq!(decisions[0]["subject"], reviews[3]["subject"]);
    assert_eq!(decisions[1]["subject"], reviews[5]["subject"]);
    assert_eq!(decisions[0]["question"], "Ship this reviewed change?");
    assert_eq!(decisions[0]["responders"], json!(["user:owner"]));
    assert_eq!(decisions[0]["choices"], json!(["approved", "rejected"]));
    assert_eq!(decisions[0]["expires_at_ms"], 86_400_000);
    // The human rejection is the next repair's evidence.
    assert_eq!(reviews[4]["context"], "aq.decision/d1");
    // Publish carries the check receipt, the review and the decision.
    let publish = of("candidate.publish");
    assert_eq!(publish.len(), 1);
    let evidence: Vec<_> = publish[0]["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        evidence,
        ["check-receipt:3", "aq.review.decision/r3", "aq.decision/d2"]
    );
    assert_eq!(publish[0]["candidate"], decisions[1]["subject"]);
}
