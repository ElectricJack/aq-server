//! Ask, then read the answer, end to end on the in-memory engine: a playbook
//! invokes the asynchronous `judgment.request` (host catalog 2.1.0), joins its
//! handle by name, and branches on `choice`, a field only the `assessed`
//! outcome has. The fake host admits the request; the completion arrives as
//! the pinned `aq.judgment.completed` event.
use aq_playbooks_model::*;
use aq_playbooks_runner::*;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

const REGISTRY_2_1: &str = include_str!("../../../contracts/aq-host-2.1.0/registry.json");
const HANDLE: &str = "judgment-op-1";
const PRODUCER: &str = "aq.host.bridge@1";
const JOIN_DEADLINE_MS: u64 = 1000;
const HOLD_MS: u64 = 50;

fn set(xs: &[&str]) -> BTreeSet<String> {
    xs.iter().map(|s| s.to_string()).collect()
}
fn edges(xs: &[(&str, &str)]) -> BTreeMap<String, String> {
    xs.iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}
fn r(root: &str, path: &[&str]) -> Expr {
    Expr::Ref {
        root: root.into(),
        path: path.iter().map(|s| s.to_string()).collect(),
    }
}
fn ret(outcome: &str) -> Step {
    Step {
        op: Operation::Return {
            outcome: outcome.into(),
            output: Expr::literal(Value::Null),
        },
        edges: BTreeMap::new(),
        on_error: None,
    }
}
fn authority() -> Authority {
    Authority {
        actor: "owner".into(),
        scope: "project".into(),
        capabilities: set(&["judgment.request.v2"]),
    }
}
fn registry() -> Registry {
    let all: Registry = serde_json::from_str(REGISTRY_2_1).unwrap();
    Registry {
        commands: all
            .commands
            .into_iter()
            .filter(|(name, _)| ["judgment.request", "operation.get"].contains(&name.as_str()))
            .collect(),
        events: all
            .events
            .into_iter()
            .filter(|(name, _)| name == "aq.judgment.completed")
            .collect(),
    }
}
/// `ask` → (`hold`, a 50 ms timer, when `hold`) → `read`, a join on the handle
/// under the key `c` → `decide` on `answer.c.output.choice`.
fn playbook(member: Expr, hold: bool) -> Artifact {
    let registry = registry();
    let judgment = &registry.commands["judgment.request"];
    let after_ask = if hold { "hold" } else { "read" };
    let mut steps = BTreeMap::from([
        (
            "ask".into(),
            Step {
                op: Operation::Invoke {
                    command: ContractRef {
                        name: judgment.name.clone(),
                        fingerprint: judgment.fingerprint().unwrap(),
                    },
                    input: Expr::literal(json!({
                        "budget": {"deadline_ms": 60000, "max_cost_microunits": 50000, "max_input_tokens": 8192, "max_output_tokens": 512},
                        "context": {"artifacts": [], "text": "Work item summary."},
                        "output": {"choices": ["approve", "decline"], "rationale_max_bytes": 2048},
                        "question": "Should this change be approved?",
                        "subject": {"id": "work-1", "revision": 1},
                    })),
                    bind: "j".into(),
                    retry: None,
                },
                edges: edges(&[
                    ("accepted", after_ask),
                    ("assessed", "error"),
                    ("failed", "error"),
                    ("refused", "error"),
                ]),
                on_error: Some("error".into()),
            },
        ),
        (
            "read".into(),
            Step {
                op: Operation::Join {
                    members: vec![member],
                    member_keys: vec!["c".into()],
                    mode: JoinMode::All,
                    accepted_outcomes: set(&["assessed"]),
                    deadline_ms: JOIN_DEADLINE_MS,
                    remaining: RemainingPolicy::Track,
                    bind: "answer".into(),
                },
                edges: edges(&[
                    ("completed", "decide"),
                    ("no_match", "unanswered"),
                    ("timeout", "late"),
                ]),
                on_error: Some("error".into()),
            },
        ),
        (
            "decide".into(),
            Step {
                op: Operation::Decide {
                    predicate: Expr::Eq {
                        left: Box::new(r("answer", &["c", "output", "choice"])),
                        right: Box::new(Expr::literal("approve")),
                    },
                },
                edges: edges(&[
                    ("true", "approved"),
                    ("false", "declined"),
                    ("unknown", "error"),
                ]),
                on_error: None,
            },
        ),
        ("approved".into(), ret("approved")),
        ("declined".into(), ret("declined")),
        ("unanswered".into(), ret("unanswered")),
        ("late".into(), ret("late")),
        ("error".into(), ret("error")),
    ]);
    if hold {
        steps.insert(
            "hold".into(),
            Step {
                op: Operation::Wait {
                    event: None,
                    correlation: BTreeMap::new(),
                    minimum_revision: None,
                    deadline_ms: HOLD_MS,
                    bind: "held".into(),
                },
                edges: edges(&[("timeout", "read")]),
                on_error: Some("error".into()),
            },
        );
    }
    Artifact::seal(ArtifactContent {
        format_version: FORMAT_VERSION,
        compiler: "test".into(),
        source: SourceBinding {
            digest: hash_bytes(b"ask-then-read"),
            header: SourceHeader {
                format_version: FORMAT_VERSION,
                id: "ask-then-read".into(),
                scope: "project".into(),
                interfaces: set(&["main"]),
                definitions: BTreeSet::new(),
            },
        },
        playbook: Playbook {
            id: "ask-then-read".into(),
            purpose: "ask, then read the answer".into(),
            scope: "project".into(),
            entries: BTreeMap::from([(
                "main".into(),
                EntryPoint {
                    input: Schema::Null,
                    outcomes: ["approved", "declined", "unanswered", "late", "error"]
                        .into_iter()
                        .map(|o| (o.to_string(), Schema::Null))
                        .collect(),
                    start: "ask".into(),
                },
            )]),
            steps,
            triggers: vec![],
            limits: Limits::default(),
            required_capabilities: set(&["judgment.request.v2"]),
        },
        registry,
        dependencies: BTreeMap::new(),
        definitions: BTreeMap::new(),
        source_map: BTreeMap::new(),
    })
    .unwrap()
}
fn provenance() -> Value {
    json!({
        "context_digest": format!("sha256:{}", "a".repeat(64)),
        "credential_profile": "profile/judgment-default",
        "model": "model-a",
        "output_digest": format!("sha256:{}", "b".repeat(64)),
        "prompt_digest": format!("sha256:{}", "c".repeat(64)),
        "provider": "provider-a",
        "usage": {"cost_microunits": 4100, "input_tokens": 900, "output_tokens": 120},
    })
}
fn assessed(choice: &str) -> Value {
    json!({"outcome": "assessed", "output": {
        "choice": choice,
        "provenance": provenance(),
        "rationale": "The change is covered by a test.",
        "reference": format!("judgment:{HANDLE}"),
        "subject": {"id": "work-1", "revision": 1},
    }})
}
fn failed() -> Value {
    json!({"outcome": "failed", "output": {
        "provenance": null,
        "reason": "provider_unavailable",
        "reference": format!("judgment:{HANDLE}"),
        "subject": {"id": "work-1", "revision": 1},
    }})
}
fn refused() -> Value {
    json!({"outcome": "refused", "output": {
        "provenance": provenance(),
        "reason": "budget_exceeded",
        "reference": format!("judgment:{HANDLE}"),
        "subject": {"id": "work-1", "revision": 1},
    }})
}
fn completion(a: &Artifact, payload: Value) -> EventEnvelope {
    let contract = &a.content.registry.events["aq.judgment.completed"];
    EventEnvelope {
        producer: PRODUCER.into(),
        event_id: "judgment-completed-1".into(),
        contract: ContractRef {
            name: contract.name.clone(),
            fingerprint: contract.fingerprint().unwrap(),
        },
        scope: "project".into(),
        correlation: BTreeMap::from([("operation".into(), HANDLE.into())]),
        subject: Some("work-1".into()),
        revision: Some(1),
        causation: None,
        occurred_at_ms: None,
        observed_at_ms: None,
        payload,
    }
}
struct Ask {
    engine: Engine<MemoryStorage>,
    host: FakeHost,
    artifact: Artifact,
    run: String,
}
/// Installs and admits the playbook, then drives it at time 0: the host admits
/// the judgment request and the run waits (at the join, or at `hold`).
fn ask(hold: bool) -> Ask {
    let engine = Engine::new(
        MemoryStorage::retaining_settled(),
        "async-answer",
        "owner-1",
    )
    .unwrap();
    let artifact = playbook(r("j", &[]), hold);
    engine.install(artifact.clone()).unwrap();
    engine
        .activate("ask-then-read", &artifact.digest, authority())
        .unwrap();
    let run = engine
        .admit("ask-then-read", "main", Value::Null, authority(), 0)
        .unwrap();
    let mut host = FakeHost {
        responses: BTreeMap::from([(
            "judgment.request".into(),
            vec![DispatchResult::Accepted {
                handle: HANDLE.into(),
            }],
        )]),
        ..Default::default()
    };
    engine.drive(&mut host, 0, 20).unwrap();
    let s = engine.snapshot().unwrap();
    assert_eq!(s.runs[&run].status, RunStatus::Waiting);
    assert_eq!(s.runs[&run].step, if hold { "hold" } else { "read" });
    assert_eq!(s.runs[&run].variables["j"], json!({"handle": HANDLE}));
    Ask {
        engine,
        host,
        artifact,
        run,
    }
}
impl Ask {
    fn complete(&mut self, payload: Value, at_ms: i64) {
        self.engine
            .admit_event(PRODUCER, completion(&self.artifact, payload), at_ms)
            .unwrap();
        self.engine.drive(&mut self.host, at_ms, 20).unwrap();
    }
    /// The run while it is live.
    fn run(&self) -> Run {
        self.engine
            .snapshot()
            .unwrap()
            .runs
            .get(&self.run)
            .cloned()
            .unwrap_or_else(|| panic!("{} has left the state", self.run))
    }
    /// The finished run as its cohort left the state (spec 0003: a finished
    /// run leaves in the commit that finishes it). Asserts that it has left.
    fn finished(&self) -> Run {
        assert!(
            !self.engine.snapshot().unwrap().runs.contains_key(&self.run),
            "{} is still live",
            self.run
        );
        self.engine
            .store
            .settled_run(&self.engine.namespace, &self.run)
            .unwrap_or_else(|| panic!("{} is not in history", self.run))
    }
    /// Every public inspection item of `kind` for this run (live state plus
    /// history).
    fn inspected(&self, kind: InspectionKind) -> Vec<Value> {
        let mut items = vec![];
        let mut after = None;
        loop {
            let page = self
                .engine
                .inspect(&InspectionQuery {
                    kind,
                    run: Some(self.run.clone()),
                    after,
                    limit: 1000,
                })
                .unwrap();
            items.extend(page.items);
            match page.next {
                Some(next) => after = Some(next),
                None => return items,
            }
        }
    }
    fn assert_finished(&self, outcome: &str) {
        let run = self.finished();
        assert_eq!(run.status, RunStatus::Completed, "{run:?}");
        assert_eq!(run.outcome.as_deref(), Some(outcome), "{run:?}");
        // Inspection serves the same outcome from history.
        let inspected = self.inspected(InspectionKind::Runs);
        assert_eq!(inspected.len(), 1, "{inspected:?}");
        assert_eq!(inspected[0]["status"], "completed");
        assert_eq!(inspected[0]["outcome"], outcome);
        // The run and the operation it owned left together.
        let s = self.engine.snapshot().unwrap();
        assert!(!s.intents.values().any(|i| i.run == self.run));
        assert!(
            !self
                .inspected(InspectionKind::Receipts)
                .iter()
                .any(|r| r["kind"] == "engine_failure"),
        );
        assert!(!s.receipts.iter().any(|r| r.kind == "engine_failure"));
    }
}

#[test]
fn an_assessed_answer_is_read_by_name_and_branches_on_its_choice() {
    for (choice, outcome) in [("approve", "approved"), ("decline", "declined")] {
        let mut a = ask(false);
        a.complete(assessed(choice), 10);
        a.assert_finished(outcome);
        let answer = &a.finished().variables["answer"];
        assert_eq!(answer["c"]["outcome"], "assessed");
        assert_eq!(answer["c"]["output"]["choice"], choice);
        // `participant` is the operation the engine owns, not the host handle.
        let effects = a.inspected(InspectionKind::Effects);
        assert_eq!(effects.len(), 1, "{effects:?}");
        assert_eq!(answer["c"]["participant"], effects[0]["id"]);
    }
}

#[test]
fn a_failed_or_refused_answer_takes_no_match_without_reading_the_choice() {
    for payload in [failed(), refused()] {
        let outcome = payload["outcome"].clone();
        let mut a = ask(false);
        a.complete(payload, 10);
        a.assert_finished("unanswered");
        let run = a.finished();
        // The playbook never reached the step that reads `choice`.
        let receipts = a.inspected(InspectionKind::Receipts);
        assert!(
            !receipts
                .iter()
                .any(|r| r["run"] == a.run.as_str() && r["step"] == "decide"),
            "{receipts:?}"
        );
        assert!(
            receipts
                .iter()
                .any(|r| r["run"] == a.run.as_str() && r["step"] == "unanswered")
        );
        assert_eq!(run.variables["answer"]["c"]["outcome"], outcome);
        assert!(
            run.variables["answer"]["c"]["output"]
                .get("choice")
                .is_none()
        );
    }
}

#[test]
fn an_answer_that_arrives_before_the_join_is_reached_is_read_there() {
    let mut a = ask(true);
    a.complete(assessed("approve"), 10);
    // The completion is recorded on the operation while the run still holds.
    let run = a.run();
    assert_eq!(
        (run.status, run.step.as_str()),
        (RunStatus::Waiting, "hold")
    );
    let s = a.engine.snapshot().unwrap();
    let intent = s.intents.values().find(|i| i.run == a.run).unwrap();
    assert_eq!(intent.status, IntentStatus::Completed);
    a.engine.drive(&mut a.host, HOLD_MS as i64, 20).unwrap();
    a.assert_finished("approved");
    assert_eq!(
        a.finished().variables["answer"]["c"]["output"]["choice"],
        "approve"
    );
}

#[test]
fn an_answer_that_never_arrives_takes_the_timeout_edge_at_the_deadline() {
    let mut a = ask(false);
    a.engine
        .drive(&mut a.host, JOIN_DEADLINE_MS as i64 - 1, 20)
        .unwrap();
    assert_eq!(a.run().status, RunStatus::Waiting);
    a.engine
        .drive(&mut a.host, JOIN_DEADLINE_MS as i64, 20)
        .unwrap();
    // The timeout edge is taken at the deadline with nothing read. The run
    // holds its `late` return until the operation it still owns settles.
    let run = a.run();
    assert_eq!(run.step, "late", "{run:?}");
    assert_eq!(run.status, RunStatus::Finishing, "{run:?}");
    assert_eq!(run.pending_return, Some(("late".into(), Value::Null)));
    assert_eq!(run.variables["answer"], json!({}));
    // A completion after the deadline settles the operation but cannot
    // replace the recorded timeout.
    a.complete(assessed("approve"), JOIN_DEADLINE_MS as i64 + 10);
    a.assert_finished("late");
    assert_eq!(a.finished().variables["answer"], json!({}));
}

#[test]
fn an_answer_landing_exactly_at_the_join_deadline_is_read() {
    // The join counts an answer accepted at or before its deadline, and the
    // engine records completions before it polls the join, so an answer
    // accepted at the deadline wins over the timeout.
    let mut a = ask(false);
    a.complete(assessed("approve"), JOIN_DEADLINE_MS as i64);
    a.assert_finished("approved");
}

#[test]
fn an_answer_that_never_arrives_keeps_the_run_live_until_the_host_settles_it() {
    let mut a = ask(false);
    let deadline = a.artifact.content.registry.commands["judgment.request"].deadline_ms as i64;
    assert_eq!(deadline, 60_000);
    a.engine
        .drive(&mut a.host, JOIN_DEADLINE_MS as i64, 20)
        .unwrap();
    assert_eq!(a.run().step, "late");
    // Past the command's own deadline the operation is Unknown. The engine
    // neither reconciles it (judgment.request is keyed_idempotent, so only an
    // authorized recovery request replays it) nor cancels it (no
    // cancellation), so the run stays Finishing however long it waits.
    for now in [deadline, deadline + 1, 10 * deadline, 1_000 * deadline] {
        a.engine.drive(&mut a.host, now, 20).unwrap();
        let s = a.engine.snapshot().unwrap();
        let intent = s.intents.values().find(|i| i.run == a.run).unwrap();
        assert_eq!(intent.status, IntentStatus::Unknown, "at {now}");
        let run = &s.runs[&a.run];
        assert_eq!(run.status, RunStatus::Finishing, "at {now}");
        assert_eq!(run.outcome, None);
        assert_eq!(run.pending_return, Some(("late".into(), Value::Null)));
    }
    assert!(a.host.reconciliations.is_empty());
    assert!(a.host.cancelled.is_empty());
    // A final completion from the host, however late, settles it: the run
    // finishes with the timeout's outcome.
    a.complete(failed(), 1_000 * deadline + 1);
    a.assert_finished("late");
}

#[test]
fn a_stored_artifact_with_the_handle_path_installs_and_is_refused_at_the_join() {
    // Verification stays lenient so an old artifact can never make its
    // namespace unloadable; compiling a draft with this form is refused.
    let e = Engine::new(MemoryStorage::default(), "async-answer", "owner-1").unwrap();
    let artifact = playbook(r("j", &["handle"]), false);
    e.install(artifact.clone()).unwrap();
    e.activate("ask-then-read", &artifact.digest, authority())
        .unwrap();
    let run = e
        .admit("ask-then-read", "main", Value::Null, authority(), 0)
        .unwrap();
    let mut host = FakeHost {
        responses: BTreeMap::from([(
            "judgment.request".into(),
            vec![DispatchResult::Accepted {
                handle: HANDLE.into(),
            }],
        )]),
        ..Default::default()
    };
    e.drive(&mut host, 0, 20).unwrap();
    // The runner resolves `j.handle` to the bare handle string, which is no
    // owned child, refuses the join step and takes its `on_error` edge.
    let s = e.snapshot().unwrap();
    assert!(
        s.receipts.iter().any(|r| r.run == run
            && r.step == "read"
            && r.kind == "error"
            && r.detail["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("join requires owned child handle"))),
        "{:?}",
        s.receipts
    );
    assert_eq!(s.runs[&run].step, "error");
    assert_eq!(
        s.runs[&run].pending_return,
        Some(("error".into(), Value::Null))
    );
}
