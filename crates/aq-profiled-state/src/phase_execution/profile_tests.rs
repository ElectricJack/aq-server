use super::super::canonical;
use super::*;
use crate::{Effects, Retention, Status, Stop, hot::LaunchOwner};
use serde::Deserialize;

#[derive(Deserialize)]
struct Fixture {
    proposal: Proposal,
    accept: AcceptRequest,
    accepted: AcceptedReceipt,
    prepare: PrepareRequest,
    execution: ExecutionReceipt,
}
fn fixture() -> Fixture {
    serde_json::from_str(super::FIXTURE).unwrap()
}
fn hot(f: &Fixture) -> HotState {
    HotState {
        schema_version: 1,
        status: Status::Granted,
        grant: Some(f.execution.grant.clone()),
        execution_deadline_ms: Some(f.execution.execution_deadline_ms),
        owner: Some(LaunchOwner {
            guardian_id: f.prepare.original.guardian_id,
            launch_id: f.prepare.original.launch_id,
            reservation_id: f.prepare.original.reservation.reservation_id,
        }),
        preparation_operation_id: Some(f.accept.original.operation_id),
        cancellation_id: None,
        revoked: false,
        stop: Stop::Unknown,
        retention: Retention::Unknown,
        effects: Effects::Unknown,
    }
}
#[test]
fn sealing_preserves_pinned_fact_bodies_and_digests() {
    let f = fixture();
    for expected in &f.execution.confirmation.facts {
        let actual = seal_fact(
            &f.proposal,
            &f.accepted,
            &f.prepare,
            f.execution.issued_at_ms,
            f.execution.confirmation.confirmation_id,
            FactObservation {
                revision: expected.body.revision,
                observed_at_ms: expected.body.observed_at_ms,
                ceiling: expected.body.expires_at_ms,
                value: expected.body.value.clone(),
            },
        )
        .unwrap();
        assert_eq!(
            canonical(&actual.body).unwrap(),
            canonical(&expected.body).unwrap()
        );
        assert_eq!(actual.record.digest, expected.record.digest);
        assert_eq!(
            actual.record.version.as_str(),
            expected.body.revision.to_string()
        );
        assert_eq!(
            actual.record.id.as_str(),
            format!(
                "aq.phase-fact/{}/{:?}",
                f.execution.confirmation.confirmation_id,
                expected.body.value.kind()
            )
        );
    }
}
#[test]
fn replay_is_exact_while_continuation_only_advances_expiry_within_deadline() {
    let f = fixture();
    let mut current = hot(&f);
    assert!(
        require_hot_state(
            GrantMode::Replay(&f.execution),
            &current,
            &f.prepare,
            &f.accept
        )
        .is_ok()
    );
    assert!(
        require_hot_state(
            GrantMode::Continuing(&f.execution),
            &current,
            &f.prepare,
            &f.accept
        )
        .is_ok()
    );
    for expiry in [
        f.execution.grant.expires_at_ms + 1,
        f.execution.execution_deadline_ms,
    ] {
        current.grant.as_mut().unwrap().expires_at_ms = expiry;
        assert!(
            require_hot_state(
                GrantMode::Replay(&f.execution),
                &current,
                &f.prepare,
                &f.accept
            )
            .is_err()
        );
        assert!(
            require_hot_state(
                GrantMode::Continuing(&f.execution),
                &current,
                &f.prepare,
                &f.accept
            )
            .is_ok()
        );
    }
    for expiry in [
        f.execution.grant.expires_at_ms - 1,
        f.execution.execution_deadline_ms + 1,
    ] {
        current.grant.as_mut().unwrap().expires_at_ms = expiry;
        assert!(
            require_hot_state(
                GrantMode::Continuing(&f.execution),
                &current,
                &f.prepare,
                &f.accept
            )
            .is_err()
        );
    }
}
#[test]
fn retained_grant_requires_original_operation_owner_binding_and_deadline() {
    let f = fixture();
    for change in 0..7 {
        let mut current = hot(&f);
        match change {
            0 => current.preparation_operation_id = Some(Uuid::new_v4()),
            1 => current.owner.as_mut().unwrap().guardian_id = Uuid::new_v4(),
            2 => current.owner.as_mut().unwrap().launch_id = Uuid::new_v4(),
            3 => current.owner.as_mut().unwrap().reservation_id = Uuid::new_v4(),
            4 => current.grant.as_mut().unwrap().grant_id = Uuid::new_v4(),
            5 => current.grant.as_mut().unwrap().binding.attempt_id = Uuid::new_v4(),
            _ => current.execution_deadline_ms = Some(f.execution.execution_deadline_ms + 1),
        }
        assert!(
            require_hot_state(
                GrantMode::Replay(&f.execution),
                &current,
                &f.prepare,
                &f.accept
            )
            .is_err()
        );
        assert!(
            require_hot_state(
                GrantMode::Continuing(&f.execution),
                &current,
                &f.prepare,
                &f.accept
            )
            .is_err()
        );
    }
}
#[test]
fn fresh_preparation_requires_no_grant_or_execution_deadline() {
    let f = fixture();
    let mut current = hot(&f);
    assert!(require_hot_state(GrantMode::Fresh, &current, &f.prepare, &f.accept).is_err());
    current.grant = None;
    assert!(require_hot_state(GrantMode::Fresh, &current, &f.prepare, &f.accept).is_err());
    current.execution_deadline_ms = None;
    assert!(require_hot_state(GrantMode::Fresh, &current, &f.prepare, &f.accept).is_ok());
}
