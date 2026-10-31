//! Compiled pilot graphs driven by the domain-enforcing fake services, rather
//! than queues of unconditional scripted command successes.
use aq_playbooks_cli::{
    catalog,
    fake_host::{Episode, FakeHost, Observation, Subject},
    pilots,
};
use aq_playbooks_runner::{Engine, IntentStatus, MemoryStorage, RunStatus};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

fn setup(name: &str) -> (Engine<MemoryStorage>, FakeHost, String) {
    let scenario = pilots::scenario(name).unwrap();
    let engine = Engine::new(
        MemoryStorage::default(),
        format!("host-{name}"),
        "runtime-1",
    )
    .unwrap();
    for artifact in &scenario.artifacts {
        engine.install(artifact.clone()).unwrap();
    }
    for (binding, digest) in &scenario.bindings {
        engine
            .activate(binding, digest, scenario.authority.clone())
            .unwrap();
    }
    let start = &scenario.starts[0];
    let root = engine
        .admit(
            &start.binding,
            &start.entry,
            start.input.clone(),
            start.authority.clone(),
            0,
        )
        .unwrap();
    let mut host = FakeHost::default();
    host.grant_all("pilot-user", &["project"]);
    host.visibility
        .insert("supervisor".into(), BTreeSet::from(["project".into()]));
    host.subjects.insert("candidate-1".into(), 1);
    host.subjects.insert("main".into(), 1);
    let view = match name {
        "routing" => "execution_options",
        "watcher" => "health_observations",
        _ => "project",
    };
    host.capture_observation(Observation {
        reference: "snapshot-1".into(),
        partition: "project-health".into(),
        scope: "project".into(),
        visibility: "project".into(),
        view: view.into(),
        subject: Subject {
            id: "candidate-1".into(),
            revision: 1,
        },
        pages: vec![json!({"synthetic":true})],
        complete: true,
        observed_at_ms: 0,
        expires_at_ms: 60_000,
        episode_revisions: BTreeMap::new(),
    })
    .unwrap();
    (engine, host, root)
}

/// Explicitly configured fake checks and judgments. Async work remains pending
/// until each test supplies an authoritative domain completion below.
fn drive(
    engine: &Engine<MemoryStorage>,
    host: &mut FakeHost,
    now: i64,
    checks: bool,
    judgment: bool,
) {
    drive_with_hook(engine, host, now, checks, judgment, |_, _| {});
}

fn drive_with_hook(
    engine: &Engine<MemoryStorage>,
    host: &mut FakeHost,
    now: i64,
    checks: bool,
    judgment: bool,
    mut before_dispatch: impl FnMut(&Engine<MemoryStorage>, &mut FakeHost),
) {
    host.now_ms = now as u64;
    for _ in 0..300 {
        let mut changed = false;
        let ids: Vec<_> = engine.snapshot().unwrap().runs.keys().cloned().collect();
        for id in ids {
            changed |= engine.advance(&id, now).unwrap();
        }
        for intent in engine
            .snapshot()
            .unwrap()
            .intents
            .values()
            .filter(|i| i.status == IntentStatus::Prepared)
        {
            if checks && intent.command.name == "check.request" {
                let subject: Subject =
                    serde_json::from_value(intent.input["subject"].clone()).unwrap();
                host.configured_outcomes.insert(
                    FakeHost::check_key(&subject, intent.input["suite"].as_str().unwrap()),
                    "passed".into(),
                );
            }
            if judgment && intent.command.name == "judgment.request" {
                host.configured_outcomes
                    .insert(intent.id.clone(), "assessed".into());
                host.configured_choices
                    .insert(intent.id.clone(), "permitted-default".into());
            }
        }
        before_dispatch(engine, host);
        changed |= engine.dispatch_next(host, now).unwrap().is_some();
        if !changed {
            return;
        }
    }
    panic!("pilot exceeded integration transition bound");
}

fn complete(
    engine: &Engine<MemoryStorage>,
    host: &mut FakeHost,
    kind: &str,
    disposition: &str,
    now: i64,
) {
    let work = host
        .work
        .values()
        .find(|work| work.kind == kind && work.completion.is_none())
        .unwrap()
        .clone();
    host.now_ms = now as u64;
    let event = host
        .complete_work(
            &work.reference,
            disposition,
            &format!("domain-result:{}", work.reference),
        )
        .unwrap();
    engine
        .admit_event("aq.operations", event.clone(), now)
        .unwrap();
    assert!(
        engine
            .admit_event("aq.operations", event, now)
            .unwrap()
            .duplicate
    );
}

#[test]
fn routing_uses_authorized_assessment_and_host_revision_preconditions() {
    let (engine, mut host, root) = setup("routing");
    drive(&engine, &mut host, 0, false, true);
    let state = engine.snapshot().unwrap();
    assert_eq!(state.runs[&root].outcome.as_deref(), Some("success"));
    assert_eq!(host.subjects["candidate-1"], 2);
    let change = state
        .intents
        .values()
        .find(|intent| intent.command.name == "work.change")
        .unwrap();
    assert_eq!(change.input["value"], "permitted-default");
    assert!(
        state
            .intents
            .values()
            .any(|intent| intent.command.name == "judgment.request")
    );
}

#[test]
fn watcher_reconciles_host_attested_observation_and_recovers_atomic_notifications() {
    let (engine, mut host, root) = setup("watcher");
    let findings = json!([{"key":"stuck-work","evidence":"work-1","expected_revision":0}]);
    host.record_evaluation("evaluation-1", "snapshot-1", &findings)
        .unwrap();
    drive(&engine, &mut host, 0, false, false);
    assert_eq!(
        engine.snapshot().unwrap().runs[&root].outcome.as_deref(),
        Some("success")
    );
    assert!(host.episodes["project-health/stuck-work"].active);
    assert_eq!(host.outbox.len(), 1);
    host.deliver_notifications(1, false).unwrap();
    host = host.restart();
    host.deliver_notifications(1, true).unwrap();
    assert_eq!(host.recipient_receipts.len(), 1);
    assert_eq!(host.episodes.len(), 1);
}

#[test]
fn watcher_partial_positive_scan_keeps_unseen_episode_open() {
    let (engine, mut host, root) = setup("watcher");
    host.observations.clear();
    host.episodes.insert(
        "project-health/unseen".into(),
        Episode {
            revision: 3,
            active: true,
            evidence: "prior-complete-scan".into(),
            last_notification_ms: 0,
        },
    );
    host.capture_observation(Observation {
        reference: "snapshot-1".into(),
        partition: "project-health".into(),
        scope: "project".into(),
        visibility: "project".into(),
        view: "health_observations".into(),
        subject: Subject {
            id: "candidate-1".into(),
            revision: 1,
        },
        pages: vec![json!({"partial":true})],
        complete: false,
        observed_at_ms: 0,
        expires_at_ms: 60_000,
        episode_revisions: BTreeMap::new(),
    })
    .unwrap();
    host.record_evaluation(
        "evaluation-1",
        "snapshot-1",
        &json!([{"key":"stuck-work","evidence":"work-1","expected_revision":0}]),
    )
    .unwrap();
    drive(&engine, &mut host, 0, false, false);
    assert_eq!(
        engine.snapshot().unwrap().runs[&root].outcome.as_deref(),
        Some("success")
    );
    assert!(host.episodes["project-health/stuck-work"].active);
    assert!(host.episodes["project-health/unseen"].active);
    assert_eq!(host.episodes["project-health/unseen"].revision, 3);
}

#[test]
fn coding_admission_and_completed_review_with_changes_cannot_publish() {
    let (engine, mut host, root) = setup("coding");
    drive(&engine, &mut host, 0, true, false);
    assert_eq!(host.work.len(), 1);
    assert!(host.candidates.is_empty());
    assert!(host.evidence.is_empty());
    assert!(!engine.snapshot().unwrap().runs[&root].status.terminal());
    complete(&engine, &mut host, "implementation", "accepted", 1);
    drive(&engine, &mut host, 1, true, false);
    assert!(host.work.values().any(|work| work.kind == "review"));
    assert!(host.candidates.is_empty());
    complete(&engine, &mut host, "review", "changes_requested", 2);
    drive(&engine, &mut host, 2, true, false);
    let state = engine.snapshot().unwrap();
    assert_eq!(state.runs[&root].outcome.as_deref(), Some("escalated"));
    assert!(
        state
            .intents
            .values()
            .all(|intent| intent.command.name != "candidate.publish")
    );
    assert_eq!(host.subjects["main"], 1);
}

#[test]
fn coding_requires_explicit_accepted_review_and_exact_check_before_independent_followup() {
    let (engine, mut host, root) = setup("coding");
    drive(&engine, &mut host, 0, true, false);
    complete(&engine, &mut host, "implementation", "accepted", 1);
    drive(&engine, &mut host, 1, true, false);
    complete(&engine, &mut host, "review", "accepted", 2);
    drive(&engine, &mut host, 2, true, false);
    let state = engine.snapshot().unwrap();
    assert_eq!(state.runs[&root].outcome.as_deref(), Some("success"));
    assert_eq!(host.subjects["main"], 2);
    assert!(!host.evidence.is_empty());
    assert_eq!(host.messages.len(), 1);
    assert!(state.runs.values().any(|run| run.origin.is_some()
        && run.parent.is_none()
        && run.status == RunStatus::Completed));
    assert!(
        host.work
            .values()
            .filter(|work| work.kind == "review")
            .all(|work| work.completion.as_ref().unwrap()["disposition"] == "accepted")
    );
}

#[test]
fn coding_real_publication_conflict_observes_new_base_repairs_and_checks_new_candidate() {
    let (engine, mut host, root) = setup("coding");
    drive(&engine, &mut host, 0, true, false);
    complete(&engine, &mut host, "implementation", "accepted", 1);
    drive(&engine, &mut host, 1, true, false);
    complete(&engine, &mut host, "review", "accepted", 2);
    let mut conflicted = false;
    drive_with_hook(&engine, &mut host, 2, true, false, |engine, host| {
        if !conflicted
            && engine.snapshot().unwrap().intents.values().any(|intent| {
                intent.command.name == "candidate.publish"
                    && intent.status == IntentStatus::Prepared
            })
        {
            // A concurrent domain writer advances the destination after this
            // immutable candidate was prepared and checked against main@1.
            assert!(
                host.candidates
                    .values()
                    .any(|candidate| candidate.base.revision == 1)
            );
            host.subjects.insert("main".into(), 2);
            conflicted = true;
        }
    });
    assert!(conflicted);
    assert_eq!(host.subjects["main"], 2);
    assert_eq!(host.candidates.len(), 1);
    assert!(
        host.work
            .values()
            .any(|work| work.kind == "repair" && work.completion.is_none())
    );
    complete(&engine, &mut host, "repair", "accepted", 3);
    drive(&engine, &mut host, 3, true, false);
    let state = engine.snapshot().unwrap();
    assert_eq!(state.runs[&root].outcome.as_deref(), Some("success"));
    assert_eq!(host.subjects["main"], 3);
    assert_eq!(host.candidates.len(), 2);
    assert_eq!(
        host.observations
            .values()
            .filter(|observation| observation.subject.id == "main")
            .map(|observation| observation.subject.revision)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([1, 2])
    );
    let candidates: BTreeSet<_> = host.candidates.keys().cloned().collect();
    assert_eq!(
        host.evidence
            .values()
            .filter(|evidence| candidates.contains(&evidence.subject.id) && evidence.passed)
            .count(),
        2
    );
    assert_eq!(
        state
            .intents
            .values()
            .filter(|intent| intent.command.name == "candidate.publish")
            .count(),
        2
    );
    assert_eq!(
        host.work
            .values()
            .filter(|work| work.kind == "repair")
            .count(),
        1
    );
}

#[test]
fn report_deadline_cancels_owned_author_and_late_completion_keeps_one_message() {
    let (engine, mut host, root) = setup("report");
    drive(&engine, &mut host, 0, false, false);
    assert_eq!(host.work.len(), 1);
    assert!(host.messages.is_empty());
    drive(&engine, &mut host, 1001, false, false);
    assert_eq!(host.messages.len(), 1);
    let author = host.work.values().next().unwrap().clone();
    assert!(author.cancelled);
    complete(&engine, &mut host, "supervisor_authoring", "accepted", 2000);
    drive(&engine, &mut host, 2000, false, false);
    assert_eq!(host.messages.len(), 1);
    let state = engine.snapshot().unwrap();
    assert_eq!(state.runs[&root].outcome.as_deref(), Some("success"));
    assert!(
        state
            .intents
            .values()
            .filter(|intent| intent.command.name == "work.ensure")
            .any(|intent| !intent.late_results.is_empty())
    );
}

#[test]
fn every_pilot_uses_the_same_twelve_captured_host_contracts() {
    let registry = catalog::catalog();
    for name in pilots::NAMES {
        for artifact in pilots::scenario(name).unwrap().artifacts {
            for (command, contract) in &artifact.content.registry.commands {
                assert_eq!(
                    Some(contract),
                    registry.commands.get(command),
                    "pilot-specific contract mutation in {name}: {command}"
                );
            }
        }
    }
    assert_eq!(registry.commands.len(), 12);
}
