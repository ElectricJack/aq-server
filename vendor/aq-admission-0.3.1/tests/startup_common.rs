#![allow(dead_code)]
#[path = "common.rs"]
pub mod common;
use aq_admission::{startup::*, transport::*, *};
use aq_session_protocol::{ContextDelivery, ContextRef, canonical_digest};
use common::*;
pub fn fixture() -> (NativeOffer, Vec<u8>) {
    let mut offer: ProfiledOffer =
        serde_json::from_str(include_str!("../artifacts/shared-launch.json")).unwrap();
    offer.manifest.bindings.source = artifact("source");
    offer.manifest.bindings.outcome = artifact("outcome");
    offer.manifest.selection_explanation = artifact("selection");
    offer.manifest.context.references = vec![
        ContextRef {
            artifact: artifact("instructions"),
            provenance: Text::new("profile").unwrap(),
            delivery: ContextDelivery::Startup,
            bytes: 12,
            tokens: 12,
            required: true,
        },
        ContextRef {
            artifact: artifact("task-context"),
            provenance: Text::new("authorized snapshot").unwrap(),
            delivery: ContextDelivery::Startup,
            bytes: 12,
            tokens: 12,
            required: true,
        },
    ];
    offer.manifest.context.startup_byte_budget = 4096;
    offer.manifest.context.startup_token_budget = 4096;
    offer.manifest.context.context_window_tokens = 8192;
    offer.manifest.context.reserved_output_tokens = 4096;
    offer.manifest.model.max_output_tokens = 4096;
    offer.binding.launch_manifest_digest = canonical_digest(&offer.manifest).unwrap();
    let bytes = "instructions\n\ntask-context".as_bytes().to_vec();
    let payload = ArtifactRef {
        digest: Digest::of(&bytes),
        ..artifact("rendered-startup")
    };
    let renderer = artifact("synthetic-renderer-qualification");
    let mut artifacts: Vec<_> = [
        ("fixture", ArtifactKind::Definition),
        ("source", ArtifactKind::Source),
        ("outcome", ArtifactKind::Outcome),
        ("selection", ArtifactKind::Selection),
        ("instructions", ArtifactKind::Context),
        ("task-context", ArtifactKind::Context),
        ("synthetic-renderer-qualification", ArtifactKind::Definition),
    ]
    .into_iter()
    .map(|(name, kind)| RetainedArtifact {
        artifact: artifact(name),
        kind,
        bytes: name.len() as u64,
        retention_receipt: artifact("synthetic-only-receipt"),
        retained_at_ms: 1000,
        retain_until_ms: 2000,
    })
    .collect();
    artifacts.push(RetainedArtifact {
        artifact: payload.clone(),
        kind: ArtifactKind::Context,
        bytes: bytes.len() as u64,
        retention_receipt: artifact("synthetic-only-receipt"),
        retained_at_ms: 1000,
        retain_until_ms: 2000,
    });
    let startup = Descriptor {
        schema_version: 1,
        payload,
        renderer_qualification: renderer,
        startup_inputs: vec![
            InputSpan {
                artifact: artifact("instructions"),
                offset: 0,
                bytes: 12,
            },
            InputSpan {
                artifact: artifact("task-context"),
                offset: 14,
                bytes: 12,
            },
        ],
        payload_bytes: bytes.len() as u64,
        payload_tokens_upper_bound: 30,
        implicit_tokens_upper_bound: 20,
    };
    let descriptor = startup.artifact().unwrap();
    let descriptor_bytes = aq_session_protocol::canonical_bytes(&startup).unwrap();
    offer.manifest.modules.push(descriptor.clone());
    artifacts.push(RetainedArtifact {
        artifact: descriptor,
        kind: ArtifactKind::Definition,
        bytes: descriptor_bytes.len() as u64,
        retention_receipt: artifact("synthetic-only-receipt"),
        retained_at_ms: 1000,
        retain_until_ms: 2000,
    });
    offer.binding.launch_manifest_digest = canonical_digest(&offer.manifest).unwrap();
    (
        NativeOffer {
            schema_version: 1,
            startup,
            offered: OfferedLaunch {
                lifecycle_binding: lifecycle_binding(&offer).unwrap(),
                offer,
                clock: DeliveryClock::new(1500, 2000).unwrap(),
                reservation_ttl_ms: 500,
                required_features: vec![
                    Feature::ArtifactTransferV1,
                    Feature::ServerMaterialV1,
                    Feature::DurableHostJournalV1,
                ],
                artifacts,
            },
        },
        bytes,
    )
}
