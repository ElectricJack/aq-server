#![allow(dead_code)]
#[path = "common.rs"]
mod common;
use aq_admission::{Digest, Label, started::*};
use common::*;
pub fn request() -> Request {
    let observation = RunningObservation {
        guardian: GuardianIdentity {
            intent: LaunchIntent {
                launch_id: id(71),
                session: binding().session,
                manifest_digest: binding().launch_manifest_digest,
                configuration_digest: Digest::of(b"sealed-local-launch"),
            },
            nonce: id(79),
            process: ProcessEvidence {
                pid: 10,
                start_ticks: 100,
            },
            machine_boot_id: id(80),
            pid_namespace: Label::new("pid:[4026532000]").unwrap(),
        },
        phase: RunningPhase::Running,
        pane: PaneIdentity {
            server: ProcessEvidence {
                pid: 11,
                start_ticks: 101,
            },
            pane: ProcessEvidence {
                pid: 12,
                start_ticks: 102,
            },
            pane_id: Label::new("%0").unwrap(),
            session_name: Label::new("aq").unwrap(),
        },
        permit_sequence: 1,
        permit_deadline_boot_ms: 30_000,
        observed_boot_ms: 1_500,
        reaped_count: 0,
        kernel_wait_echild: false,
        root_status: None,
        stop_reason: None,
    };
    Request {
        schema_version: 1,
        operation_id: id(72),
        binding: binding(),
        grant_id: id(73),
        guardian_id: id(70),
        launch_id: id(71),
        reservation_id: id(74),
        runtime: artifact("qualified-worker-runtime"),
        qualification: artifact("host-start-qualification"),
        journal_revision: 3,
        journal_digest: Digest::of(b"durable-started-owner-record"),
        observation_digest: observation.digest().unwrap(),
        observation,
    }
}
