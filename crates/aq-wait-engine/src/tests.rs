use super::*;
use std::collections::BTreeMap;
fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}
fn text(s: impl Into<String>) -> wire::Text {
    wire::Text::new(s).unwrap()
}
fn artifact(s: &str) -> wire::ArtifactRef {
    wire::ArtifactRef {
        id: text(s),
        version: text("1"),
        digest: wire::Digest::of(s.as_bytes()),
    }
}
fn fixture(
    count: usize,
    any: bool,
) -> (
    wire::Request,
    AuthorityFence,
    ResolvedPolicy,
    StableIds,
    KnownGraph,
) {
    let value: serde_json::Value = serde_json::from_slice(include_bytes!(
        "../../aq-lifecycle-api/artifacts/fixtures.json"
    ))
    .unwrap();
    let request: wire::Request =
        serde_json::from_value(value["register_any_wait"].clone()).unwrap();
    let wire::Command::RegisterWait(mut registration) = request.body.command else {
        panic!()
    };
    let members: Vec<_> = (0..count)
        .map(|n| wire::Member {
            member_id: id(20 + n as u128),
            condition: wire::Condition::Task {
                task_id: id(100 + n as u128),
                revision: text("fixture-start-0"),
                requirement: wire::TaskRequirement::Successful,
            },
        })
        .collect();
    registration.predicate = if count == 1 {
        wire::Predicate::Single {
            member: members[0].clone(),
        }
    } else if any {
        wire::Predicate::Any { members }
    } else {
        wire::Predicate::All { members }
    };
    registration.predicate_digest = canonical_digest(&registration.predicate).unwrap();
    let body = PolicyBody {
        maximum_age_ms: 100_000,
        check_interval_ms: 1000,
        retry_base_ms: 1000,
        retry_cap_ms: 60_000,
        max_retries: 5,
        max_work_attempts: 128,
    };
    let policy = ResolvedPolicy {
        reference: wire::ArtifactRef {
            id: text("fixture-policy"),
            version: text("1"),
            digest: canonical_digest(&body).unwrap(),
        },
        body,
    };
    registration.policy = policy.reference.clone();
    let fence = AuthorityFence {
        scope: registration.scope.clone(),
        binding: registration.binding.clone(),
        principal: wire::Digest::of(b"authenticated fixture agent"),
        source_access: wire::Digest::of(b"source access revision1"),
        task_cancelled: false,
    };
    let graph = KnownGraph {
        scope: registration.scope.clone(),
        complete_known_component: true,
        revision: artifact("fixture known wait graph1"),
        edges: Vec::new(),
        external_unknown: false,
    };
    let ids = StableIds {
        continuation_id: id(200),
        outcome_id: id(201),
        resume_intent_id: id(202),
    };
    (
        wire::Request::seal(id(30), wire::Command::RegisterWait(registration)).unwrap(),
        fence,
        policy,
        ids,
        graph,
    )
}
fn begin(count: usize, any: bool) -> (TransitionPlan, AuthorityFence) {
    let (request, fence, policy, ids, graph) = fixture(count, any);
    (
        Snapshot::begin(request, &fence, policy, ids, &graph, 1000).unwrap(),
        fence,
    )
}
fn work(plan: &TransitionPlan) -> (WorkTicket, Vec<SourceRequest>) {
    match plan.actions.first().unwrap() {
        Action::Install { ticket, sources } | Action::Reobserve { ticket, sources } => {
            (ticket.clone(), sources.clone())
        }
        _ => panic!("expected source work"),
    }
}
fn respond(
    plan: &TransitionPlan,
    fence: &AuthorityFence,
    owner: &mut FakeSource,
    now: u64,
) -> TransitionPlan {
    let (ticket, requests) = work(plan);
    let replies = requests
        .iter()
        .map(|r| {
            if ticket.kind == WorkKind::Install {
                owner.install(r)
            } else {
                owner.reobserve(r)
            }
        })
        .collect::<Result<Vec<_>>>()
        .unwrap();
    plan.next
        .step(Input::SourceBatch { ticket, replies }, fence, now)
        .unwrap()
}
#[derive(Clone)]
enum Tx {
    Open(Vec<(Uuid, wire::MemberState)>),
    Committed(Vec<(Uuid, wire::MemberState)>),
    RolledBack,
}
/// Fake OWNER protocol, not Graph implementation. Allocated IDs can commit out
/// of order; only a contiguous settled prefix is a committed replay boundary.
struct FakeSource {
    transactions: BTreeMap<u64, Tx>,
    next: u64,
    floor: u64,
    subscriptions: BTreeMap<wire::Digest, wire::ArtifactRef>,
    calls: usize,
    unavailable: bool,
    retained_snapshots: BTreeMap<wire::Digest, Vec<u8>>,
}
impl FakeSource {
    fn new() -> Self {
        Self {
            transactions: BTreeMap::new(),
            next: 0,
            floor: 0,
            subscriptions: BTreeMap::new(),
            calls: 0,
            unavailable: false,
            retained_snapshots: BTreeMap::new(),
        }
    }
    fn allocate(&mut self, operations: Vec<(Uuid, wire::MemberState)>) -> u64 {
        self.next += 1;
        self.transactions.insert(self.next, Tx::Open(operations));
        self.next
    }
    fn commit(&mut self, sequence: u64) {
        let Tx::Open(ops) = self.transactions[&sequence].clone() else {
            panic!()
        };
        // One transaction contributes FIRST-before/FINAL-after only: internal
        // completion followed by reopen is not an externally committed success.
        let coalesced: BTreeMap<_, _> = ops.into_iter().collect();
        self.transactions
            .insert(sequence, Tx::Committed(coalesced.into_iter().collect()));
    }
    fn rollback(&mut self, sequence: u64) {
        self.transactions.insert(sequence, Tx::RolledBack);
    }
    fn boundary(&self) -> u64 {
        let mut through = 0;
        for (seq, tx) in &self.transactions {
            if matches!(tx, Tx::Open(_)) {
                break;
            }
            through = *seq;
        }
        through
    }
    fn latest(&self, member: Uuid) -> wire::MemberState {
        self.transactions
            .values()
            .filter_map(|tx| {
                if let Tx::Committed(ops) = tx {
                    ops.iter()
                        .find(|(m, _)| *m == member)
                        .map(|(_, s)| s.clone())
                } else {
                    None
                }
            })
            .next_back()
            .unwrap_or(wire::MemberState::Pending)
    }
    fn proof(&mut self, r: &SourceRequest, install: bool) -> Result<SourceReply> {
        self.calls += 1;
        if self.unavailable {
            return Ok(SourceReply::RetryableUnavailable {
                member_id: r.member.member_id,
                reason: text("temporary fixture owner outage"),
            });
        }
        let from = r
            .previous
            .as_ref()
            .map(|b| b.committed_replay_cursor.as_str().parse::<u64>().unwrap())
            .unwrap_or(0);
        if !install && from < self.floor {
            return Ok(SourceReply::ReplayGap {
                member_id: r.member.member_id,
                reason: text("fixture history no longer covers prior boundary"),
            });
        }
        let through = self.boundary();
        let mut state = wire::MemberState::Pending;
        let mut matched = 0;
        // Fixture predicate means a committed terminal transition since start0.
        // Previously committed historical terminal evidence survives later reopen.
        for (seq, tx) in self.transactions.range(..=through) {
            if let Tx::Committed(ops) = tx
                && let Some((_, verdict)) = ops.iter().find(|(m, _)| *m == r.member.member_id)
                && matches!(
                    verdict,
                    wire::MemberState::Satisfied | wire::MemberState::SourceFailed
                )
            {
                state = verdict.clone();
                matched = *seq;
                break;
            }
        }
        let evidence = matches!(
            state,
            wire::MemberState::Satisfied | wire::MemberState::SourceFailed
        )
        .then(|| wire::SourceEvidence {
            owner: r.member.condition.owner(),
            condition_digest: canonical_digest(&r.member.condition).unwrap(),
            revision: text(format!("committed-transaction-{matched}")),
            result: artifact(&format!("fixture-terminal-evidence-{matched}")),
        });
        let observation = wire::MemberOutcome {
            member_id: r.member.member_id,
            state,
            evidence,
        };
        let sub = r.subscription.digest()?;
        let receipt = self
            .subscriptions
            .entry(sub.clone())
            .or_insert_with(|| artifact(&format!("fixture-source-sub-{}", sub.as_str())))
            .clone();
        let mut snapshot = serde_json::json!({"owner_profile":"committed-fixture-source-1","scope":r.scope,"subscription":r.subscription,"condition":r.member.condition,"committed_prefix":through,"observation":observation});
        snapshot.sort_all_objects();
        let snapshot = serde_json::to_vec(&snapshot).unwrap();
        let owner_digest = wire::Digest::of(&snapshot);
        self.retained_snapshots
            .insert(owner_digest.clone(), snapshot);
        let owner_snapshot = wire::ArtifactRef {
            id: text(format!("fixture-owner-snapshot-{}", owner_digest.as_str())),
            version: text("1"),
            digest: owner_digest.clone(),
        };
        Ok(SourceReply::Committed(Box::new(CommittedObservation {
            protocol: SourceProtocol::CommittedReplay,
            subscription: r.subscription.clone(),
            scope: r.scope.clone(),
            binding: r.binding.clone(),
            principal: r.principal.clone(),
            source_access: r.source_access.clone(),
            incarnation: text("fixture-owner-epoch1"),
            boundary: wire::SourceBoundary {
                member_id: r.member.member_id,
                owner: r.member.condition.owner(),
                condition_digest: canonical_digest(&r.member.condition)?,
                committed_revision: text(format!("committed-prefix-{through}")),
                committed_replay_cursor: text(through.to_string()),
                subscription_receipt: receipt,
                snapshot_digest: owner_digest,
            },
            owner_snapshot,
            projection_digest: canonical_digest(&observation)?,
            replay_from: r
                .previous
                .as_ref()
                .map(|b| b.committed_replay_cursor.clone()),
            observation,
        })))
    }
}
impl SourceOwnerPort for FakeSource {
    fn install(&mut self, r: &SourceRequest) -> Result<SourceReply> {
        self.proof(r, true)
    }
    fn reobserve(&mut self, r: &SourceRequest) -> Result<SourceReply> {
        self.proof(r, false)
    }
}

#[test]
fn early_resolution_never_skips_unconditional_reobserve_and_result_is_immutable() {
    let (plan, fence) = begin(1, false);
    let mut source = FakeSource::new();
    let seq = source.allocate(vec![(id(20), wire::MemberState::Satisfied)]);
    source.commit(seq);
    let installed = respond(&plan, &fence, &mut source, 1001);
    assert!(installed.next.outcome().is_none());
    assert!(matches!(installed.actions[0], Action::Reobserve { .. }));
    let finished = respond(&installed, &fence, &mut source, 1002);
    let original = finished.next.outcome().unwrap().clone();
    assert_eq!(original.kind, wire::WaitOutcomeKind::Satisfied);
    assert_eq!(original.outcome.outcome_id, id(201));
    assert!(
        matches!(&finished.actions[0],Action::PersistOutcome{resume:Some(r),..} if r.intent_id==id(202))
    );
    let late = finished
        .next
        .step(Input::Hint { member_id: id(20) }, &fence, 2000)
        .unwrap();
    assert_eq!(late.next.outcome(), Some(&original));
    assert!(late.actions.is_empty());
}
#[test]
fn owner_snapshot_and_aq_projection_digests_are_independent_verified_links() {
    let (plan, fence) = begin(1, false);
    let mut source = FakeSource::new();
    let (ticket, requests) = work(&plan);
    let SourceReply::Committed(proof) = source.install(&requests[0]).unwrap() else {
        panic!()
    };
    assert_ne!(proof.boundary.snapshot_digest, proof.projection_digest);
    assert_eq!(
        wire::Digest::of(&source.retained_snapshots[&proof.owner_snapshot.digest]),
        proof.boundary.snapshot_digest
    );
    assert_eq!(
        canonical_digest(&proof.observation).unwrap(),
        proof.projection_digest
    );
    let installed = plan
        .next
        .step(
            Input::SourceBatch {
                ticket,
                replies: vec![SourceReply::Committed(proof)],
            },
            &fence,
            1001,
        )
        .unwrap();
    assert!(matches!(installed.actions[0], Action::Reobserve { .. }));
    let restored = Snapshot::decode(&installed.next.encode().unwrap()).unwrap();
    assert_eq!(
        restored.members[0]
            .original_snapshot
            .as_ref()
            .unwrap()
            .digest,
        restored.members[0]
            .original
            .as_ref()
            .unwrap()
            .snapshot_digest
    );
}
#[test]
fn resolution_during_install_and_with_no_hint_is_found_after_restart() {
    let (plan, fence) = begin(1, false);
    let mut source = FakeSource::new();
    let installed = respond(&plan, &fence, &mut source, 1001);
    let seq = source.allocate(vec![(id(20), wire::MemberState::Satisfied)]);
    source.commit(seq);
    let restored = Snapshot::decode(&installed.next.encode().unwrap()).unwrap();
    let held = TransitionPlan {
        next: restored,
        ..installed
    };
    let finished = respond(&held, &fence, &mut source, 1002);
    assert_eq!(
        finished.next.outcome().unwrap().kind,
        wire::WaitOutcomeKind::Satisfied
    );
}
#[test]
fn allocated_later_commit_does_not_advance_safe_watermark_past_held_early_mutation() {
    let (plan, fence) = begin(1, false);
    let mut source = FakeSource::new();
    let early = source.allocate(vec![(id(20), wire::MemberState::Satisfied)]);
    let later = source.allocate(vec![(id(999), wire::MemberState::Pending)]);
    source.commit(later);
    assert_eq!(source.next, 2);
    assert_eq!(source.boundary(), 0);
    let installed = respond(&plan, &fence, &mut source, 1001);
    assert_eq!(
        installed.next.members[0]
            .original
            .as_ref()
            .unwrap()
            .committed_replay_cursor
            .as_str(),
        "0"
    );
    let pending = respond(&installed, &fence, &mut source, 1002);
    assert!(pending.next.outcome().is_none());
    source.commit(early);
    assert_eq!(source.boundary(), 2);
    let restored = Snapshot::decode(&pending.next.encode().unwrap()).unwrap();
    let driven = restored
        .step(Input::Drive, &fence, restored.next_due_ms())
        .unwrap();
    let finished = respond(&driven, &fence, &mut source, driven.next.logical_now);
    assert_eq!(
        finished.next.outcome().unwrap().kind,
        wire::WaitOutcomeKind::Satisfied
    );
}
#[test]
fn rollback_gap_is_settled_but_an_unqualified_max_allocated_claim_is_refused() {
    let (plan, fence) = begin(1, false);
    let mut source = FakeSource::new();
    let early = source.allocate(Vec::new());
    let later = source.allocate(vec![(id(20), wire::MemberState::Satisfied)]);
    source.commit(later);
    source.rollback(early);
    let (ticket, requests) = work(&plan);
    let mut reply = source.install(&requests[0]).unwrap();
    if let SourceReply::Committed(p) = &mut reply {
        p.protocol = SourceProtocol::Unqualified;
    }
    assert_eq!(
        plan.next
            .step(
                Input::SourceBatch {
                    ticket,
                    replies: vec![reply]
                },
                &fence,
                1001
            )
            .unwrap_err(),
        Error::Proof
    );
    let installed = respond(&plan, &fence, &mut source, 1001);
    let finished = respond(&installed, &fence, &mut source, 1002);
    assert_eq!(source.boundary(), 2);
    assert!(finished.next.outcome().is_some());
}
#[test]
fn internal_atomic_complete_reopen_is_not_success_but_two_commits_have_historical_evidence() {
    let (plan, fence) = begin(1, false);
    let mut source = FakeSource::new();
    let seq = source.allocate(vec![
        (id(20), wire::MemberState::Satisfied),
        (id(20), wire::MemberState::Pending),
    ]);
    source.commit(seq);
    let installed = respond(&plan, &fence, &mut source, 1001);
    let pending = respond(&installed, &fence, &mut source, 1002);
    assert!(pending.next.outcome().is_none());
    let done = source.allocate(vec![(id(20), wire::MemberState::Satisfied)]);
    source.commit(done);
    let reopen = source.allocate(vec![(id(20), wire::MemberState::Pending)]);
    source.commit(reopen);
    assert_eq!(source.latest(id(20)), wire::MemberState::Pending);
    let driven = pending
        .next
        .step(Input::Drive, &fence, pending.next.next_due_ms())
        .unwrap();
    let finished = respond(&driven, &fence, &mut source, driven.next.logical_now);
    assert_eq!(
        finished.next.outcome().unwrap().members[0]
            .evidence
            .as_ref()
            .unwrap()
            .revision
            .as_str(),
        "committed-transaction-2"
    );
}
#[test]
fn retention_gap_cannot_use_current_only_success_and_retryable_outage_keeps_last_freshness() {
    let (plan, fence) = begin(1, false);
    let mut source = FakeSource::new();
    let installed = respond(&plan, &fence, &mut source, 1001);
    let pending = respond(&installed, &fence, &mut source, 1002);
    let seq = source.allocate(vec![(id(20), wire::MemberState::Satisfied)]);
    source.commit(seq);
    source.floor = 1;
    assert_eq!(source.latest(id(20)), wire::MemberState::Satisfied);
    let driven = pending
        .next
        .step(Input::Drive, &fence, pending.next.next_due_ms())
        .unwrap();
    let gap = respond(&driven, &fence, &mut source, driven.next.logical_now);
    assert!(gap.next.outcome().is_none());
    assert_eq!(
        gap.next.members[0]
            .current
            .as_ref()
            .unwrap()
            .committed_replay_cursor
            .as_str(),
        "0"
    );
    assert_eq!(gap.next.members[0].last_verified_at, Some(1002));
    assert!(gap.next.members[0].unavailable_reason.is_some());
    let no_hot_retry = gap
        .next
        .step(
            Input::Hint { member_id: id(20) },
            &fence,
            gap.next.logical_now + 1,
        )
        .unwrap();
    assert!(no_hot_retry.actions.is_empty());
    source.floor = 0;
    let driven = gap
        .next
        .step(Input::Drive, &fence, gap.next.next_due_ms())
        .unwrap();
    let repaired = respond(&driven, &fence, &mut source, driven.next.logical_now);
    assert_eq!(
        repaired.next.outcome().unwrap().kind,
        wire::WaitOutcomeKind::Satisfied
    );
}
#[test]
fn lost_install_reply_restart_reclaim_and_exact_request_replay_preserve_stable_identity_age() {
    let (plan, fence) = begin(1, false);
    let mut source = FakeSource::new();
    let (old_ticket, requests) = work(&plan);
    let original_reply = source.install(&requests[0]).unwrap();
    let stored = Snapshot::decode(&plan.next.encode().unwrap()).unwrap();
    let reclaimed = stored
        .step(
            Input::Reclaim {
                ticket: old_ticket.clone(),
            },
            &fence,
            1100,
        )
        .unwrap();
    assert!(reclaimed.actions.is_empty());
    let driven = reclaimed
        .next
        .step(Input::Drive, &fence, reclaimed.next.next_due_ms())
        .unwrap();
    let (new_ticket, new_requests) = work(&driven);
    assert_ne!(old_ticket, new_ticket);
    assert_eq!(requests[0].subscription, new_requests[0].subscription);
    assert_eq!(
        driven
            .next
            .step(
                Input::SourceBatch {
                    ticket: old_ticket,
                    replies: vec![original_reply]
                },
                &fence,
                driven.next.logical_now
            )
            .unwrap_err(),
        Error::Stale
    );
    let installed = respond(&driven, &fence, &mut source, driven.next.logical_now);
    assert_eq!(source.subscriptions.len(), 1);
    let replay = installed
        .next
        .replay_registration(&plan.next.request, &fence)
        .unwrap();
    assert!(replay.replay);
    assert_eq!(replay.next, installed.next);
    assert_eq!(replay.next.original_created_at_ms(), 1000);
    assert_eq!(replay.next.deadline_ms(), 101_000);
    let mut changed = plan.next.registration().clone();
    changed.preference = wire::WaitPreference::Hibernate;
    let changed = wire::Request::seal(
        plan.next.request.body.operation_id,
        wire::Command::RegisterWait(changed),
    )
    .unwrap();
    assert_eq!(
        installed
            .next
            .replay_registration(&changed, &fence)
            .unwrap_err(),
        Error::Conflict
    );
}
#[test]
fn dirty_hint_during_held_round_is_not_lost_and_does_not_commit_stale_success() {
    let (plan, fence) = begin(1, false);
    let mut source = FakeSource::new();
    let installed = respond(&plan, &fence, &mut source, 1001);
    let seq = source.allocate(vec![(id(20), wire::MemberState::Satisfied)]);
    source.commit(seq);
    let (ticket, requests) = work(&installed);
    let reply = source.reobserve(&requests[0]).unwrap();
    let mut held = installed.next.clone();
    let before = held.encode().unwrap().len();
    for _ in 0..64 {
        let plan = held
            .step(Input::Hint { member_id: id(20) }, &fence, 1002)
            .unwrap();
        assert!(plan.actions.is_empty());
        held = plan.next;
    }
    assert!(held.encode().unwrap().len() < before + 64);
    let again = held
        .step(
            Input::SourceBatch {
                ticket,
                replies: vec![reply],
            },
            &fence,
            1003,
        )
        .unwrap();
    assert!(again.next.outcome().is_none());
    assert!(matches!(again.actions[0], Action::Reobserve { .. }));
    let final_round = respond(&again, &fence, &mut source, 1004);
    assert!(final_round.next.outcome().is_some());
}
#[test]
fn any_all_use_declared_order_and_unavailable_is_never_evidenced_failure() {
    for any in [true, false] {
        let (plan, fence) = begin(2, any);
        let mut source = FakeSource::new();
        let first = source.allocate(vec![(id(21), wire::MemberState::Satisfied)]);
        source.commit(first);
        let installed = respond(&plan, &fence, &mut source, 1001);
        let result = respond(&installed, &fence, &mut source, 1002);
        if any {
            assert_eq!(
                result.next.outcome().unwrap().selected_member_id,
                Some(id(21))
            );
        } else {
            assert!(result.next.outcome().is_none());
        }
    }
    let (plan, fence) = begin(2, true);
    let mut source = FakeSource::new();
    let seq = source.allocate(vec![
        (id(20), wire::MemberState::Satisfied),
        (id(21), wire::MemberState::Satisfied),
    ]);
    source.commit(seq);
    let installed = respond(&plan, &fence, &mut source, 1001);
    let result = respond(&installed, &fence, &mut source, 1002);
    assert_eq!(
        result.next.outcome().unwrap().selected_member_id,
        Some(id(20))
    );
    let (plan, fence) = begin(2, true);
    let mut source = FakeSource::new();
    let installed = respond(&plan, &fence, &mut source, 1001);
    let (ticket, requests) = work(&installed);
    let seq = source.allocate(vec![(id(20), wire::MemberState::SourceFailed)]);
    source.commit(seq);
    let failed = source.reobserve(&requests[0]).unwrap();
    let pending = installed
        .next
        .step(
            Input::SourceBatch {
                ticket,
                replies: vec![
                    failed,
                    SourceReply::RetryableUnavailable {
                        member_id: id(21),
                        reason: text("offline"),
                    },
                ],
            },
            &fence,
            1002,
        )
        .unwrap();
    assert!(pending.next.outcome().is_none());
    assert_eq!(*pending.next.phase(), Phase::Pending);
}
#[test]
fn source_scope_principal_incarnation_and_snapshot_proof_are_mandatory() {
    let (plan, fence) = begin(1, false);
    let mut source = FakeSource::new();
    let installed = respond(&plan, &fence, &mut source, 1001);
    let (ticket, requests) = work(&installed);
    for change in 0..11 {
        let SourceReply::Committed(mut proof) = source.reobserve(&requests[0]).unwrap() else {
            panic!()
        };
        match change {
            0 => proof.scope.workspace_id = id(900),
            1 => proof.binding.attempt_generation += 1,
            2 => proof.principal = wire::Digest::of(b"foreign"),
            3 => proof.source_access = wire::Digest::of(b"new source"),
            4 => proof.incarnation = text("new epoch"),
            5 => proof.boundary.snapshot_digest = wire::Digest::of(b"not observed"),
            6 => proof.replay_from = Some(text("999")),
            7 => proof.projection_digest = wire::Digest::of(b"different projection"),
            8 => proof.boundary.condition_digest = wire::Digest::of(b"different condition"),
            9 => proof.subscription.member_id = id(800),
            _ => proof.owner_snapshot.digest = wire::Digest::of(b"different owner body"),
        }
        assert!(
            installed
                .next
                .step(
                    Input::SourceBatch {
                        ticket: ticket.clone(),
                        replies: vec![SourceReply::Committed(proof)]
                    },
                    &fence,
                    1002
                )
                .is_err()
        );
    }
    let mut changed = fence.clone();
    changed.source_access = wire::Digest::of(b"changed during IO");
    let held = installed.next.step(Input::Drive, &changed, 1002).unwrap();
    assert_eq!(*held.next.phase(), Phase::RecoveryHeld);
    assert!(held.next.outcome().is_none());
    let mut foreign = fence.clone();
    foreign.principal = wire::Digest::of(b"rebound principal");
    assert_eq!(
        installed
            .next
            .step(Input::Drive, &foreign, 1002)
            .unwrap_err(),
        Error::Stale
    );
}
#[test]
fn cancellation_deadline_and_late_reply_races_preserve_one_immutable_outcome() {
    for cancelled in [true, false] {
        let (plan, fence) = begin(1, false);
        let mut source = FakeSource::new();
        let installed = respond(&plan, &fence, &mut source, 1001);
        let seq = source.allocate(vec![(id(20), wire::MemberState::Satisfied)]);
        source.commit(seq);
        let (ticket, requests) = work(&installed);
        let reply = source.reobserve(&requests[0]).unwrap();
        let mut current = fence.clone();
        current.task_cancelled = cancelled;
        let now = if cancelled {
            1002
        } else {
            installed.next.deadline_ms()
        };
        let result = installed
            .next
            .step(
                Input::SourceBatch {
                    ticket: ticket.clone(),
                    replies: vec![reply.clone()],
                },
                &current,
                now,
            )
            .unwrap();
        assert_eq!(
            result.next.outcome().unwrap().kind,
            if cancelled {
                wire::WaitOutcomeKind::Cancelled
            } else {
                wire::WaitOutcomeKind::DeadlineReached
            }
        );
        assert!(
            matches!(&result.actions[0],Action::PersistOutcome{resume,..} if resume.is_some()!=cancelled)
        );
        let late = result
            .next
            .step(
                Input::SourceBatch {
                    ticket,
                    replies: vec![reply],
                },
                &current,
                now + 1,
            )
            .unwrap();
        assert_eq!(late.next.outcome(), result.next.outcome());
        assert!(late.actions.is_empty());
    }
    let (plan, mut fence) = begin(1, false);
    let mut source = FakeSource::new();
    let seq = source.allocate(vec![(id(20), wire::MemberState::Satisfied)]);
    source.commit(seq);
    let installed = respond(&plan, &fence, &mut source, 1001);
    let done = respond(&installed, &fence, &mut source, 1002);
    fence.task_cancelled = true;
    let cancelled = done.next.step(Input::Drive, &fence, 1003).unwrap();
    assert_eq!(cancelled.next.outcome(), done.next.outcome());
    assert!(matches!(cancelled.actions[0], Action::RetireResume { .. }));
}
#[test]
fn rollback_time_and_retry_exhaustion_never_reset_age_or_create_hot_polling() {
    let (plan, fence) = begin(1, false);
    let mut source = FakeSource::new();
    source.unavailable = true;
    let mut pending = respond(&plan, &fence, &mut source, 1001);
    for _ in 0..4 {
        let calls = source.calls;
        let early = pending.next.step(Input::Drive, &fence, 1).unwrap();
        assert!(early.actions.is_empty());
        assert_eq!(early.next.original_created_at_ms(), 1000);
        assert_eq!(source.calls, calls);
        let driven = pending
            .next
            .step(Input::Drive, &fence, pending.next.next_due_ms())
            .unwrap();
        pending = respond(&driven, &fence, &mut source, driven.next.logical_now);
    }
    assert_eq!(*pending.next.phase(), Phase::RecoveryHeld);
    assert!(pending.next.outcome().is_none());
    let mut overdue = pending.next.clone();
    overdue.logical_now = overdue.deadline_ms();
    let final_plan = overdue.step(Input::Drive, &fence, 2).unwrap();
    assert_eq!(
        final_plan.next.outcome().unwrap().kind,
        wire::WaitOutcomeKind::DeadlineReached
    );
    assert_eq!(final_plan.next.original_created_at_ms(), 1000);
}
#[test]
fn bounded_known_cycles_and_serialized_opposing_registration_refuse_without_global_claim() {
    let (request, fence, policy, ids, mut graph) = fixture(1, false);
    let owner = DependencyKey::Task {
        project: fence.scope.project_id,
        task: fence.binding.task_id,
    };
    let source = DependencyKey::Task {
        project: fence.scope.project_id,
        task: id(100),
    };
    graph.edges.push((source.clone(), owner.clone()));
    assert_eq!(
        Snapshot::begin(
            request.clone(),
            &fence,
            policy.clone(),
            ids.clone(),
            &graph,
            1000
        )
        .unwrap_err(),
        Error::Cycle
    );
    graph.edges = vec![(source.clone(), source.clone()); 4097];
    assert_eq!(
        graph
            .check(&owner, std::slice::from_ref(&source))
            .unwrap_err(),
        Error::Capacity
    );
    graph.edges = (0..66)
        .map(|n| {
            (
                DependencyKey::Task {
                    project: fence.scope.project_id,
                    task: id(100 + n),
                },
                DependencyKey::Task {
                    project: fence.scope.project_id,
                    task: id(101 + n),
                },
            )
        })
        .collect();
    assert_eq!(
        graph
            .check(&owner, std::slice::from_ref(&source))
            .unwrap_err(),
        Error::Capacity
    );
    graph.edges = vec![(owner.clone(), source.clone())];
    assert_eq!(graph.check(&source, &[owner]).unwrap_err(), Error::Cycle);
    graph.edges.clear();
    graph.complete_known_component = false;
    assert_eq!(graph.check(&source, &[]).unwrap_err(), Error::Capacity);
    graph.complete_known_component = true;
    graph.external_unknown = true;
    assert!(matches!(
        graph.check(&source, &[]).unwrap(),
        wire::CycleAssessment::ExternalUnknown { .. }
    ));
}
#[test]
fn bounded_restore_and_backpressure_refuse_malformed_or_duplicate_work() {
    let (plan, fence) = begin(2, true);
    assert!(Snapshot::decode(&vec![b' '; MAX_SNAPSHOT_BYTES + 1]).is_err());
    let mut changed = serde_json::to_value(&plan.next).unwrap();
    changed["deadline"] = serde_json::json!(999999);
    assert!(serde_json::from_value::<Snapshot>(changed).is_err());
    let driven = plan.next.step(Input::Drive, &fence, 1001).unwrap();
    assert!(driven.actions.is_empty());
    let (ticket, requests) = work(&plan);
    let mut source = FakeSource::new();
    let reply = source.install(&requests[0]).unwrap();
    assert!(
        plan.next
            .step(
                Input::SourceBatch {
                    ticket,
                    replies: vec![reply.clone(), reply]
                },
                &fence,
                1001
            )
            .is_err()
    );
    let (mut request, fence, policy, ids, graph) = fixture(1, false);
    if let wire::Command::RegisterWait(r) = &mut request.body.command {
        r.predicate = wire::Predicate::Single {
            member: wire::Member {
                member_id: id(20),
                condition: wire::Condition::Task {
                    task_id: r.binding.task_id,
                    revision: text("1"),
                    requirement: wire::TaskRequirement::Successful,
                },
            },
        };
        r.predicate_digest = canonical_digest(&r.predicate).unwrap();
    }
    assert!(
        request.validate().is_err(),
        "frozen self-wait validation refuses before resealing"
    );
    assert!(Snapshot::begin(request, &fence, policy, ids, &graph, 1000).is_err());
}
#[test]
fn restored_work_cannot_skip_post_install_reobserve_or_contradict_terminal_member_history() {
    let (plan, fence) = begin(1, false);
    let mut encoded = serde_json::to_value(&plan.next).unwrap();
    encoded["inflight"]["kind"] = serde_json::json!("reobserve");
    assert!(Snapshot::decode(&serde_json::to_vec(&encoded).unwrap()).is_err());
    encoded["phase"] = serde_json::json!("pending");
    assert!(Snapshot::decode(&serde_json::to_vec(&encoded).unwrap()).is_err());
    let mut source = FakeSource::new();
    let seq = source.allocate(vec![(id(20), wire::MemberState::Satisfied)]);
    source.commit(seq);
    let installed = respond(&plan, &fence, &mut source, 1001);
    let mut encoded = serde_json::to_value(&installed.next).unwrap();
    encoded["inflight"]["kind"] = serde_json::json!("install");
    assert!(Snapshot::decode(&serde_json::to_vec(&encoded).unwrap()).is_err());
    let finished = respond(&installed, &fence, &mut source, 1002);
    let mut encoded = serde_json::to_value(&finished.next).unwrap();
    encoded["outcome"]["members"][0]["state"] = serde_json::json!("pending");
    encoded["outcome"]["members"][0]["evidence"] = serde_json::Value::Null;
    encoded["outcome"]["kind"] = serde_json::json!("deadline_reached");
    encoded["logical_now"] = serde_json::json!(finished.next.deadline_ms());
    encoded["outcome"]["recorded_at_ms"] = serde_json::json!(finished.next.deadline_ms());
    let mut payload = encoded["outcome"].clone();
    payload["outcome"].as_object_mut().unwrap().remove("digest");
    payload.sort_all_objects();
    encoded["outcome"]["outcome"]["digest"] =
        serde_json::to_value(wire::Digest::of(&serde_json::to_vec(&payload).unwrap())).unwrap();
    let wire_outcome: wire::WaitOutcome =
        serde_json::from_value(encoded["outcome"].clone()).unwrap();
    wire_outcome
        .validate_for(&finished.next.registration().predicate)
        .unwrap();
    assert!(
        Snapshot::decode(&serde_json::to_vec(&encoded).unwrap()).is_err(),
        "a recomputed digest cannot replace retained member source facts"
    );
}
fn rehash_outcome(encoded: &mut serde_json::Value) {
    let mut payload = encoded["outcome"].clone();
    payload["outcome"].as_object_mut().unwrap().remove("digest");
    payload.sort_all_objects();
    encoded["outcome"]["outcome"]["digest"] =
        serde_json::to_value(wire::Digest::of(&serde_json::to_vec(&payload).unwrap())).unwrap();
}
#[test]
fn restored_unsupported_terminal_kinds_early_deadline_and_cancelled_resume_are_refused() {
    let (plan, fence) = begin(1, false);
    let mut source = FakeSource::new();
    source.unavailable = true;
    let unavailable = respond(&plan, &fence, &mut source, 1001);
    let deadline = unavailable
        .next
        .step(Input::Drive, &fence, unavailable.next.deadline_ms())
        .unwrap();
    for kind in ["source_unavailable", "manual_override"] {
        let mut encoded = serde_json::to_value(&deadline.next).unwrap();
        encoded["outcome"]["kind"] = serde_json::json!(kind);
        if kind == "manual_override" {
            encoded["outcome"]["unresolved_predicate"] =
                serde_json::to_value(deadline.next.registration().predicate_digest.clone())
                    .unwrap();
        }
        rehash_outcome(&mut encoded);
        let wire_outcome: wire::WaitOutcome =
            serde_json::from_value(encoded["outcome"].clone()).unwrap();
        wire_outcome
            .validate_for(&deadline.next.registration().predicate)
            .unwrap();
        assert!(
            Snapshot::decode(&serde_json::to_vec(&encoded).unwrap()).is_err(),
            "wire-valid kind requires an implemented trusted reducer transition"
        );
    }
    let mut encoded = serde_json::to_value(&deadline.next).unwrap();
    encoded["outcome"]["recorded_at_ms"] = serde_json::json!(1002);
    rehash_outcome(&mut encoded);
    assert!(Snapshot::decode(&serde_json::to_vec(&encoded).unwrap()).is_err());
    let mut current = fence.clone();
    current.task_cancelled = true;
    let cancelled = plan.next.step(Input::Drive, &current, 1001).unwrap();
    let mut encoded = serde_json::to_value(&cancelled.next).unwrap();
    encoded["resume_allowed"] = serde_json::json!(true);
    assert!(Snapshot::decode(&serde_json::to_vec(&encoded).unwrap()).is_err());
}
