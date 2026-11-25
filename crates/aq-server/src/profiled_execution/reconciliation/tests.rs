use super::*;
use crate::execution::profiled::{Effects, Retention};
use aq_admission::{
    ArtifactRef, Digest, Text,
    control::CurrentPhase,
    transport::{JournalEvidence, JournalUnavailable, WorkerIdentity},
};
use std::collections::BTreeMap;

fn offered() -> Snapshot {
    let attempt_id = Uuid::new_v4();
    Snapshot {
        binding: LaunchBinding {
            schema_version: 1,
            session: SessionIdentity::Remote {
                node_id: Uuid::new_v4(),
                boot_id: Uuid::new_v4(),
                attempt_id,
                assignment_generation: 1,
                session_id: Uuid::new_v4(),
            },
            offer_id: Uuid::new_v4(),
            attempt_id,
            assignment_generation: 1,
            execution_spec_digest: Digest::of(b"spec"),
            launch_manifest_digest: Digest::of(b"manifest"),
            graph_access_profile_digest: Digest::of(b"profile"),
        },
        versions: Versions {
            record_revision: 1,
            authority_revision: 1,
            grant_revision: 0,
        },
        hot: HotState {
            schema_version: 1,
            status: Status::Offered,
            grant: None,
            execution_deadline_ms: None,
            owner: None,
            preparation_operation_id: None,
            cancellation_id: None,
            revoked: false,
            stop: Stop::Unknown,
            retention: Retention::Unknown,
            effects: Effects::Unknown,
        },
        holds_task: true,
        offer_expiry: 1000,
        preparation: None,
        started: None,
        terminal: None,
    }
}

#[test]
fn offer_expiry_is_exclusive() {
    assert!(matches!(
        offered().render(999, true).unwrap().phase,
        CurrentPhase::Offered {
            expires_at_ms: 1000
        }
    ));
    assert!(matches!(
        offered().render(1000, true).unwrap().phase,
        CurrentPhase::StopRequired {
            reason: FailureCode::Expired,
            ..
        }
    ));
}

#[test]
fn revocation_precedes_cancellation_and_recovery() {
    for (time, revoked, cancelled, expected) in [
        (1000, true, true, "credential_revoked"),
        (1000, false, true, "cancelled"),
        (999, false, false, "recovery_required"),
    ] {
        let mut snapshot = offered();
        snapshot.hot.revoked = revoked;
        snapshot.hot.cancellation_id = cancelled.then(Uuid::new_v4);
        let CurrentPhase::StopRequired { reason, .. } = snapshot.render(time, false).unwrap().phase
        else {
            panic!("expected stop required for {expected:?}");
        };
        assert_eq!(serde_json::to_value(reason).unwrap(), expected);
    }
}

#[test]
fn released_history_requires_terminal_evidence_and_supplies_no_live_authority() {
    let mut snapshot = offered();
    snapshot.holds_task = false;
    assert!(snapshot.render(999, true).is_none());

    let mut snapshot = offered();
    snapshot.holds_task = false;
    let release = ArtifactRef {
        id: Text::new(format!(
            "{}{}",
            aq_machine_control::lapse::RECEIPT,
            snapshot.binding.attempt_id
        ))
        .unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(b"retained release evidence"),
    };
    snapshot.terminal = Some(release.clone());
    assert!(matches!(
        snapshot.render(999, true).unwrap().phase,
        CurrentPhase::Released { release: actual } if actual == release
    ));
}

#[test]
fn explicit_reads_distinguish_missing_from_invalid_history_and_scans_omit_invalid_rows() {
    let snapshot = offered();
    let binding = snapshot.binding.clone();
    let id = binding.attempt_id;
    let SessionIdentity::Remote {
        node_id, boot_id, ..
    } = binding.session
    else {
        unreachable!()
    };
    let mut request = Reconcile {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: WorkerIdentity { node_id, boot_id },
        attempts: vec![ReconcileAttempt {
            binding,
            guardian_id: Uuid::new_v4(),
            launch_id: Uuid::new_v4(),
            last_grant_id: None,
            journal: JournalEvidence::Unavailable {
                reason: JournalUnavailable::Missing,
            },
        }],
        after_attempt: None,
        limit: 32,
    };
    assert!(matches!(
        render_attempts(BTreeMap::new(), &request, 999, true).as_slice(),
        [ReconcileEntry::Refused {
            code: FailureCode::UnknownOrNotOwned,
            ..
        }]
    ));
    assert!(matches!(
        render_attempts(BTreeMap::from([(id, Err(invalid()))]), &request, 999, true).as_slice(),
        [ReconcileEntry::Refused {
            code: FailureCode::RecoveryRequired,
            ..
        }]
    ));
    request.attempts.clear();
    let rows = BTreeMap::from([(id, Ok(snapshot)), (Uuid::new_v4(), Err(invalid()))]);
    assert!(matches!(
        render_attempts(rows, &request, 999, true).as_slice(),
        [ReconcileEntry::Current { .. }]
    ));
}
