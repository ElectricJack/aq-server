use super::*;

#[test]
fn checkpoints_allow_only_exact_replay_or_the_next_phase() {
    let phases = ["reserved", "prepared", "started", "stopped"];
    for (from, phase) in phases.iter().enumerate() {
        let previous = Checkpoint {
            phase,
            grant: Some(Uuid::from_u128(1)),
            revision: 5,
            digest: "original",
        };
        for (to, phase) in phases.iter().enumerate() {
            let next = Checkpoint {
                phase,
                revision: if from == to { 5 } else { 6 },
                ..previous
            };
            let result = previous.transition_to(&next);
            if from == to {
                assert_eq!(result.unwrap(), Transition::Replay);
            } else if to == from + 1 {
                assert_eq!(result.unwrap(), Transition::Advance);
            } else {
                assert!(matches!(result, Err(AppError::Conflict(_))));
            }
        }
        assert!(
            previous
                .transition_to(&Checkpoint {
                    phase: "unrecognized",
                    ..previous
                })
                .is_err()
        );
    }
}

#[test]
fn same_phase_rejects_changed_grant_revision_or_digest() {
    let original = Checkpoint {
        phase: "started",
        grant: Some(Uuid::from_u128(1)),
        revision: 5,
        digest: "original",
    };
    for changed in [
        Checkpoint {
            grant: None,
            ..original
        },
        Checkpoint {
            grant: Some(Uuid::from_u128(2)),
            ..original
        },
        Checkpoint {
            revision: 4,
            ..original
        },
        Checkpoint {
            revision: 6,
            ..original
        },
        Checkpoint {
            digest: "changed",
            ..original
        },
    ] {
        assert!(matches!(
            original.transition_to(&changed),
            Err(AppError::Conflict(_))
        ));
    }
}

#[test]
fn advancing_requires_a_newer_revision_but_allows_journal_gaps() {
    for (phase, next_phase) in [
        ("reserved", "prepared"),
        ("prepared", "started"),
        ("started", "stopped"),
    ] {
        let previous = Checkpoint {
            phase,
            grant: None,
            revision: 5,
            digest: "original",
        };
        for revision in [4, 5, 6, 12, i64::MAX] {
            let next = Checkpoint {
                phase: next_phase,
                grant: Some(Uuid::from_u128(1)),
                revision,
                digest: "new checkpoint",
            };
            let result = previous.transition_to(&next);
            if revision > 5 {
                assert_eq!(result.unwrap(), Transition::Advance);
            } else {
                assert!(matches!(result, Err(AppError::Conflict(_))));
            }
        }
    }
}
