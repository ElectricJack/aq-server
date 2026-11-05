#![allow(dead_code)]
#[path = "startup_common.rs"]
pub mod startup_common;
use aq_admission::{control::*, transport::*, *};
use startup_common::{common::*, fixture as native_fixture};
pub fn fixture() -> (
    startup::NativeOffer,
    Accept,
    PreparationDelivery,
    Prepare,
    InitialGrantDelivery,
    Renew,
    RenewedAuthority,
) {
    let (offer, _) = native_fixture();
    let binding = offer.offered.offer.binding.clone();
    let identity = match binding.session {
        SessionIdentity::Remote {
            node_id, boot_id, ..
        } => WorkerIdentity { node_id, boot_id },
        _ => unreachable!(),
    };
    let reservation = NoLaunchReservation {
        reservation_id: id(100),
        reservation_operation_id: id(101),
        reservation_journal_digest: Digest::of(b"reserved"),
    };
    let accept = Accept {
        schema_version: 1,
        operation_id: id(102),
        identity: identity.clone(),
        acceptance: aq_session_protocol::ProfiledAcceptance {
            binding: binding.clone(),
            advertised_capabilities: offer.offered.offer.required_capabilities.clone(),
        },
        reservation: reservation.clone(),
        guardian_id: id(103),
        launch_id: id(104),
        journal_revision: 1,
        journal_digest: Digest::of(b"reserved-1"),
        reservation_remaining_ms: 500,
    };
    let mut material = lease();
    material.binding = binding.clone();
    material.credential_profile = offer.offered.offer.manifest.credentials[0]
        .credential_profile
        .clone();
    material.provider = Label::new(offer.offered.offer.manifest.model.provider.as_str()).unwrap();
    material.purpose =
        Label::new(offer.offered.offer.manifest.credentials[0].purpose.as_str()).unwrap();
    let plan = PreparationPlan {
        schema_version: 1,
        operation_id: accept.operation_id,
        binding: binding.clone(),
        reservation: reservation.clone(),
        guardian_id: accept.guardian_id,
        launch_id: accept.launch_id,
        clock: DeliveryClock::new(1500, 1900).unwrap(),
        materials: vec![material.clone()],
    };
    let preparation = PreparationDelivery {
        schema_version: 1,
        original: plan.clone(),
        versions: Versions {
            record_revision: 2,
            authority_revision: 2,
            grant_revision: 0,
        },
        delivery: DeliveryStamp {
            nonce: id(110),
            clock: DeliveryClock::new(1600, 1900).unwrap(),
        },
    };
    let receipt = MaterialReceipt {
        schema_version: 1,
        operation_id: id(111),
        lease_id: material.lease_id,
        binding: binding.clone(),
        material_digest: material.material_digest.clone(),
        private_binding: artifact("synthetic-private-binding"),
        received_at_ms: 1600,
    };
    let prepare = Prepare {
        schema_version: 1,
        operation_id: id(112),
        identity: identity.clone(),
        binding: binding.clone(),
        reservation,
        guardian_id: accept.guardian_id,
        launch_id: accept.launch_id,
        journal_revision: 2,
        journal_digest: Digest::of(b"prepared-2"),
        artifacts: offer
            .offered
            .artifacts
            .iter()
            .map(|a| ArtifactReceipt {
                artifact: a.artifact.clone(),
                bytes: a.bytes,
                digest: a.artifact.digest.clone(),
            })
            .collect(),
        materials: vec![receipt],
    };
    let granted = GrantedLaunch {
        schema_version: 1,
        grant: aq_session_protocol::ProfiledGrant {
            binding: binding.clone(),
            grant_id: id(113),
            expires_at_ms: 1800,
        },
        reservation_id: accept.reservation.reservation_id,
        guardian_id: accept.guardian_id,
        launch_id: accept.launch_id,
        clock: DeliveryClock::new(1500, 1800).unwrap(),
        materials: vec![material],
    };
    let initial = InitialGrantDelivery {
        schema_version: 1,
        operation_id: prepare.operation_id,
        original: granted,
        execution_deadline_ms: 2400,
        versions: Versions {
            record_revision: 3,
            authority_revision: 3,
            grant_revision: 1,
        },
        delivery: DeliveryStamp {
            nonce: id(114),
            clock: DeliveryClock::new(1600, 1800).unwrap(),
        },
    };
    let renew = Renew {
        schema_version: 1,
        operation_id: id(115),
        identity,
        binding: binding.clone(),
        grant_id: initial.original.grant.grant_id,
        reservation_id: initial.original.reservation_id,
        guardian_id: initial.original.guardian_id,
        launch_id: initial.original.launch_id,
        journal_revision: 4,
    };
    let renewed = RenewedAuthority {
        schema_version: 1,
        operation_id: renew.operation_id,
        binding,
        grant_id: renew.grant_id,
        reservation_id: renew.reservation_id,
        guardian_id: renew.guardian_id,
        launch_id: renew.launch_id,
        journal_revision: renew.journal_revision,
        execution_deadline_ms: initial.execution_deadline_ms,
        versions: Versions {
            record_revision: 4,
            authority_revision: 4,
            grant_revision: 2,
        },
        delivery: DeliveryStamp {
            nonce: id(116),
            clock: DeliveryClock::new(1700, 2200).unwrap(),
        },
    };
    (offer, accept, preparation, prepare, initial, renew, renewed)
}
pub fn publication() -> serde_json::Value {
    let (offer, accept, preparation, prepare, initial, renew, renewed) = fixture();
    let poll = Poll {
        schema_version: 1,
        operation_id: id(140),
        identity: accept.identity.clone(),
        inventory_sequence: 1,
        available: Resources {
            slots: 1,
            memory_bytes: 1,
            cpu_millis: 1,
            disk_bytes: 1,
        },
        limit: 1,
        after_offer: None,
    };
    let poll_page = NativePollPage {
        schema_version: 1,
        operation_id: poll.operation_id,
        identity: poll.identity.clone(),
        server_time_ms: offer.offered.clock.server_time_ms,
        offers: vec![offer.clone()],
        cancellations: vec![],
        next_after: None,
    };
    let reconcile = Reconcile {
        schema_version: 1,
        operation_id: id(150),
        identity: accept.identity.clone(),
        after_attempt: None,
        limit: 1,
        attempts: vec![ReconcileAttempt {
            binding: initial.original.grant.binding.clone(),
            guardian_id: accept.guardian_id,
            launch_id: accept.launch_id,
            last_grant_id: Some(initial.original.grant.grant_id),
            journal: JournalEvidence::Recorded {
                revision: 3,
                digest: Digest::of(b"started"),
            },
        }],
    };
    let reconciled = ReconcilePage {
        schema_version: 1,
        operation_id: reconcile.operation_id,
        identity: reconcile.identity.clone(),
        delivery_nonce: id(152),
        server_time_ms: 1700,
        attempts: vec![ReconcileEntry::Current {
            authority: Box::new(CurrentAuthority {
                binding: initial.original.grant.binding.clone(),
                versions: initial.versions.clone(),
                phase: CurrentPhase::Granted {
                    owner: LaunchOwner {
                        reservation_id: initial.original.reservation_id,
                        guardian_id: initial.original.guardian_id,
                        launch_id: initial.original.launch_id,
                    },
                    grant_id: initial.original.grant.grant_id,
                    expires_at_ms: initial.original.grant.expires_at_ms,
                    execution_deadline_ms: initial.execution_deadline_ms,
                    started_operation_id: Some(id(151)),
                },
            }),
        }],
        next_after: None,
    };
    serde_json::json!({"native_offer":offer,"requests":{"poll":poll,"accept":accept,"prepare":prepare,"renew":renew,"reconcile":reconcile},"replies":[ControlReply::Poll(poll_page),ControlReply::Preparation(preparation),ControlReply::InitialGrant(initial),ControlReply::Renewed(renewed),ControlReply::Reconciled(reconciled),ControlReply::Failure(Failure{schema_version:1,code:FailureCode::Expired,operation_id:Some(id(160))})]})
}
