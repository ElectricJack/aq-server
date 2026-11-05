//! D6 host adapters (catalog profile 6, AQ host catalog 2.0.0) on disposable
//! PostgreSQL through the production service owners. The sandbox check
//! executor and the judgment provider are in-memory fakes of the injected
//! ports (no real sandbox, provider or network); every receipt disposition
//! comes from an executor-reported stop, never from the host.
mod common;
use aq_playbook_host::runner::{CancellationResult, Intent, IntentStatus};
use aq_server::{
    playbooks::{
        domain::*,
        host_adapters::{
            ArtifactPin, CHECK_EXECUTION_PROFILE, CheckAttempt, CheckExecutor, CheckObservation,
            CheckRun, CheckStop, HostResult, JudgmentProvider, JudgmentReply, JudgmentRequest,
            PinnedCheck, PinnedOutcome, ProviderFailure, WorkGraph,
        },
        review_domain::{CatalogProfile, HOST7_CAPABILITIES},
        service::*,
    },
    protected_storage::{KeyFile, Keyring, ProtectionConfig},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    os::unix::fs::PermissionsExt,
    sync::{Arc, Mutex},
};
use uuid::Uuid;

#[derive(Default)]
struct FakeGraph {
    nodes: Mutex<BTreeMap<Uuid, Value>>,
}
#[async_trait::async_trait]
impl WorkGraph for FakeGraph {
    async fn node(&self, _: Uuid, id: Uuid) -> Result<Option<Value>> {
        Ok(self.nodes.lock().unwrap().get(&id).cloned())
    }
    async fn mutate(&self, _: Uuid, _: Value) -> Result<(u16, Value)> {
        panic!("D6 adapters never mutate the graph")
    }
}

/// Sandbox executor fake: one attempt per operation, scripted stop per check.
#[derive(Default)]
struct FakeChecks {
    pinned: Mutex<BTreeMap<String, PinnedOutcome>>,
    script: Mutex<BTreeMap<String, CheckObservation>>,
    attempts: Mutex<BTreeMap<Uuid, (CheckAttempt, CheckObservation)>>,
    starts: Mutex<usize>,
    cancels: Mutex<Vec<Uuid>>,
}
impl FakeChecks {
    fn starts(&self) -> usize {
        *self.starts.lock().unwrap()
    }
    fn finish(&self, operation: Uuid, run: CheckRun) {
        self.attempts.lock().unwrap().get_mut(&operation).unwrap().1 =
            CheckObservation::Finished(run);
    }
}
#[async_trait::async_trait]
impl CheckExecutor for FakeChecks {
    async fn pinned(&self, _: Uuid, _: Uuid, subject: &str) -> Result<Option<PinnedOutcome>> {
        Ok(self.pinned.lock().unwrap().get(subject).cloned())
    }
    async fn start(&self, attempt: &CheckAttempt) -> Result<()> {
        *self.starts.lock().unwrap() += 1;
        assert!(
            !self.cancels.lock().unwrap().contains(&attempt.operation),
            "start after cancel"
        );
        let mut attempts = self.attempts.lock().unwrap();
        assert!(
            !attempts.contains_key(&attempt.operation),
            "second writer for one operation"
        );
        let observation = self.script.lock().unwrap()[&attempt.check_id].clone();
        attempts.insert(attempt.operation, (attempt.clone(), observation));
        Ok(())
    }
    async fn observe(&self, operation: Uuid) -> Result<CheckObservation> {
        Ok(self
            .attempts
            .lock()
            .unwrap()
            .get(&operation)
            .map(|(_, o)| o.clone())
            .unwrap_or(CheckObservation::NotFound))
    }
    /// Acknowledges only; the stop is reported later through `observe`.
    async fn cancel(&self, operation: Uuid) -> Result<()> {
        self.cancels.lock().unwrap().push(operation);
        Ok(())
    }
}

const SECRET: &str = "sk-live-never-leaves-the-vault";
/// Provider fake: replies scripted per question; holds a secret that must
/// never appear in a request, output or provenance.
#[derive(Default)]
struct FakeJudge {
    replies: Mutex<BTreeMap<String, std::result::Result<JudgmentReply, ProviderFailure>>>,
    requests: Mutex<Vec<JudgmentRequest>>,
    /// Questions whose reply waits for `release`, proving the provider runs
    /// off the dispatch path.
    held: Mutex<BTreeSet<String>>,
    release: tokio::sync::Notify,
}
#[async_trait::async_trait]
impl JudgmentProvider for FakeJudge {
    fn credential_profile(&self, _: Uuid, _: Uuid) -> Option<String> {
        Some("profile/judgment-default".into())
    }
    async fn complete(
        &self,
        request: &JudgmentRequest,
    ) -> std::result::Result<JudgmentReply, ProviderFailure> {
        assert!(!format!("{request:?}").contains(SECRET));
        let _credential = SECRET; // resolved inside the provider only
        self.requests.lock().unwrap().push(request.clone());
        if self.held.lock().unwrap().contains(&request.question) {
            self.release.notified().await;
        }
        self.replies.lock().unwrap()[&request.question].clone()
    }
}

struct Host {
    service: Service,
    runtime: RuntimeOwner,
    grant: Uuid,
    scope: GrantScope,
    now: i64,
    graph: Arc<FakeGraph>,
}
async fn host(
    f: &common::profiled::Fixture,
    checks: Option<Arc<dyn CheckExecutor>>,
    judgments: Option<Arc<dyn JudgmentProvider>>,
) -> Host {
    let l = &f.selection.lineage;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("key");
    std::fs::write(&path, [9_u8; 32]).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let keys = Keyring::load(&ProtectionConfig {
        active_key_id: "test".into(),
        keys: vec![KeyFile {
            id: "test".into(),
            file: path,
        }],
    })
    .unwrap();
    let graph = Arc::new(FakeGraph::default());
    let service = Service::new_host7(
        &f.state,
        Namespace {
            namespace_id: Uuid::new_v4(),
            generation: 1,
            name: format!("fixture-{}", Uuid::new_v4()),
            workspace_id: l.workspace_id,
            project_id: l.project_id,
            owner_user_id: f.actor,
        },
        Arc::new(keys),
        graph.clone(),
        checks,
        judgments,
    )
    .await
    .unwrap();
    service.initialize(f.actor).await.unwrap();
    let now = (f.state.clock)().timestamp_millis();
    let scope = GrantScope {
        workspace_id: l.workspace_id,
        project_id: l.project_id,
        recipients: vec![],
        capabilities: BTreeSet::from(HOST7_CAPABILITIES.map(String::from)),
    };
    let grant = Uuid::new_v4();
    service
        .grant(f.actor, grant, scope.clone(), now + 10_000)
        .await
        .unwrap();
    let runtime = service.adopt_runtime(Uuid::new_v4(), 0).await.unwrap();
    Host {
        service,
        runtime,
        grant,
        scope,
        now,
        graph,
    }
}
impl Host {
    fn intent(&self, id: &str, name: &str, input: Value) -> Intent {
        Intent {
            id: id.into(),
            run: "synthetic-run".into(),
            step: name.replace('.', "-"),
            invocation: 1,
            deadline_at_ms: self.now + 5000,
            command: CatalogProfile::Host7.registry().commands[name].clone(),
            input_digest: digest(&input).unwrap(),
            input,
            authority: CatalogProfile::Host7
                .authority(self.grant, 1, &self.scope)
                .unwrap(),
            status: IntentStatus::Dispatched,
            cancel_requested: false,
            cancel_dispatched: false,
            handle: None,
            result: None,
            reason: None,
            late_results: vec![],
            completion_event: None,
        }
    }
    async fn run(&self, id: &str, name: &str, input: Value) -> HostResult {
        self.service
            .host_dispatch(&self.runtime, self.grant, 1, &self.intent(id, name, input))
            .await
            .unwrap()
    }
    /// Dispatch that must be accepted; returns the durable handle.
    async fn accepted(&self, id: &str, name: &str, input: Value) -> String {
        let r = self.run(id, name, input).await;
        assert_eq!(r.outcome, "accepted", "{r:?}");
        r.output["handle"].as_str().unwrap().to_string()
    }
    async fn get(&self, handle: &str) -> HostResult {
        self.run(
            &format!("get-{}", Uuid::new_v4()),
            "operation.get",
            json!({"handle": handle}),
        )
        .await
    }
    /// Waits (bounded) for an asynchronous operation to complete by polling
    /// operation.get, the reconciliation path; returns the completion.
    async fn settled(&self, handle: &str) -> (String, Value) {
        for _ in 0..400 {
            let r = self.get(handle).await;
            if r.outcome != "running" {
                return self.completed(handle).await;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("operation {handle} did not complete");
    }
    /// operation.get that must report a completion; returns it.
    async fn completed(&self, handle: &str) -> (String, Value) {
        let r = self.get(handle).await;
        assert_eq!(r.outcome, "completed", "{r:?}");
        (
            r.output["outcome"].as_str().unwrap().to_string(),
            r.output["output"].clone(),
        )
    }
}
async fn outbox(pool: &sqlx::PgPool, namespace: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM playbook_outbox WHERE namespace_id=$1")
        .bind(namespace)
        .fetch_one(pool)
        .await
        .unwrap()
}

fn validator(n: u8) -> ArtifactPin {
    ArtifactPin {
        id: "aq.check.cargo-test".into(),
        version: "1.0.0".into(),
        digest: format!("sha256:{}", format!("{n:x}").repeat(64)),
    }
}
fn check_input(check: &str, v: &ArtifactPin, revision: i64) -> Value {
    json!({"check": {"id": check, "validator": v},
        "source": {"format": "sha1", "hex": "0123456789abcdef0123456789abcdef01234567"},
        "subject": {"id": "work-1", "revision": revision}})
}
fn run(stop: CheckStop, inputs_complete: bool) -> CheckObservation {
    CheckObservation::Finished(CheckRun {
        stop,
        inputs_complete,
        executor_version: "aq-check-executor/1.0.0".into(),
        inputs_digest: format!("sha256:{}", "c".repeat(64)),
        result_artifacts: vec![ArtifactPin {
            id: "check-log".into(),
            version: "1".into(),
            digest: format!("sha256:{}", "d".repeat(64)),
        }],
    })
}

#[tokio::test]
async fn check_request_runs_in_sandbox_and_reports_typed_outcome() {
    let f = common::profiled::fixture().await;
    common::profiled::insert(&f).await;
    let pool = f.db.pool().await;
    let checks = Arc::new(FakeChecks::default());
    let names = ["unit", "lint", "partial", "slow"];
    checks.pinned.lock().unwrap().insert(
        "work-1".into(),
        PinnedOutcome {
            revision: 1,
            required_checks: names
                .iter()
                .enumerate()
                .map(|(i, id)| PinnedCheck {
                    id: (*id).into(),
                    validator: validator(i as u8 + 1),
                })
                .collect(),
        },
    );
    *checks.script.lock().unwrap() = BTreeMap::from([
        ("unit".into(), run(CheckStop::Exited(0), true)),
        ("lint".into(), run(CheckStop::Exited(1), true)),
        // Exit 0 with a missing input is never passed.
        ("partial".into(), run(CheckStop::Exited(0), false)),
        ("slow".into(), CheckObservation::Running),
    ]);
    let h = host(&f, Some(checks.clone()), None).await;
    let ns = h.service.namespace.namespace_id;

    // Accepted with a durable handle; the attempt ran in the sandbox profile.
    let handle = h
        .accepted(
            "check-1",
            "check.request",
            check_input("unit", &validator(1), 1),
        )
        .await;
    assert_eq!(checks.starts(), 1);
    let op: Uuid = handle.parse().unwrap();
    let attempt = checks.attempts.lock().unwrap()[&op].0.clone();
    assert_eq!(attempt.execution_profile, CHECK_EXECUTION_PROFILE);
    assert_eq!(
        (attempt.check_id.as_str(), attempt.subject_revision),
        ("unit", 1)
    );
    assert_eq!(
        attempt.source_hex,
        "0123456789abcdef0123456789abcdef01234567"
    );
    // The durable intent row exists and is complete; the event is sealed.
    let (outcome, output) = h.completed(&handle).await;
    assert_eq!(outcome, "passed");
    assert_eq!(output["disposition"], "passed");
    assert_eq!(
        output["receipt"]["exit"],
        json!({"code": 0, "stop": "exited"})
    );
    assert_eq!(
        output["receipt"]["executor_version"],
        "aq-check-executor/1.0.0"
    );
    assert_eq!(output["receipt"]["result_artifacts"][0]["id"], "check-log");
    assert!(
        output["receipt"]["digest"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
    assert_eq!(output["reference"], format!("check-receipt:{handle}"));
    assert_eq!(output["subject"], json!({"id": "work-1", "revision": 1}));
    assert_eq!(output["check"]["validator"]["digest"], validator(1).digest);
    assert_eq!(outbox(&pool, ns).await, 1, "one aq.check.receipted event");
    // A lost response re-dispatches the same intent: same handle, no new
    // attempt, no second event.
    assert_eq!(
        h.accepted(
            "check-1",
            "check.request",
            check_input("unit", &validator(1), 1)
        )
        .await,
        handle
    );
    assert_eq!(checks.starts(), 1, "never a second writer");
    assert_eq!(h.completed(&handle).await.1, output);
    assert_eq!(outbox(&pool, ns).await, 1);

    // Non-zero exit is failed; a missing input is infrastructure_failure.
    let lint = h
        .accepted(
            "check-2",
            "check.request",
            check_input("lint", &validator(2), 1),
        )
        .await;
    let (outcome, output) = h.completed(&lint).await;
    assert_eq!(
        (outcome.as_str(), &output["disposition"]),
        ("failed", &json!("failed"))
    );
    assert_eq!(
        output["receipt"]["exit"],
        json!({"code": 1, "stop": "exited"})
    );
    let partial = h
        .accepted(
            "check-3",
            "check.request",
            check_input("partial", &validator(3), 1),
        )
        .await;
    let (outcome, output) = h.completed(&partial).await;
    assert_eq!(outcome, "infrastructure_failure");
    assert_eq!(output["disposition"], "unknown");

    // A running attempt reports running until the executor stops it.
    let slow = h
        .accepted(
            "check-4",
            "check.request",
            check_input("slow", &validator(4), 1),
        )
        .await;
    let r = h.get(&slow).await;
    assert_eq!(r.outcome, "running");
    assert_eq!(r.output["reference"], format!("check:{slow}"));
    let CheckObservation::Finished(timed_out) = run(CheckStop::TimedOut, true) else {
        unreachable!()
    };
    checks.finish(slow.parse().unwrap(), timed_out);
    let (outcome, output) = h.completed(&slow).await;
    assert_eq!(outcome, "infrastructure_failure");
    assert_eq!(output["disposition"], "timed_out");
    assert_eq!(
        output["receipt"]["exit"],
        json!({"code": null, "stop": "timed_out"})
    );
    assert_eq!(checks.starts(), 4);
    assert_eq!(outbox(&pool, ns).await, 4);

    // Typed refusals settle synchronously and start nothing.
    let refused = |r: HostResult, reason: &str, disposition: Value| {
        assert_eq!(r.outcome, "refused", "{r:?}");
        assert_eq!(r.output["reason"], reason);
        assert_eq!(r.output["disposition"], disposition);
    };
    refused(
        h.run(
            "check-5",
            "check.request",
            check_input("e2e", &validator(1), 1),
        )
        .await,
        "not_required",
        Value::Null,
    );
    refused(
        h.run(
            "check-6",
            "check.request",
            check_input("unit", &validator(9), 1),
        )
        .await,
        "not_required",
        Value::Null,
    );
    refused(
        h.run(
            "check-7",
            "check.request",
            check_input("unit", &validator(1), 0),
        )
        .await,
        "stale_subject",
        json!("stale"),
    );
    let mut short = check_input("unit", &validator(1), 1);
    short["source"] =
        json!({"format": "sha256", "hex": "0123456789abcdef0123456789abcdef01234567"});
    refused(
        h.run("check-8", "check.request", short).await,
        "invalid_request",
        Value::Null,
    );
    assert_eq!(checks.starts(), 4);
    // At most 8 executions per subject and check.
    for i in 0..7 {
        h.accepted(
            &format!("unit-{i}"),
            "check.request",
            check_input("unit", &validator(1), 1),
        )
        .await;
    }
    assert_eq!(checks.starts(), 11);
    refused(
        h.run(
            "unit-8",
            "check.request",
            check_input("unit", &validator(1), 1),
        )
        .await,
        "execution_limit",
        Value::Null,
    );
    assert_eq!(checks.starts(), 11);
    // Unknown handles are refused, never invented.
    assert_eq!(h.get(&Uuid::new_v4().to_string()).await.outcome, "refused");
    // Undeclared fields (a shell command, a pre-set result) never reach an adapter.
    let mut forged = check_input("unit", &validator(1), 1);
    forged["passed"] = json!(true);
    assert!(matches!(
        h.service
            .host_dispatch(
                &h.runtime,
                h.grant,
                1,
                &h.intent("check-9", "check.request", forged)
            )
            .await,
        Err(Error::Invalid)
    ));

    // An unbound executor is a typed refusal, not a fabricated receipt.
    let unbound = host(&f, None, None).await;
    refused(
        unbound
            .run(
                "check-1",
                "check.request",
                check_input("unit", &validator(1), 1),
            )
            .await,
        "unavailable",
        Value::Null,
    );
}

#[tokio::test]
async fn check_request_cancel_completes_from_executor_stop() {
    let f = common::profiled::fixture().await;
    common::profiled::insert(&f).await;
    let pool = f.db.pool().await;
    let checks = Arc::new(FakeChecks::default());
    checks.pinned.lock().unwrap().insert(
        "work-1".into(),
        PinnedOutcome {
            revision: 1,
            required_checks: ["unit", "slow"]
                .iter()
                .enumerate()
                .map(|(i, id)| PinnedCheck {
                    id: (*id).into(),
                    validator: validator(i as u8 + 1),
                })
                .collect(),
        },
    );
    *checks.script.lock().unwrap() = BTreeMap::from([
        ("unit".into(), run(CheckStop::Exited(0), true)),
        ("slow".into(), CheckObservation::Running),
    ]);
    let judge = Arc::new(FakeJudge::default());
    *judge.replies.lock().unwrap() =
        BTreeMap::from([("fits?".into(), Ok(reply("isolated", 10, 10)))]);
    let h = host(&f, Some(checks.clone()), Some(judge)).await;
    let ns = h.service.namespace.namespace_id;
    let slow_input = || check_input("slow", &validator(2), 1);
    let stopped = || {
        let CheckObservation::Finished(r) = run(CheckStop::Cancelled, true) else {
            unreachable!()
        };
        r
    };
    let cancel = |handle: &str| json!({"handle": handle});
    let work = json!({"id": "work-1", "revision": 1});

    // operation.cancel asks the executor to stop; while the executor still
    // reports running the cancel is only accepted and nothing completes.
    let slow = h.accepted("check-1", "check.request", slow_input()).await;
    let op: Uuid = slow.parse().unwrap();
    let r = h.run("cancel-1", "operation.cancel", cancel(&slow)).await;
    assert_eq!(r.outcome, "accepted", "{r:?}");
    assert_eq!(r.output, json!({"reference": slow, "subject": work}));
    assert_eq!(*checks.cancels.lock().unwrap(), [op]);
    assert_eq!(h.get(&slow).await.outcome, "running");
    assert_eq!(outbox(&pool, ns).await, 0, "no fabricated completion");
    // The executor's cancelled stop alone completes the operation.
    checks.finish(op, stopped());
    let r = h.run("cancel-2", "operation.cancel", cancel(&slow)).await;
    assert_eq!(r.outcome, "cancelled", "{r:?}");
    let (outcome, output) = h.completed(&slow).await;
    assert_eq!(
        (outcome.as_str(), &output["disposition"]),
        ("infrastructure_failure", &json!("cancelled"))
    );
    assert_eq!(
        output["receipt"]["exit"],
        json!({"code": null, "stop": "cancelled"})
    );
    assert_eq!(outbox(&pool, ns).await, 1, "one aq.check.receipted event");
    // A lost response returns the retained cancel result.
    let r = h.run("cancel-1", "operation.cancel", cancel(&slow)).await;
    assert_eq!(r.outcome, "accepted");

    // A check that stopped otherwise is too_late and keeps its outcome.
    let unit = h
        .accepted(
            "check-2",
            "check.request",
            check_input("unit", &validator(1), 1),
        )
        .await;
    let r = h.run("cancel-3", "operation.cancel", cancel(&unit)).await;
    assert_eq!(r.outcome, "too_late", "{r:?}");
    assert_eq!(h.completed(&unit).await.0, "passed");
    // Judgments are not cancellable; unknown handles are refused.
    let judgment = h
        .accepted("judge-1", "judgment.request", judgment_input("fits?", work))
        .await;
    for (i, handle) in [judgment, Uuid::new_v4().to_string()].iter().enumerate() {
        let r = h
            .run(&format!("cancel-r{i}"), "operation.cancel", cancel(handle))
            .await;
        assert_eq!(r.outcome, "refused", "{r:?}");
    }

    // Runner cancellation of the check.request intent itself, on a fresh
    // namespace so each phase stays well inside the intent deadline.
    let h = host(&f, Some(checks.clone()), None).await;
    let runner = |id: &str, input: Value| {
        let intent = h.intent(id, "check.request", input);
        let service = &h.service;
        let runtime = &h.runtime;
        let grant = h.grant;
        async move {
            service
                .check_cancel(runtime, grant, 1, &intent)
                .await
                .unwrap()
        }
    };
    // Before admission: confirmed, and the intent never starts an attempt.
    let starts = checks.starts();
    assert!(matches!(
        runner("check-3", slow_input()).await,
        CancellationResult::Confirmed
    ));
    assert!(matches!(
        h.service
            .host_dispatch(
                &h.runtime,
                h.grant,
                1,
                &h.intent("check-3", "check.request", slow_input())
            )
            .await,
        Err(Error::Forbidden)
    ));
    assert_eq!(checks.starts(), starts);
    // Admitted and running: accepted until the executor reports the stop.
    let slow = h.accepted("check-4", "check.request", slow_input()).await;
    assert!(matches!(
        runner("check-4", slow_input()).await,
        CancellationResult::Accepted
    ));
    assert_eq!(h.get(&slow).await.outcome, "running");
    checks.finish(slow.parse().unwrap(), stopped());
    assert!(matches!(
        runner("check-4", slow_input()).await,
        CancellationResult::Confirmed
    ));
    assert_eq!(h.completed(&slow).await.1["disposition"], "cancelled");
    // Already completed otherwise: too late.
    h.accepted(
        "check-2",
        "check.request",
        check_input("unit", &validator(1), 1),
    )
    .await;
    assert!(matches!(
        runner("check-2", check_input("unit", &validator(1), 1)).await,
        CancellationResult::TooLate
    ));
}

fn judgment_input(question: &str, subject: Value) -> Value {
    json!({"budget": {"deadline_ms": 60000, "max_cost_microunits": 50000,
            "max_input_tokens": 8192, "max_output_tokens": 512},
        "context": {"artifacts": [{"digest": format!("sha256:{}", "a".repeat(64)),
            "id": "snapshot-1", "version": "1"}], "text": "Work item summary."},
        "output": {"choices": ["permitted-default", "isolated"], "rationale_max_bytes": 2048},
        "question": question, "subject": subject})
}
fn reply(choice: &str, output_tokens: u64, cost: u64) -> JudgmentReply {
    JudgmentReply {
        provider: "anthropic".into(),
        model: "claude-sonnet-5-5".into(),
        choice: choice.into(),
        rationale: "Isolation is not required for this change.".into(),
        input_tokens: 900,
        output_tokens,
        cost_microunits: cost,
    }
}

#[tokio::test]
async fn judgment_request_respects_budget_and_records_provenance() {
    let f = common::profiled::fixture().await;
    common::profiled::insert(&f).await;
    let pool = f.db.pool().await;
    let judge = Arc::new(FakeJudge::default());
    *judge.replies.lock().unwrap() = BTreeMap::from([
        ("fits?".into(), Ok(reply("permitted-default", 120, 4000))),
        ("tokens?".into(), Ok(reply("isolated", 513, 4000))),
        ("cost?".into(), Ok(reply("isolated", 100, 50_001))),
        ("off-menu?".into(), Ok(reply("deploy-now", 100, 10))),
        ("down?".into(), Err(ProviderFailure::Unavailable)),
    ]);
    let h = host(&f, None, Some(judge.clone())).await;
    let ns = h.service.namespace.namespace_id;
    let work = json!({"id": "work-1", "revision": 1});

    // Accepted with a durable handle; completes as assessed with provenance.
    let handle = h
        .accepted(
            "judge-1",
            "judgment.request",
            judgment_input("fits?", work.clone()),
        )
        .await;
    let (outcome, output) = h.settled(&handle).await;
    assert_eq!(outcome, "assessed");
    assert_eq!(output["choice"], "permitted-default");
    assert_eq!(output["reference"], format!("judgment:{handle}"));
    assert_eq!(output["subject"], work);
    let p = &output["provenance"];
    assert_eq!(p["provider"], "anthropic");
    assert_eq!(p["model"], "claude-sonnet-5-5");
    assert_eq!(p["credential_profile"], "profile/judgment-default");
    for d in ["prompt_digest", "context_digest", "output_digest"] {
        assert!(p[d].as_str().unwrap().starts_with("sha256:"), "{d}");
    }
    assert_eq!(
        p["usage"],
        json!({"input_tokens": 900, "output_tokens": 120, "cost_microunits": 4000})
    );
    assert!(
        !output.to_string().contains(SECRET),
        "no secret in provenance"
    );
    // The provider saw explicit bounded inputs, the host-chosen profile and
    // the declared budget; no tool loop, one request.
    {
        let requests = judge.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let r = &requests[0];
        assert_eq!(r.credential_profile, "profile/judgment-default");
        assert_eq!(r.choices, ["permitted-default", "isolated"]);
        assert_eq!(r.context_text, "Work item summary.");
        assert_eq!(
            (
                r.max_input_tokens,
                r.max_output_tokens,
                r.max_cost_microunits
            ),
            (8192, 512, 50000)
        );
        assert_eq!(
            r.deadline_ms, 60000,
            "the budget deadline, not the 5 s dispatch deadline: off the dispatch path"
        );
    }
    assert_eq!(
        outbox(&pool, ns).await,
        1,
        "one aq.judgment.completed event"
    );
    // A lost response re-dispatches: same handle, never a second request.
    assert_eq!(
        h.accepted(
            "judge-1",
            "judgment.request",
            judgment_input("fits?", work.clone())
        )
        .await,
        handle
    );
    assert_eq!(judge.requests.lock().unwrap().len(), 1);
    assert_eq!(outbox(&pool, ns).await, 1);

    // Exceeding the token or cost budget is a typed refusal that still
    // records usage, with no output digest.
    for (i, q) in ["tokens?", "cost?"].iter().enumerate() {
        let handle = h
            .accepted(
                &format!("judge-b{i}"),
                "judgment.request",
                judgment_input(q, work.clone()),
            )
            .await;
        let (outcome, output) = h.settled(&handle).await;
        assert_eq!(outcome, "refused", "{q}");
        assert_eq!(output["reason"], "budget_exceeded");
        assert_eq!(output["provenance"]["output_digest"], Value::Null);
        assert_eq!(
            output["provenance"]["credential_profile"],
            "profile/judgment-default"
        );
        assert!(output.get("choice").is_none(), "no judgment past budget");
    }
    // An answer outside the declared output schema is refused.
    let off = h
        .accepted(
            "judge-o",
            "judgment.request",
            judgment_input("off-menu?", work.clone()),
        )
        .await;
    let (outcome, output) = h.settled(&off).await;
    assert_eq!(
        (outcome.as_str(), &output["reason"]),
        ("refused", &json!("invalid_output"))
    );
    // Provider unavailability is a typed failure.
    let down = h
        .accepted(
            "judge-d",
            "judgment.request",
            judgment_input("down?", work.clone()),
        )
        .await;
    let (outcome, output) = h.settled(&down).await;
    assert_eq!(
        (outcome.as_str(), &output["reason"]),
        ("failed", &json!("provider_unavailable"))
    );
    assert_eq!(outbox(&pool, ns).await, 5);

    // A stale graph subject is refused before any provider request.
    let node = Uuid::new_v4();
    h.graph
        .nodes
        .lock()
        .unwrap()
        .insert(node, json!({"version": 3}));
    let r = h
        .run(
            "judge-s",
            "judgment.request",
            judgment_input("fits?", json!({"id": node.to_string(), "revision": 2})),
        )
        .await;
    assert_eq!(
        (r.outcome.as_str(), &r.output["reason"]),
        ("refused", &json!("stale_subject"))
    );
    assert_eq!(judge.requests.lock().unwrap().len(), 5);

    // Pre-call estimate: a prompt expected to exceed max_input_tokens is
    // refused budget_exceeded before any operation or provider request.
    let mut big = judgment_input("fits?", work.clone());
    big["context"]["text"] = json!("x".repeat(32_768));
    let r = h.run("judge-e", "judgment.request", big).await;
    assert_eq!(
        (r.outcome.as_str(), &r.output["reason"]),
        ("refused", &json!("budget_exceeded"))
    );
    assert_eq!(r.output["provenance"], Value::Null);
    assert_eq!(judge.requests.lock().unwrap().len(), 5, "nothing sent");
    assert_eq!(outbox(&pool, ns).await, 5, "no operation completed");

    // The provider runs off the dispatch path: dispatch returns the handle
    // while the provider is still answering, and the operation completes
    // later by its aq.judgment.completed event.
    judge.held.lock().unwrap().insert("held?".into());
    judge
        .replies
        .lock()
        .unwrap()
        .insert("held?".into(), Ok(reply("isolated", 100, 10)));
    let held = h
        .accepted(
            "judge-h",
            "judgment.request",
            judgment_input("held?", work.clone()),
        )
        .await;
    let r = h.get(&held).await;
    assert_eq!(r.outcome, "running", "{r:?}");
    assert_eq!(r.output["reference"], format!("judgment:{held}"));
    assert_eq!(outbox(&pool, ns).await, 5);
    judge.release.notify_one();
    let (outcome, output) = h.settled(&held).await;
    assert_eq!(
        (outcome.as_str(), &output["choice"]),
        ("assessed", &json!("isolated"))
    );
    assert_eq!(judge.requests.lock().unwrap().len(), 6);
    assert_eq!(outbox(&pool, ns).await, 6);

    // With no permitted credential profile the host refuses; it never asks
    // the playbook for one.
    let unbound = host(&f, None, None).await;
    let r = unbound
        .run("judge-1", "judgment.request", judgment_input("fits?", work))
        .await;
    assert_eq!(
        (r.outcome.as_str(), &r.output["reason"]),
        ("refused", &json!("no_permitted_profile"))
    );
    assert_eq!(r.output["provenance"], Value::Null);
}
