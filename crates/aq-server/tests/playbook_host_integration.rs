//! D7 host adapters candidate.prepare / candidate.publish (catalog profiles 7
//! and 8) on disposable PostgreSQL through the production service owners. The D1
//! delivery service and the D6 sandbox executor are in-memory fakes of the
//! injected ports; every evidence item is resolved in its owning store.
mod common;
use aq_playbook_host::runner::{Intent, IntentStatus};
use aq_server::{
    delivery::{PublicationMode, PublicationPolicy},
    playbooks::{
        domain::*,
        host_adapters::{
            ArtifactPin, CheckAttempt, CheckExecutor, CheckObservation, CheckRun, CheckStop,
            EvidencePolicy, HostResult, Integration, IntegrationBinding, PinnedCheck,
            PinnedOutcome, PrepareCall, PrepareReply, PublishCall, PublishReply, ReconcileCall,
            ReconcileReply, ReviewEvidence, WorkGraph,
        },
        review_domain::{CatalogProfile, HOST7_CAPABILITIES},
        service::*,
    },
    protected_storage::{KeyFile, Keyring, ProtectionConfig},
};
use aq_worker_protocol::ObjectId;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    os::unix::fs::PermissionsExt,
    sync::{Arc, Mutex},
};
use uuid::Uuid;

struct FakeGraph;
#[async_trait::async_trait]
impl WorkGraph for FakeGraph {
    async fn node(&self, _: Uuid, _: Uuid) -> Result<Option<Value>> {
        Ok(None)
    }
    async fn mutate(&self, _: Uuid, _: Value) -> Result<(u16, Value)> {
        panic!("D7 adapters never mutate the graph")
    }
}

/// Sandbox executor fake: the scripted stop per check id is reported at once.
#[derive(Default)]
struct FakeChecks {
    pinned: Mutex<BTreeMap<String, PinnedOutcome>>,
    script: BTreeMap<String, CheckObservation>,
    attempts: Mutex<BTreeMap<Uuid, CheckObservation>>,
}
#[async_trait::async_trait]
impl CheckExecutor for FakeChecks {
    async fn pinned(&self, _: Uuid, _: Uuid, subject: &str) -> Result<Option<PinnedOutcome>> {
        Ok(self.pinned.lock().unwrap().get(subject).cloned())
    }
    async fn start(&self, attempt: &CheckAttempt) -> Result<()> {
        let observation = self.script[&attempt.check_id].clone();
        self.attempts
            .lock()
            .unwrap()
            .insert(attempt.operation, observation);
        Ok(())
    }
    async fn observe(&self, operation: Uuid) -> Result<CheckObservation> {
        Ok(self
            .attempts
            .lock()
            .unwrap()
            .get(&operation)
            .cloned()
            .unwrap_or(CheckObservation::NotFound))
    }
    async fn cancel(&self, _: Uuid) -> Result<()> {
        Ok(())
    }
}

/// D1 delivery fake: scripted replies, every call recorded.
struct FakeIntegration {
    binding: Mutex<Option<IntegrationBinding>>,
    prepares: Mutex<Vec<PrepareCall>>,
    publishes: Mutex<Vec<PublishCall>>,
    reconciles: Mutex<Vec<ReconcileCall>>,
    publish_replies: Mutex<VecDeque<PublishReply>>,
    reconcile_reply: Mutex<ReconcileReply>,
    reviews: Mutex<BTreeMap<Uuid, ReviewEvidence>>,
}
#[async_trait::async_trait]
impl Integration for FakeIntegration {
    async fn binding(&self, _: Uuid, _: Uuid) -> Result<Option<IntegrationBinding>> {
        Ok(self.binding.lock().unwrap().clone())
    }
    async fn prepare(&self, call: &PrepareCall) -> Result<PrepareReply> {
        self.prepares.lock().unwrap().push(call.clone());
        Ok(PrepareReply::Prepared {
            candidate: oid('c'),
        })
    }
    async fn publish(&self, call: &PublishCall) -> Result<PublishReply> {
        self.publishes.lock().unwrap().push(call.clone());
        Ok(self
            .publish_replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(PublishReply::Published {
                old: call.expected_old.clone(),
                new: call.candidate.clone(),
            }))
    }
    async fn reconcile(&self, call: &ReconcileCall) -> Result<ReconcileReply> {
        self.reconciles.lock().unwrap().push(call.clone());
        Ok(self.reconcile_reply.lock().unwrap().clone())
    }
    async fn review(&self, _: Uuid, _: Uuid, decision: Uuid) -> Result<Option<ReviewEvidence>> {
        Ok(self.reviews.lock().unwrap().get(&decision).cloned())
    }
}
fn fake(checks: &[&str], review: bool) -> Arc<FakeIntegration> {
    Arc::new(FakeIntegration {
        binding: Mutex::new(Some(IntegrationBinding {
            repository_id: Uuid::new_v4(),
            binding_generation: 3,
            publication: PublicationPolicy {
                mode: PublicationMode::FastForward,
                target: "refs/heads/main".into(),
                wait_for_checks: None,
            },
            evidence: EvidencePolicy {
                checks: checks.iter().map(|c| c.to_string()).collect(),
                review,
                judgment_choices: vec![],
            },
        })),
        prepares: Mutex::default(),
        publishes: Mutex::default(),
        reconciles: Mutex::default(),
        publish_replies: Mutex::default(),
        reconcile_reply: Mutex::new(ReconcileReply::Unresolved),
        reviews: Mutex::default(),
    })
}
fn oid(c: char) -> ObjectId {
    serde_json::from_value(json!({"algorithm": "sha1", "hex": c.to_string().repeat(40)})).unwrap()
}
fn validator() -> ArtifactPin {
    ArtifactPin {
        id: "aq.check.cargo-test".into(),
        version: "1.0.0".into(),
        digest: format!("sha256:{}", "1".repeat(64)),
    }
}
fn finished(code: i32) -> CheckObservation {
    CheckObservation::Finished(CheckRun {
        stop: CheckStop::Exited(code),
        inputs_complete: true,
        executor_version: "aq-check-executor/1.0.0".into(),
        inputs_digest: format!("sha256:{}", "c".repeat(64)),
        result_artifacts: vec![],
    })
}
fn checks() -> Arc<FakeChecks> {
    Arc::new(FakeChecks {
        script: BTreeMap::from([
            ("unit".into(), finished(0)),
            ("extra".into(), finished(0)),
            ("lint".into(), finished(1)),
        ]),
        ..Default::default()
    })
}

struct Host {
    service: Service,
    runtime: RuntimeOwner,
    grant: Uuid,
    scope: GrantScope,
    now: i64,
    profile: CatalogProfile,
}
async fn host(
    f: &common::profiled::Fixture,
    checks: Arc<FakeChecks>,
    integration: Arc<FakeIntegration>,
) -> Host {
    host_with(f, checks, integration, CatalogProfile::Host7).await
}
async fn host_with(
    f: &common::profiled::Fixture,
    checks: Arc<FakeChecks>,
    integration: Arc<FakeIntegration>,
    profile: CatalogProfile,
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
    let namespace = Namespace {
        namespace_id: Uuid::new_v4(),
        generation: 1,
        name: format!("fixture-{}", Uuid::new_v4()),
        workspace_id: l.workspace_id,
        project_id: l.project_id,
        owner_user_id: f.actor,
    };
    let (keys, graph, checks, integration) = (
        Arc::new(keys),
        Arc::new(FakeGraph),
        Some(checks as Arc<dyn CheckExecutor>),
        Some(integration as Arc<dyn Integration>),
    );
    let service = match profile {
        CatalogProfile::Host8 => {
            Service::new_host8_integration(
                &f.state,
                namespace,
                keys,
                graph,
                checks,
                None,
                integration,
            )
            .await
        }
        _ => {
            Service::new_host7_integration(
                &f.state,
                namespace,
                keys,
                graph,
                checks,
                None,
                integration,
            )
            .await
        }
    }
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
        profile,
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
            command: self.profile.registry().commands[name].clone(),
            input_digest: digest(&input).unwrap(),
            input,
            authority: self.profile.authority(self.grant, 1, &self.scope).unwrap(),
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
    async fn try_run(&self, id: &str, name: &str, input: Value) -> Result<HostResult> {
        self.service
            .host_dispatch(&self.runtime, self.grant, 1, &self.intent(id, name, input))
            .await
    }
    async fn run(&self, id: &str, name: &str, input: Value) -> HostResult {
        self.try_run(id, name, input).await.unwrap()
    }
    async fn get(&self, handle: &str) -> HostResult {
        self.run(
            &format!("get-{}", Uuid::new_v4()),
            "operation.get",
            json!({"handle": handle}),
        )
        .await
    }
    /// Prepares the default candidate; returns its reference.
    async fn prepare(&self, id: &str) -> String {
        let r = self
            .run(
                id,
                "candidate.prepare",
                json!({"base": {"id": format!("refs/heads/main@sha1:{}", "a".repeat(40)), "revision": 0},
                    "sources": [{"id": format!("aq.source/{}", Uuid::from_u128(7)), "revision": 1}]}),
            )
            .await;
        assert_eq!(r.outcome, "prepared", "{r:?}");
        // Only catalog 2.1.0 (Host8) declares `source` on `prepared`.
        assert_eq!(
            r.output.get("source").is_some(),
            self.profile == CatalogProfile::Host8
        );
        let reference = r.output["reference"].as_str().unwrap().to_string();
        assert_eq!(r.output["subject"], json!({"id": reference, "revision": 1}));
        reference
    }
    /// Runs one check.request to completion; returns its evidence reference.
    async fn check(&self, id: &str, check: &str, subject: &str, source: &ObjectId) -> String {
        let source = json!({"format": "sha1", "hex": source.hex});
        let r = self
            .run(
                id,
                "check.request",
                json!({"check": {"id": check, "validator": validator()}, "source": source,
                    "subject": {"id": subject, "revision": 1}}),
            )
            .await;
        assert_eq!(r.outcome, "accepted", "{r:?}");
        let handle = r.output["handle"].as_str().unwrap();
        let done = self.get(handle).await;
        assert_eq!(done.outcome, "completed", "{done:?}");
        format!("check-receipt:{handle}")
    }
    fn publish_input(&self, candidate: &str, evidence: &[&str]) -> Value {
        json!({"candidate": {"id": candidate, "revision": 1},
            "destination": {"id": format!("refs/heads/main@{}", oid_text('a')), "revision": 0},
            "evidence": evidence.iter().map(|e| json!({"id": e, "revision": 1})).collect::<Vec<_>>()})
    }
    async fn receipt(&self, reference: &str) -> Value {
        self.service
            .integration_receipt(reference)
            .await
            .unwrap()
            .unwrap()
    }
    /// A publish that must refuse; returns the typed reason in its receipt.
    async fn refusal(&self, id: &str, input: Value) -> String {
        let r = self.run(id, "candidate.publish", input).await;
        assert_eq!(r.outcome, "refused", "{r:?}");
        let receipt = self.receipt(r.output["reference"].as_str().unwrap()).await;
        assert_eq!(receipt["state"], "refused");
        receipt["reason"].as_str().unwrap().to_string()
    }
}
fn oid_text(c: char) -> String {
    format!("sha1:{}", c.to_string().repeat(40))
}
fn pin(checks: &FakeChecks, subject: &str) {
    checks.pinned.lock().unwrap().insert(
        subject.into(),
        PinnedOutcome {
            revision: 1,
            required_checks: ["unit", "extra", "lint"]
                .iter()
                .map(|id| PinnedCheck {
                    id: (*id).into(),
                    validator: validator(),
                })
                .collect(),
        },
    );
}
async fn outbox(pool: &sqlx::PgPool, namespace: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM playbook_outbox WHERE namespace_id=$1")
        .bind(namespace)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn publish_refused_without_required_evidence() {
    let f = common::profiled::fixture().await;
    common::profiled::insert(&f).await;
    let checks = checks();
    let port = fake(&["unit"], true);
    let h = host(&f, checks.clone(), port.clone()).await;
    let candidate = h.prepare("prep-1").await;
    pin(&checks, &candidate);
    pin(&checks, "work-1");
    let input = |evidence: &[&str]| h.publish_input(&candidate, evidence);
    let review = Uuid::new_v4();
    port.reviews.lock().unwrap().insert(
        review,
        ReviewEvidence {
            subject: json!({"id": candidate, "revision": 1}),
            approved: true,
            issuer_authorized: true,
        },
    );
    let review_ref = format!("aq.review.decision/{review}");

    // Nothing, or only the review: the required check is missing.
    assert_eq!(h.refusal("p-none", input(&[])).await, "evidence_missing");
    assert_eq!(
        h.refusal("p-review-only", input(&[&review_ref])).await,
        "evidence_missing"
    );
    // Caller-asserted evidence never resolves in an owning store.
    for (i, asserted) in [
        "approved:true".to_string(),
        "check:passed".into(),
        format!("check-receipt:{}", Uuid::new_v4()),
        format!("aq.review.decision/{}", Uuid::new_v4()),
    ]
    .iter()
    .enumerate()
    {
        assert_eq!(
            h.refusal(&format!("p-asserted-{i}"), input(&[asserted, &review_ref]))
                .await,
            "evidence_unknown",
            "{asserted}"
        );
    }
    // A passing check bound to another subject is stale.
    let other_subject = h
        .check("c-other-subject", "unit", "work-1", &oid('c'))
        .await;
    assert_eq!(
        h.refusal("p-stale-0", input(&[&other_subject, &review_ref]))
            .await,
        "evidence_stale"
    );
    // A failed check never counts, even next to a passing one.
    let unit = h.check("c-unit", "unit", &candidate, &oid('c')).await;
    let lint = h.check("c-lint", "lint", &candidate, &oid('c')).await;
    assert_eq!(
        h.refusal("p-failed", input(&[&unit, &lint, &review_ref]))
            .await,
        "evidence_failed"
    );
    // Only an unrequired check: the required one is still missing.
    let extra = h.check("c-extra", "extra", &candidate, &oid('c')).await;
    assert_eq!(
        h.refusal("p-unrequired", input(&[&extra, &review_ref]))
            .await,
        "evidence_missing"
    );
    // A review from an unauthorized issuer, or of another subject.
    let rogue = Uuid::new_v4();
    let stale_review = Uuid::new_v4();
    port.reviews.lock().unwrap().extend([
        (
            rogue,
            ReviewEvidence {
                subject: json!({"id": candidate, "revision": 1}),
                approved: true,
                issuer_authorized: false,
            },
        ),
        (
            stale_review,
            ReviewEvidence {
                subject: json!({"id": "aq.candidate/other", "revision": 1}),
                approved: true,
                issuer_authorized: true,
            },
        ),
    ]);
    let rogue_ref = format!("aq.review.decision/{rogue}");
    let stale_ref = format!("aq.review.decision/{stale_review}");
    assert_eq!(
        h.refusal("p-rogue", input(&[&unit, &rogue_ref])).await,
        "evidence_unauthorized"
    );
    assert_eq!(
        h.refusal("p-stale-review", input(&[&unit, &stale_ref]))
            .await,
        "evidence_stale"
    );
    // Candidate and destination are checked before any evidence.
    let mut stale_candidate = input(&[&unit, &review_ref]);
    stale_candidate["candidate"]["revision"] = json!(2);
    assert_eq!(
        h.refusal("p-candidate", stale_candidate).await,
        "candidate_stale"
    );
    let mut elsewhere = input(&[&unit, &review_ref]);
    elsewhere["destination"]["id"] = json!(format!("refs/heads/release@{}", oid_text('a')));
    assert_eq!(
        h.refusal("p-destination", elsewhere).await,
        "destination_mismatch"
    );
    assert!(
        port.publishes.lock().unwrap().is_empty(),
        "never dispatched"
    );

    // The complete, bound set is the only one that publishes.
    let r = h
        .run("p-ok", "candidate.publish", input(&[&unit, &review_ref]))
        .await;
    assert_eq!(r.outcome, "published", "{r:?}");
    assert_eq!(port.publishes.lock().unwrap().len(), 1);
}

/// D9 follow-up: a check on a candidate runs on the prepared candidate's OID,
/// whatever input.source says, so its receipt is never evidence_stale.
#[tokio::test]
async fn candidate_check_binds_prepared_source() {
    let f = common::profiled::fixture().await;
    common::profiled::insert(&f).await;
    let checks = checks();
    let port = fake(&["unit"], false);
    let h = host(&f, checks.clone(), port.clone()).await;
    let candidate = h.prepare("prep-1").await;
    pin(&checks, &candidate);
    pin(&checks, "work-1");
    let source = |c: char| json!({"format": "sha1", "hex": c.to_string().repeat(40)});
    let request = |subject: &str, c: char| {
        json!({"check": {"id": "unit", "validator": validator()}, "source": source(c),
            "subject": {"id": subject, "revision": 1}})
    };

    // input.source 'd' is ignored: the check runs on, and echoes, 'c'.
    let r = h
        .run("c-unit", "check.request", request(&candidate, 'd'))
        .await;
    assert_eq!(r.outcome, "accepted", "{r:?}");
    let handle = r.output["handle"].as_str().unwrap().to_string();
    let done = h.get(&handle).await;
    assert_eq!(done.outcome, "completed", "{done:?}");
    assert_eq!(done.output["output"]["source"], source('c'));
    let r = h
        .run(
            "p-ok",
            "candidate.publish",
            h.publish_input(&candidate, &[&format!("check-receipt:{handle}")]),
        )
        .await;
    assert_eq!(r.outcome, "published", "{r:?}");

    // A non-candidate subject keeps its input.source.
    let r = h
        .run("c-work", "check.request", request("work-1", 'd'))
        .await;
    assert_eq!(r.outcome, "accepted", "{r:?}");
    let done = h.get(r.output["handle"].as_str().unwrap()).await;
    assert_eq!(done.output["output"]["source"], source('d'));

    // A candidate reference with no prepared candidate has no bytes to check.
    let unknown = format!("aq.candidate/{}", Uuid::new_v4());
    pin(&checks, &unknown);
    let r = h
        .run("c-unknown", "check.request", request(&unknown, 'c'))
        .await;
    assert_eq!(r.outcome, "refused", "{r:?}");
    assert_eq!(r.output["reason"], "stale_subject");
}

/// Host8 (catalog 2.1.0): prepare emits the candidate's catalog OID as
/// `source`, and a candidate check whose input.source differs is refused
/// instead of silently substituted.
#[tokio::test]
async fn host8_prepare_emits_source_and_check_requires_it() {
    let f = common::profiled::fixture().await;
    common::profiled::insert(&f).await;
    let checks = checks();
    let port = fake(&["unit"], false);
    let h = host_with(&f, checks.clone(), port.clone(), CatalogProfile::Host8).await;
    let source = |c: char| json!({"format": "sha1", "hex": c.to_string().repeat(40)});
    let r = h
        .run(
            "prep-1",
            "candidate.prepare",
            json!({"base": {"id": format!("refs/heads/main@sha1:{}", "a".repeat(40)), "revision": 0},
                "sources": [{"id": format!("aq.source/{}", Uuid::from_u128(7)), "revision": 1}]}),
        )
        .await;
    assert_eq!(r.outcome, "prepared", "{r:?}");
    assert_eq!(r.output["source"], source('c'));
    let candidate = r.output["reference"].as_str().unwrap().to_string();
    pin(&checks, &candidate);
    let request = |c: char| {
        json!({"check": {"id": "unit", "validator": validator()}, "source": source(c),
            "subject": {"id": candidate, "revision": 1}})
    };

    let r = h.run("c-bad", "check.request", request('d')).await;
    assert_eq!(r.outcome, "refused", "{r:?}");
    assert_eq!(r.output["reason"], "invalid_request");

    let r = h.run("c-ok", "check.request", request('c')).await;
    assert_eq!(r.outcome, "accepted", "{r:?}");
    let done = h.get(r.output["handle"].as_str().unwrap()).await;
    assert_eq!(done.outcome, "completed", "{done:?}");
    assert_eq!(done.output["output"]["source"], source('c'));
}

#[tokio::test]
async fn prepare_then_publish_records_receipts() {
    let f = common::profiled::fixture().await;
    common::profiled::insert(&f).await;
    let pool = f.db.pool().await;
    let checks = checks();
    let port = fake(&["unit"], false);
    let h = host(&f, checks.clone(), port.clone()).await;
    let ns = h.service.namespace.namespace_id;
    let binding = port.binding.lock().unwrap().clone().unwrap();

    // Prepare: the intent's stable operation is the D1 candidate operation.
    let before = outbox(&pool, ns).await;
    let candidate = h.prepare("prep-1").await;
    assert_eq!(
        outbox(&pool, ns).await,
        before + 1,
        "aq.integration.changed"
    );
    let prepared = port.prepares.lock().unwrap()[0].clone();
    assert_eq!(candidate, format!("aq.candidate/{}", prepared.operation));
    assert_eq!(prepared.base, oid('a'));
    assert_eq!(prepared.sources, vec![Uuid::from_u128(7)]);
    assert_eq!(
        (prepared.repository_id, prepared.binding_generation),
        (binding.repository_id, 3)
    );
    // Keyed-idempotent: re-dispatch returns the retained result, no new call.
    h.prepare("prep-1").await;
    assert_eq!(port.prepares.lock().unwrap().len(), 1);
    let receipt = h.receipt(&candidate).await;
    assert_eq!(receipt["state"], "prepared");
    assert_eq!(receipt["command"], "candidate.prepare");
    assert_eq!(receipt["detail"]["candidate"], json!(oid('c')));

    // Publish with a passing check bound to the candidate and its OID.
    pin(&checks, &candidate);
    let unit = h.check("c-unit", "unit", &candidate, &oid('c')).await;
    let before = outbox(&pool, ns).await;
    let input = h.publish_input(&candidate, &[&unit]);
    let r = h.run("pub-1", "candidate.publish", input.clone()).await;
    assert_eq!(r.outcome, "published", "{r:?}");
    let publication = r.output["reference"].as_str().unwrap().to_string();
    assert!(publication.starts_with("aq.publication/"));
    assert_eq!(r.output["subject"], json!({"id": candidate, "revision": 1}));
    assert_eq!(
        outbox(&pool, ns).await,
        before + 1,
        "aq.integration.changed"
    );
    let call = port.publishes.lock().unwrap()[0].clone();
    assert_eq!(publication, format!("aq.publication/{}", call.operation));
    assert_eq!(call.candidate_operation, prepared.operation);
    assert_eq!(
        (call.candidate.clone(), call.expected_old.clone()),
        (oid('c'), oid('a'))
    );
    assert_eq!(call.publication, binding.publication);
    // Retained: no second push for the same intent.
    assert_eq!(h.run("pub-1", "candidate.publish", input.clone()).await, r);
    assert_eq!(port.publishes.lock().unwrap().len(), 1);

    // Receipts are visible in operation.get and inspection.
    for reference in [&candidate, &publication] {
        let got = h.get(reference).await;
        assert_eq!(got.outcome, "completed", "{got:?}");
        assert_eq!(got.output["outcome"], "completed");
        assert_eq!(got.output["output"]["disposition"], "accepted");
        assert_eq!(got.output["output"]["reference"], json!(reference));
    }
    let receipt = h.receipt(&publication).await;
    assert_eq!(receipt["state"], "published");
    assert_eq!(receipt["command"], "candidate.publish");
    assert_eq!(receipt["candidate_operation"], json!(prepared.operation));
    assert_eq!(receipt["request"]["evidence"], json!([unit]));
    assert_eq!(receipt["detail"]["new"], json!(oid('c')));

    // A lost reply is reconciled under a new operation, never re-pushed.
    port.publish_replies
        .lock()
        .unwrap()
        .push_back(PublishReply::Ambiguous);
    assert!(matches!(
        h.try_run("pub-2", "candidate.publish", input.clone()).await,
        Err(Error::Unknown)
    ));
    let lost = port.publishes.lock().unwrap()[1].operation;
    let lost_ref = format!("aq.publication/{lost}");
    assert_eq!(h.get(&lost_ref).await.outcome, "indeterminate");
    assert_eq!(h.receipt(&lost_ref).await["state"], "ambiguous");
    assert!(matches!(
        h.try_run("pub-2", "candidate.publish", input.clone()).await,
        Err(Error::Unknown)
    ));
    *port.reconcile_reply.lock().unwrap() = ReconcileReply::Present { observed: oid('c') };
    let r = h.run("pub-2", "candidate.publish", input.clone()).await;
    assert_eq!(r.outcome, "published", "{r:?}");
    assert_eq!(
        port.publishes.lock().unwrap().len(),
        2,
        "never pushed twice"
    );
    let reconciles = port.reconciles.lock().unwrap().clone();
    assert_eq!(reconciles.len(), 2);
    assert!(reconciles.iter().all(|c| c.target_operation == lost));
    assert_ne!(reconciles[0].operation, reconciles[1].operation);
    assert!(reconciles.iter().all(|c| c.operation != lost));
    let receipt = h.receipt(&lost_ref).await;
    assert_eq!(receipt["state"], "published");
    assert_eq!(receipt["reconcile_attempts"], 2);
    assert_eq!(
        h.get(&lost_ref).await.output["output"]["disposition"],
        "accepted"
    );
    // Settled: the existing disposition, with no further reconcile.
    assert_eq!(h.run("pub-2", "candidate.publish", input).await, r);
    assert_eq!(port.reconciles.lock().unwrap().len(), 2);
}
