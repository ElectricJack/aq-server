//! The fake host's own tests. They were its module's unit tests while it was
//! part of the CLI's library; `tests/host_pilots.rs` runs them, once.
use crate::fake_host::*;
use aq_playbooks_cli::catalog;
use aq_playbooks_model::digest;
use aq_playbooks_runner::{
    Authority, CancellationResult, DispatchResult, Host, Intent, IntentStatus,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
fn subject() -> Subject {
    Subject {
        id: "project:sample".into(),
        revision: 1,
    }
}
fn fixture() -> FakeHost {
    let mut h = FakeHost {
        now_ms: 10,
        ..FakeHost::default()
    };
    h.subjects.insert(subject().id, 1);
    h.grant_all("alice", &["team"]);
    h.grant_all("bob", &["team"]);
    h.visibility
        .insert("supervisor".into(), BTreeSet::from(["team".into()]));
    h
}
fn intent(id: &str, name: &str, input: Value) -> Intent {
    Intent {
        id: id.into(),
        run: "run".into(),
        step: "step".into(),
        invocation: 1,
        deadline_at_ms: 60_000,
        command: catalog::catalog().commands[name].clone(),
        input_digest: digest(&input).unwrap(),
        input,
        authority: Authority {
            actor: "alice".into(),
            scope: "project".into(),
            capabilities: catalog::catalog().commands.keys().cloned().collect(),
        },
        status: IntentStatus::Prepared,
        cancel_requested: false,
        cancel_dispatched: false,
        late_results: vec![],
        completion_event: None,
        dispatch_sequence: None,
        handle: None,
        result: None,
        reason: None,
    }
}
fn outcome(response: DispatchResult) -> (String, Value) {
    match response {
        DispatchResult::Completed { outcome, output } => (outcome, output),
        other => panic!("expected completion, got {other:?}"),
    }
}
fn observation(reference: &str, complete: bool, expires: u64) -> Observation {
    Observation {
        reference: reference.into(),
        partition: "watcher".into(),
        scope: "project".into(),
        visibility: "team".into(),
        view: "health_observations".into(),
        subject: subject(),
        pages: vec![json!({"subject":"project:sample","stuck":true})],
        complete,
        observed_at_ms: 1,
        expires_at_ms: expires,
        episode_revisions: BTreeMap::new(),
    }
}
fn reconcile(id: &str, reference: &str, findings: Value) -> Intent {
    intent(
        id,
        "findings.reconcile",
        json!({"partition":"watcher","observation":reference,"evaluation":id,"findings":findings,"reminder_ms":100}),
    )
}
fn evaluate(h: &mut FakeHost, id: &str, reference: &str, findings: Value) -> DispatchResult {
    h.record_evaluation(id, reference, &findings).unwrap();
    h.dispatch(&reconcile(id, reference, findings))
}
fn finding(revision: u64) -> Value {
    json!([{"key":"stuck","evidence":"diagnostic:revision1","expected_revision":revision}])
}
fn open_episode(h: &mut FakeHost) {
    h.capture_observation(observation("scan1", true, 1000))
        .unwrap();
    assert_eq!(
        outcome(evaluate(h, "open", "scan1", finding(0))).0,
        "reconciled"
    );
}

#[test]
fn findings_suppress_unchanged_then_remind_and_clear_from_complete_fresh_baselines() {
    let mut h = fixture();
    open_episode(&mut h);
    assert_eq!(h.outbox.len(), 1);
    assert_eq!(h.episodes["watcher/stuck"].revision, 1);
    h.capture_observation(observation("scan2", true, 1000))
        .unwrap();
    evaluate(&mut h, "unchanged", "scan2", finding(1));
    assert_eq!(h.outbox.len(), 1);
    assert_eq!(h.episodes["watcher/stuck"].revision, 1);
    h.now_ms = 110;
    h.capture_observation(observation("scan3", true, 1000))
        .unwrap();
    evaluate(&mut h, "reminder", "scan3", finding(1));
    assert_eq!(h.outbox.len(), 2);
    assert_eq!(h.episodes["watcher/stuck"].revision, 2);
    h.capture_observation(observation("scan4", true, 1000))
        .unwrap();
    evaluate(&mut h, "clear", "scan4", json!([]));
    assert!(!h.episodes["watcher/stuck"].active);
    assert_eq!(h.outbox.len(), 3);
}

#[test]
fn partial_expired_stale_or_invisible_observations_cannot_clear_findings() {
    let mut h = fixture();
    open_episode(&mut h);
    h.capture_observation(observation("partial", false, 1000))
        .unwrap();
    h.capture_observation(observation("expired", true, 5))
        .unwrap();
    let mut invisible = observation("invisible", true, 1000);
    invisible.visibility = "restricted".into();
    h.capture_observation(invisible).unwrap();
    assert_eq!(
        outcome(evaluate(&mut h, "partial", "partial", json!([]))).0,
        "reconciled"
    );
    assert!(h.episodes["watcher/stuck"].active);
    for reference in ["expired", "invisible"] {
        assert!(matches!(
            evaluate(&mut h, reference, reference, json!([])),
            DispatchResult::Rejected { .. }
        ));
        assert!(h.episodes["watcher/stuck"].active);
    }
    h.capture_observation(observation("stale", true, 1000))
        .unwrap();
    h.capture_observation(observation("changed", true, 1000))
        .unwrap();
    let changed = json!([{"key":"stuck","evidence":"diagnostic:revision2","expected_revision":1}]);
    evaluate(&mut h, "change", "changed", changed);
    assert!(matches!(
        evaluate(&mut h, "stale-clear", "stale", json!([])),
        DispatchResult::Rejected { .. }
    ));
    assert!(h.episodes["watcher/stuck"].active);
    assert_eq!(h.episodes["watcher/stuck"].revision, 2);
}

#[test]
fn findings_host_commit_and_recipient_ack_crashes_keep_one_notification() {
    let mut h = fixture();
    h.capture_observation(observation("scan", true, 1000))
        .unwrap();
    let request = reconcile("evaluation", "scan", finding(0));
    h.record_evaluation("evaluation", "scan", &finding(0))
        .unwrap();
    h.lose_next_response = true;
    assert!(matches!(
        h.dispatch(&request),
        DispatchResult::Unknown { .. }
    ));
    assert_eq!(h.episodes.len(), 1);
    assert_eq!(h.outbox.len(), 1);
    h = h.restart();
    assert_eq!(outcome(h.reconcile(&request)).0, "reconciled");
    assert_eq!(h.outbox.len(), 1);
    h.deliver_notifications(1, false).unwrap();
    assert_eq!(h.recipient_receipts.len(), 1);
    assert!(!h.outbox.values().next().unwrap().acknowledged);
    h = h.restart();
    h.deliver_notifications(1, true).unwrap();
    h.deliver_notifications(1, true).unwrap();
    assert_eq!(h.recipient_receipts.len(), 1);
    assert!(h.outbox.values().next().unwrap().acknowledged);
}

#[test]
fn partial_observation_can_add_positive_findings_without_clearing_unseen_episodes() {
    let mut h = fixture();
    open_episode(&mut h);
    h.capture_observation(observation("partial-positive", false, 1000))
        .unwrap();
    let findings =
        json!([{"key":"new-alert","evidence":"observed:positive","expected_revision":0}]);
    assert_eq!(
        outcome(evaluate(
            &mut h,
            "partial-evaluation",
            "partial-positive",
            findings
        ))
        .0,
        "reconciled"
    );
    assert!(h.episodes["watcher/new-alert"].active);
    assert!(h.episodes["watcher/stuck"].active);
    assert_eq!(h.outbox.len(), 2);
}

#[test]
fn findings_require_exact_host_evaluation_and_fake_refuses_other_scopes() {
    let mut h = fixture();
    h.capture_observation(observation("scan", true, 1000))
        .unwrap();
    let request = reconcile("evaluation", "scan", finding(0));
    assert!(matches!(
        h.dispatch(&request),
        DispatchResult::Rejected { .. }
    ));
    assert!(h.episodes.is_empty());
    h.record_evaluation("evaluation", "scan", &json!([]))
        .unwrap();
    assert!(matches!(
        h.dispatch(&request),
        DispatchResult::Rejected { .. }
    ));
    assert!(h.episodes.is_empty());
    let mut other = observation("other-scope", true, 1000);
    other.scope = "other-project".into();
    assert!(h.capture_observation(other).is_err());
    let mut forged = request;
    forged.authority.scope = "other-project".into();
    assert!(matches!(
        h.dispatch(&forged),
        DispatchResult::Rejected { .. }
    ));
    h.capture_observation(observation("another-scan", true, 1000))
        .unwrap();
    h.record_evaluation("other-evaluation", "another-scan", &finding(0))
        .unwrap();
    let mut mismatch = reconcile("another-operation", "scan", finding(0));
    mismatch.input["evaluation"] = "other-evaluation".into();
    assert!(matches!(
        h.dispatch(&mismatch),
        DispatchResult::Rejected { .. }
    ));
    assert!(h.episodes.is_empty());
}

fn prepare(h: &mut FakeHost) -> Subject {
    let response = h.dispatch(&intent(
        "prepare",
        "candidate.prepare",
        json!({"sources":[subject()],"base":subject()}),
    ));
    let (_, output) = outcome(response);
    parse_subject(&output["subject"]).unwrap()
}
fn check(h: &mut FakeHost, candidate: &Subject) {
    h.configured_outcomes
        .insert(FakeHost::check_key(candidate, "unit"), "passed".into());
    assert_eq!(
        outcome(h.dispatch(&intent(
            "check",
            "check.request",
            json!({"subject":candidate,"suite":"unit","execution_context":"synthetic"})
        )))
        .0,
        "passed"
    );
}
fn publication(id: &str, candidate: &Subject, evidence: &Subject) -> Intent {
    intent(
        id,
        "candidate.publish",
        json!({"candidate":candidate,"destination":subject(),"evidence":[evidence]}),
    )
}
fn hold(id: &str, variant: &str, hold_id: &str) -> Intent {
    intent(
        id,
        "work.change",
        json!({"variant":variant,"subject":subject(),"value":"publication hold","hold_id":hold_id}),
    )
}

#[test]
fn publication_requires_exact_revision_evidence_and_current_destination() {
    let mut h = fixture();
    let candidate = prepare(&mut h);
    check(&mut h, &candidate);
    assert!(matches!(
        h.dispatch(&publication("wrong-subject", &candidate, &subject())),
        DispatchResult::Rejected { .. }
    ));
    let wrong_revision = Subject {
        id: candidate.id.clone(),
        revision: 2,
    };
    assert!(
        matches!(h.dispatch(&publication("wrong-revision",&wrong_revision,&wrong_revision)),DispatchResult::Completed{outcome,..}if outcome=="conflict")
    );
    h.subjects.insert(subject().id, 2);
    assert_eq!(
        outcome(h.dispatch(&publication("stale-destination", &candidate, &candidate))).0,
        "conflict"
    );
    assert_eq!(h.subjects[&subject().id], 2);
}

#[test]
fn competing_holds_require_each_owners_release_and_publication_reconciles_once() {
    let mut h = fixture();
    let candidate = prepare(&mut h);
    check(&mut h, &candidate);
    h.dispatch(&hold("hold-a", "hold", "a"));
    let mut bob = hold("hold-b", "hold", "b");
    bob.authority.actor = "bob".into();
    h.dispatch(&bob);
    assert!(matches!(
        h.dispatch(&hold("forged-release", "release_hold", "b")),
        DispatchResult::Rejected { .. }
    ));
    h.dispatch(&hold("release-a", "release_hold", "a"));
    assert!(matches!(
        h.dispatch(&publication("held", &candidate, &candidate)),
        DispatchResult::Rejected { .. }
    ));
    let mut release_b = hold("release-b", "release_hold", "b");
    release_b.authority.actor = "bob".into();
    h.dispatch(&release_b);
    let publish = publication("publish", &candidate, &candidate);
    h.lose_next_response = true;
    assert!(matches!(
        h.dispatch(&publish),
        DispatchResult::Unknown { .. }
    ));
    assert_eq!(h.subjects[&subject().id], 2);
    h = h.restart();
    assert_eq!(outcome(h.reconcile(&publish)).0, "published");
    assert_eq!(h.subjects[&subject().id], 2);
}

#[test]
fn finite_frozen_context_pages_and_report_work_respect_visibility() {
    let mut h = fixture();
    let mut scan = observation("report-context", true, 1000);
    scan.view = "project".into();
    scan.pages.push(json!({"second":"page"}));
    h.capture_observation(scan).unwrap();
    let context = |id: &str, page: u64| {
        intent(
            id,
            "context.read",
            json!({"view":"project","subject":subject(),"snapshot":"report-context","page":page}),
        )
    };
    let (first, output) = outcome(h.dispatch(&context("page0", 0)));
    assert_eq!(first, "incomplete");
    assert_eq!(output["next_page"], 1);
    assert_eq!(outcome(h.dispatch(&context("page1", 1))).0, "observed");
    assert!(matches!(
        h.dispatch(&context("outside", 2)),
        DispatchResult::Rejected { .. }
    ));
    let mut forbidden = context("forbidden", 0);
    forbidden.authority.actor = "outsider".into();
    h.grants
        .insert("outsider".into(), h.grants["alice"].clone());
    assert!(matches!(
        h.dispatch(&forbidden),
        DispatchResult::Rejected { .. }
    ));
    let work = intent(
        "author",
        "work.ensure",
        json!({"kind":"supervisor_authoring","subject":subject(),"logical_key":"report:2026-10-30","lifetime":"owned","context":"report-context","visibility":"team"}),
    );
    h.dispatch(&work);
    h = h.restart();
    h.dispatch(&work);
    assert_eq!(h.work.len(), 1);
    assert!(
        h.complete_author_work(
            "project/report:2026-10-30",
            "restricted material".into(),
            "restricted"
        )
        .is_err()
    );
    h.complete_author_work(
        "project/report:2026-10-30",
        "permitted report".into(),
        "team",
    )
    .unwrap();
    let message = intent(
        "fallback",
        "message.send",
        json!({"identity":"report:2026-10-30","recipient":"supervisor","visibility":"team","body":"fallback"}),
    );
    h.dispatch(&message);
    let mut late = message.clone();
    late.id = "late-authored".into();
    late.input["body"] = "late authored report".into();
    assert!(matches!(h.dispatch(&late), DispatchResult::Rejected { .. }));
    assert_eq!(h.messages.len(), 1);
    assert_eq!(h.messages["report:2026-10-30"]["body"], "fallback");
}

#[test]
fn missing_fake_checks_remain_unknown_and_decision_revocation_prevents_replay() {
    let mut h = fixture();
    let check = intent(
        "unconfigured",
        "check.request",
        json!({"subject":subject(),"suite":"unit","execution_context":"synthetic"}),
    );
    assert!(matches!(h.dispatch(&check), DispatchResult::Unknown { .. }));
    assert!(h.evidence.is_empty());
    let decision = intent(
        "decision",
        "decision.request",
        json!({"subject":subject(),"question":"approve?","choices":["approve","reject"],"responders":["alice"],"expires_at_ms":1000}),
    );
    assert!(matches!(
        h.dispatch(&decision),
        DispatchResult::Accepted { .. }
    ));
    h.complete_decision("decision", "alice", "approved")
        .unwrap();
    assert_eq!(outcome(h.reconcile(&decision)).0, "approved");
    h.grants.clear();
    assert!(matches!(
        h.reconcile(&decision),
        DispatchResult::Rejected { .. }
    ));
}

#[test]
fn async_decision_admission_replay_completion_and_readback_are_distinct() {
    let mut h = fixture();
    let decision = intent(
        "decision",
        "decision.request",
        json!({"subject":subject(),"question":"approve?","choices":["approve","reject"],"responders":["alice"],"expires_at_ms":1000}),
    );
    h.lose_next_response = true;
    assert!(matches!(
        h.dispatch(&decision),
        DispatchResult::Unknown { .. }
    ));
    h = h.restart();
    assert!(
        matches!(h.reconcile(&decision), DispatchResult::Accepted { handle } if handle == "decision")
    );
    let lookup = intent("lookup", "operation.get", json!({"handle":"decision"}));
    assert_eq!(outcome(h.dispatch(&lookup)).0, "running");
    assert!(h.complete_decision("decision", "bob", "approved").is_err());
    let event = h
        .complete_decision("decision", "alice", "approved")
        .unwrap();
    assert_eq!(event.contract.name, "aq.decision.completed");
    assert_eq!(event.correlation["operation"], "decision");
    catalog::catalog().events[&event.contract.name]
        .payload
        .validate(&event.payload)
        .unwrap();
    h = h.restart();
    h.now_ms = 500;
    assert_eq!(
        serde_json::to_value(
            h.complete_decision("decision", "alice", "approved")
                .unwrap()
        )
        .unwrap(),
        serde_json::to_value(&event).unwrap()
    );
    assert!(
        h.complete_decision("decision", "alice", "rejected")
            .is_err()
    );
    let mut lookup = lookup;
    lookup.id = "fresh-lookup".into();
    let (status, output) = outcome(h.dispatch(&lookup));
    assert_eq!(status, "completed");
    assert_eq!(output, event.payload);
    assert_eq!(outcome(h.reconcile(&decision)).0, "approved");
    let mut other = lookup.clone();
    other.id = "other-actor".into();
    other.authority.actor = "bob".into();
    assert!(matches!(
        h.dispatch(&other),
        DispatchResult::Rejected { .. }
    ));
    h.grants.clear();
    assert!(matches!(
        h.reconcile(&decision),
        DispatchResult::Rejected { .. }
    ));
}

#[test]
fn async_decision_rejects_stale_responses_and_expires_or_cancels_truthfully() {
    let mut h = fixture();
    let decision = intent(
        "expires",
        "decision.request",
        json!({"subject":subject(),"question":"approve?","choices":["approve","reject"],"responders":["alice"],"expires_at_ms":1000}),
    );
    assert!(matches!(
        h.dispatch(&decision),
        DispatchResult::Accepted { .. }
    ));
    h.subjects.insert(subject().id, 2);
    assert!(h.complete_decision("expires", "alice", "approved").is_err());
    h.subjects.insert(subject().id, 1);
    h.now_ms = 1000;
    assert!(h.complete_decision("expires", "alice", "approved").is_err());
    let lookup = intent(
        "expired-status",
        "operation.get",
        json!({"handle":"expires"}),
    );
    assert_eq!(outcome(h.dispatch(&lookup)).1["outcome"], "expired");
    h.now_ms = 0;
    let mut decision = decision;
    decision.id = "cancel-me".into();
    assert!(matches!(
        h.dispatch(&decision),
        DispatchResult::Accepted { .. }
    ));
    assert!(matches!(h.cancel(&decision), CancellationResult::Confirmed));
    assert_eq!(outcome(h.reconcile(&decision)).0, "cancelled");
    assert!(matches!(h.cancel(&decision), CancellationResult::TooLate));
}

#[test]
fn host_calendar_occurrences_pin_timezone_misfire_and_configuration_across_restart() {
    let mut h = fixture();
    h.calendars.insert(
        "morning-report".into(),
        CalendarConfiguration {
            revision: 7,
            timezone: "America/Los_Angeles".into(),
            misfire: "coalesce".into(),
        },
    );
    let occurrence = h
        .admit_occurrence("report-2026-11-01", "morning-report", 1793523600000, 7)
        .unwrap();
    h = h.restart();
    h.calendars.get_mut("morning-report").unwrap().revision = 8;
    h.calendars.get_mut("morning-report").unwrap().timezone = "UTC".into();
    assert_eq!(
        h.admit_occurrence("report-2026-11-01", "morning-report", 1793523600000, 8)
            .unwrap(),
        occurrence
    );
    assert_eq!(occurrence.configuration_revision, 7);
    assert_eq!(occurrence.timezone, "America/Los_Angeles");
    assert_eq!(occurrence.misfire, "coalesce");
    assert!(
        h.admit_occurrence("new-occurrence", "morning-report", 1793610000000, 7)
            .is_err()
    );
    assert!(
        h.admit_occurrence("report-2026-11-01", "morning-report", 1793610000000, 8)
            .is_err()
    );
}

#[test]
fn fresh_base_reads_capture_authoritative_revision_and_preserve_named_snapshot() {
    let mut h = fixture();
    h.subjects.insert("main".into(), 7);
    h.visibility
        .get_mut("alice")
        .unwrap()
        .insert("project".into());
    let request = |id: &str| {
        intent(
            id,
            "context.read",
            json!({"view":"project","subject":{"id":"main","revision":1},"snapshot":"","page":0}),
        )
    };
    let first = request("base1");
    let (_, first_output) = outcome(h.dispatch(&first));
    assert_eq!(first_output["subject"]["revision"], 7);
    h.subjects.insert("main".into(), 8);
    assert_eq!(outcome(h.dispatch(&first)).1, first_output);
    let (_, second_output) = outcome(h.dispatch(&request("base2")));
    assert_eq!(second_output["subject"]["revision"], 8);
    assert_ne!(first_output["snapshot"], second_output["snapshot"]);
    let named = intent(
        "retained",
        "context.read",
        json!({"view":"project","subject":{"id":"main","revision":7},"snapshot":first_output["snapshot"],"page":0}),
    );
    assert_eq!(outcome(h.dispatch(&named)).1["subject"]["revision"], 7);
    for (id, key, value) in [
        ("bad-view", "view", json!("evidence")),
        ("bad-page", "page", json!(1)),
        ("bad-subject", "subject", json!({"id":"other","revision":1})),
    ] {
        let mut bad = request(id);
        bad.input[key] = value;
        assert!(matches!(h.dispatch(&bad), DispatchResult::Rejected { .. }));
    }
    let mut invisible = request("invisible");
    invisible.authority.actor = "bob".into();
    assert!(matches!(
        h.dispatch(&invisible),
        DispatchResult::Rejected { .. }
    ));
    assert_eq!(h.observations.len(), 2);
}
