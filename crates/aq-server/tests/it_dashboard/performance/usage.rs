//! The same receipt and correction traverse the real canonical accounting owner
//! and the mounted dashboard read. Synthetic rates and transcript identifiers.
use super::{CLAIM, COMPLETE, CUTOFF, MINUTE, success};
use crate::common::profiled::{Fixture, NOW, artifact, fixture, insert};
use aq_accounting::{
    Category, Layout,
    pricing::{RateCard, RateCardRevision, Rule},
    reconcile::{ParserAuthority, Policy},
    wire::*,
};
use aq_server::{
    execution::profiled::{Status, Stop},
    intelligence::config::{AccountingCatalog, IntelligenceConfig, LoadedRegistry},
    observations::{self, OwnerSource},
};
use aq_session_protocol::ProfiledGrant;
use serde_json::json;
use std::sync::{Arc, atomic::Ordering};
use uuid::Uuid;

fn text(label: &str) -> Text {
    Text::new(label).unwrap()
}

async fn prepare_source(f: &mut Fixture) -> OwnerSource {
    use aq_observation_admission::{Operation, Request};
    let mutations = |operation| Request {
        version: 1,
        operation_id: Uuid::new_v4(),
        node_id: f.node.node_id,
        operation,
    };
    let prepared = observations::transport::mutate(
        &f.state,
        f.node.node_id,
        &mutations(Operation::Prepare {
            boot_id: f.node.boot_id,
            store_id: Uuid::new_v4(),
        }),
    )
    .await
    .unwrap();
    observations::transport::mutate(
        &f.state,
        f.node.node_id,
        &mutations(Operation::Activate {
            marker: prepared.current_owner.marker,
        }),
    )
    .await
    .unwrap();
    f.profiled.status = Status::Granted;
    f.profiled.execution_deadline_ms = Some(NOW + 60_000);
    f.profiled.grant = Some(ProfiledGrant {
        binding: f.profiled.offer.binding.clone(),
        grant_id: Uuid::new_v4(),
        expires_at_ms: NOW + 5_000,
    });
    f.profiled.stop = Stop::Running;
    insert(f).await;
    let parser = artifact("performance-test-parser", b"synthetic parser");
    let source = OwnerSource {
        binding: aq_admission::transport::lifecycle_binding(&f.profiled.offer).unwrap(),
        source_id: Uuid::new_v4(),
        generation: 1,
        native_session_key: text("qualification-session"),
        account_claim: text(&f.selection.choice.account_reference),
        policy: Policy {
            reference: artifact("performance-test-policy", b"synthetic policy"),
            account_scope: Digest::of(b"qualification-account"),
            call_namespace: Digest::of(b"qualification-namespace"),
            provider: text(&f.selection.choice.provider),
            layout: Layout {
                policy: artifact("performance-test-layout", b"synthetic layout"),
                input_includes_cache_read: false,
                input_includes_cache_write: false,
                output_includes_reasoning: false,
            },
            parsers: vec![ParserAuthority {
                parser: parser.clone(),
                category_claim: parser,
                priority: 1,
                complete_accounting_evidence: Some(artifact(
                    "performance-test-completeness",
                    b"synthetic evidence",
                )),
            }],
        },
        billing: Some(EstimateBasis::TokenRates),
    };
    let rate_cards = ["cheap-model", "additional-call-model", "escalated-model"]
        .into_iter()
        .map(|model| RateCardRevision {
            revision: 1,
            corrects: None,
            card: RateCard {
                reference: artifact(model, model.as_bytes()),
                allocation_policy: source.policy.reference.clone(),
                currency: text("USD"),
                provider: source.policy.provider.clone(),
                model: text(model),
                layout: source.policy.layout.clone(),
                rules: Category::ALL
                    .into_iter()
                    .map(|category| Rule {
                        category,
                        subtract: Vec::new(),
                        microunits_per_million_tokens: if category == Category::Input {
                            1_000_000
                        } else {
                            0
                        },
                    })
                    .collect(),
            },
        })
        .collect();
    Arc::make_mut(&mut f.state.config).intelligence = Some(Arc::new(LoadedRegistry {
        config: IntelligenceConfig {
            revision: 1,
            accounting: AccountingCatalog {
                sources: Vec::new(),
                rate_cards,
            },
            ..Default::default()
        },
        credentials: Vec::new(),
        fingerprints: Vec::new(),
    }));
    observations::register_source(&f.state, &source)
        .await
        .unwrap();
    source
}

fn event(
    source: &OwnerSource,
    sequence: u64,
    model: &str,
    tokens: u64,
    correction_of: Option<EventRef>,
    at_ms: i64,
) -> ObservationRecord {
    let mut counters = aq_accounting::zeros();
    counters.input = Counter::Known { value: tokens };
    let parser = &source.policy.parsers[0];
    ObservationRecord::seal(
        Uuid::new_v4(),
        Observation {
            binding: source.binding.clone(),
            provenance: Provenance {
                source_id: source.source_id,
                source_generation: source.generation,
                source_kind: SourceKind::NativeTranscript,
                native_session_key: source.native_session_key.clone(),
                cursor: SourceCursor::Record {
                    id: text(&format!("record-{sequence}")),
                },
                parser: parser.parser.clone(),
                raw_record: RawRecordEvidence::Available {
                    digest: Digest::of(b"fixture"),
                    byte_length: 7,
                },
            },
            sequence,
            observed_at_ms: at_ms as u64,
            kind: ObservationKind::Usage {
                usage: Box::new(Usage {
                    call: ProviderCall {
                        provider: source.policy.provider.clone(),
                        account_reference: source.account_claim.clone(),
                        model: text(model),
                        native_call_id: text(model),
                    },
                    basis: CounterBasis::PerCall,
                    counters,
                    category_semantics: CategorySemantics::DeclaredDisjoint {
                        policy: parser.category_claim.clone(),
                    },
                    cost: Cost::Unknown {
                        reason: UnknownReason::NotReported,
                    },
                    completeness: Completeness::ReportedComplete,
                    correction_of,
                }),
            },
        },
    )
    .unwrap()
}

fn packet(f: &Fixture, events: Vec<ObservationRecord>) -> Packet {
    Packet::seal(
        Uuid::new_v4(),
        Message::Batch(ObservationBatch {
            node_id: f.node.node_id,
            boot_id: f.node.boot_id,
            first_sequence: events[0].body.sequence,
            last_sequence: events.last().unwrap().body.sequence,
            events,
        }),
    )
    .unwrap()
}

#[tokio::test]
async fn canonical_estimated_cost_replaces_corrected_call_after_restart() {
    let mut f = fixture().await;
    let source = prepare_source(&mut f).await;
    let project = f.selection.lineage.project_id;
    let task = format!("tasks/{}", f.selection.lineage.task_id);
    let first_at = CLAIM + MINUTE;
    let additional = event(&source, 2, "additional-call-model", 50_000, None, first_at);
    let first = packet(
        &f,
        vec![
            event(&source, 1, "cheap-model", 100_000, None, first_at),
            additional.clone(),
            event(&source, 3, "escalated-model", 350_000, None, first_at),
        ],
    );
    f.time.store(first_at, Ordering::SeqCst);
    let receipt = observations::ingest(&f.state, f.node.node_id, &first)
        .await
        .unwrap();
    assert_eq!(
        observations::ingest(&f.state, f.node.node_id, &first)
            .await
            .unwrap(),
        receipt
    );
    f.time.store(COMPLETE, Ordering::SeqCst);
    let app = aq_server::build_admin_router(f.state.clone());
    let original = success(&app, "summary", Some(project), COMPLETE).await;
    assert_cost(&original["usage"], 500_000);
    drop(app);

    let correction_at = COMPLETE + MINUTE;
    let correction = packet(
        &f,
        vec![event(
            &source,
            4,
            "additional-call-model",
            70_000,
            Some(EventRef {
                event_id: additional.event_id,
                digest: additional.body_digest,
            }),
            correction_at,
        )],
    );
    f.state.store = f.db.store().await;
    f.time.store(correction_at, Ordering::SeqCst);
    assert_eq!(
        observations::ingest(&f.state, f.node.node_id, &first)
            .await
            .unwrap(),
        receipt
    );
    let corrected_receipt = observations::ingest(&f.state, f.node.node_id, &correction)
        .await
        .unwrap();
    assert_eq!(
        observations::ingest(&f.state, f.node.node_id, &correction)
            .await
            .unwrap(),
        corrected_receipt
    );
    f.time.store(CUTOFF, Ordering::SeqCst);
    let app = aq_server::build_admin_router(f.state.clone());
    let old = success(&app, "summary", Some(project), COMPLETE).await;
    assert_cost(&old["usage"], 500_000);
    let corrected = success(&app, "summary", Some(project), CUTOFF).await;
    assert_cost(&corrected["usage"], 520_000);
    let drill_down = success(&app, &task, Some(project), CUTOFF).await;
    assert_cost(&drill_down["usage"], 520_000);
    let route = &drill_down["usage"]["first_route"];
    assert_eq!(route["attempt_id"], json!(source.binding.attempt_id));
    assert_eq!(route["candidate"], json!(f.selection.choice.candidate_id));
    assert_eq!(route["intended_model"], json!(f.selection.choice.model));
    let attempts = drill_down["usage"]["attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0]["attempt_id"], json!(source.binding.attempt_id));
}

fn assert_cost(usage: &serde_json::Value, expected_microunits: u64) {
    assert_eq!(usage["coverage"]["canonical_calls"], 3, "{usage}");
    assert_eq!(usage["coverage"]["priced_calls"], 3, "{usage}");
    assert_eq!(
        usage["costs"],
        json!([{"currency":"USD","basis":"token_rates","microunits":expected_microunits}]),
        "{usage}"
    );
}
