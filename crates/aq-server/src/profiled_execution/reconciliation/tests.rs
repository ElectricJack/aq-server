use super::*;
use crate::execution::profiled::{Effects, Retention};
use aq_admission::transport::{JournalEvidence, JournalUnavailable, WorkerIdentity};
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
fn offer_expiry_is_exclusive_and_revocation_precedes_cancellation_and_recovery() {
    let mut snapshot = offered();
    assert!(matches!(
        snapshot.phase(999, true).unwrap(),
        CurrentPhase::Offered { .. }
    ));
    assert!(matches!(
        snapshot.phase(1000, true).unwrap(),
        CurrentPhase::StopRequired {
            reason: FailureCode::Expired,
            ..
        }
    ));
    snapshot.hot.revoked = true;
    snapshot.hot.cancellation_id = Some(Uuid::new_v4());
    assert!(matches!(
        snapshot.phase(1000, false).unwrap(),
        CurrentPhase::StopRequired {
            reason: FailureCode::CredentialRevoked,
            ..
        }
    ));
    snapshot.hot.revoked = false;
    assert!(matches!(
        snapshot.phase(1000, false).unwrap(),
        CurrentPhase::StopRequired {
            reason: FailureCode::Cancelled,
            ..
        }
    ));
    snapshot.hot.cancellation_id = None;
    assert!(matches!(
        snapshot.phase(999, false).unwrap(),
        CurrentPhase::StopRequired {
            reason: FailureCode::RecoveryRequired,
            ..
        }
    ));
    snapshot.holds_task = false;
    assert!(snapshot.phase(999, true).is_err());
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
