//! Actual untouched 0.1.9 capture. Refresh implementation cannot rewrite it.
mod preparation_v2_common;
use aq_admission::preparation_v2::{decode, schema};
use aq_admission::{control::DeliveryStamp, preparation_v2::*, transport::DeliveryClock, *};
use preparation_v2_common::*;
use serde_json::{Value, json};
fn observed() -> Value {
    let c = case();
    let accepted_delivery = AcceptedDelivery {
        schema_version: 2,
        receipt: c.accepted.clone(),
        delivery: DeliveryStamp {
            nonce: id(1001),
            clock: DeliveryClock::new(3000, 31500).unwrap(),
        },
    };
    let accepted_time = accepted_delivery
        .sample(
            &c.accept,
            &c.proposal,
            id(1001),
            1_000_000_000,
            1_100_000_000,
            None,
        )
        .unwrap();
    let execution_time = c
        .delivery
        .sample(
            &c.prepare,
            &c.accepted,
            &c.accept,
            &c.proposal,
            id(1000),
            10_000_000_000,
            10_100_000_000,
            None,
        )
        .unwrap();
    let mut errors = Vec::new();
    let mut record = |name: String, result: aq_admission::Result<()>| {
        errors.push(json!({"case":name,"error":result.unwrap_err().to_string()}));
    };
    for mode in 0..4 {
        let mut p = c.proposal.clone();
        match mode {
            0 => p.negotiation.contract.digest = Digest::of(b"foreign contract"),
            1 => {
                p.negotiation.features.pop();
            }
            2 => p.negotiation.features.push(p.negotiation.features[0]),
            _ => p.choice.selection_digest = Digest::of(b"foreign selection"),
        }
        record(format!("proposal-{mode}"), p.validate());
    }
    for mode in 0..5 {
        let mut a = c.accepted.clone();
        match mode {
            0 => a.lease.issued_at_ms = 2000,
            1 => a.request_digest = Digest::of(b"changed request"),
            2 => a.lease.hold.resources.disk_bytes += 1,
            3 => a.lease.identity.boot_id = id(9999),
            _ => a.lease.expires_at_ms = 100001,
        }
        record(
            format!("accepted-{mode}"),
            a.validate_for(&c.accept, &c.proposal),
        );
    }
    for mode in 0..4 {
        let mut q = c.prepare.clone();
        match mode {
            0 => {
                q.original.artifacts.pop();
            }
            1 => q.original.artifacts[0].bytes += 1,
            2 => q.original.materials[0].material_digest = Digest::of(b"other material"),
            _ => q.expected_preparation_revision += 1,
        }
        record(
            format!("prepare-{mode}"),
            q.validate_for(&c.accepted, &c.accept, &c.proposal),
        );
    }
    for mode in 0..13 {
        let mut f = c.execution.confirmation.clone();
        match mode {
            0 => {
                f.facts.pop();
            }
            1 => f.facts[1] = f.facts[0].clone(),
            2 => f.facts[0].record.digest = Digest::of(b"other fact"),
            3 => {
                f.facts[0].body.expires_at_ms += 1;
                resign(&mut f.facts[0]);
            }
            4 => {
                f.facts[0].body.authority = reference("foreign-owner");
                resign(&mut f.facts[0]);
            }
            _ => {
                let kind = match mode {
                    5..=7 => FactKind::Availability,
                    8 => FactKind::Principal,
                    9 => FactKind::Inventory,
                    10 => FactKind::Budget,
                    11 => FactKind::Context,
                    _ => FactKind::Registry,
                };
                let fact = f
                    .facts
                    .iter_mut()
                    .find(|x| x.body.value.kind() == kind)
                    .unwrap();
                match &mut fact.body.value {
                    FactValue::Availability { state, .. } => {
                        *state = match mode {
                            5 => AvailabilityState::Unknown,
                            6 => AvailabilityState::Cooldown,
                            _ => AvailabilityState::Unavailable,
                        }
                    }
                    FactValue::Principal { principal, .. } => {
                        *principal = reference("foreign-principal")
                    }
                    FactValue::Inventory { harness, .. } => {
                        harness.cli_version = Text::new("different").unwrap()
                    }
                    FactValue::Budget { elapsed_ms, .. } => *elapsed_ms = None,
                    FactValue::Context { task, .. } => task.version += 1,
                    FactValue::Registry { registry, .. } => {
                        registry.version = Text::new("2").unwrap()
                    }
                    _ => unreachable!(),
                }
                resign(fact);
            }
        }
        record(
            format!("confirmation-{mode}"),
            f.validate_for(&c.prepare, &c.accepted, &c.accept, &c.proposal),
        );
    }
    for mode in 0..3 {
        let mut x = c.execution.clone();
        match mode {
            0 => x.issued_at_ms = c.accepted.lease.expires_at_ms,
            1 => x.execution_deadline_ms += 1,
            _ => x.request_digest = Digest::of(b"other request"),
        }
        record(
            format!("execution-{mode}"),
            x.validate_for(&c.prepare, &c.accepted, &c.accept, &c.proposal),
        );
    }
    let mut wire = documents(&c);
    wire.push(Document::AcceptedDelivery(Box::new(accepted_delivery)));
    for d in &wire {
        decode(&canonical(d)).unwrap();
    }
    json!({"schema":schema(),"contract":{"artifact":contract_artifact().unwrap(),"manifest":contract_manifest().unwrap()},
        "documents":wire,"digests":{"proposal":c.proposal.digest().unwrap(),"accept":c.accept.digest().unwrap(),
        "accepted":c.accepted.digest().unwrap(),"prepare":c.prepare.digest().unwrap(),
        "confirmation":c.execution.confirmation.digest().unwrap(),"execution":c.execution.digest().unwrap()},
        "fact_digests":c.execution.confirmation.facts.iter().map(|f|OwnerFact::record_digest(&f.body).unwrap()).collect::<Vec<_>>(),
        "samples":{"accepted":{"deadline_ns":accepted_time.deadline_ns(),"hard_deadline_ns":accepted_time.hard_deadline_ns()},
        "execution":{"deadline_ns":execution_time.deadline_ns(),"hard_deadline_ns":execution_time.hard_deadline_ns()}},
        "errors":errors})
}
#[test]
fn untouched_019_phase_wire_schema_pins_clocks_and_errors_stay_exact() {
    let value = observed();
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("artifacts/preparation-v2.actual-019-before.json");
    if std::env::var_os("AQ_CAPTURE_UNTOUCHED_ADMISSION_019").is_some() {
        assert_eq!(
            env!("CARGO_PKG_VERSION"),
            "0.1.9",
            "capture only before refresh implementation"
        );
        let metadata = json!({"crate_version":env!("CARGO_PKG_VERSION"),
            "phase_source_digest":Digest::of(include_bytes!("../src/preparation_v2.rs")),
            "facts_source_digest":Digest::of(include_bytes!("../src/preparation_v2/facts.rs"))});
        std::fs::write(
            path,
            format!(
                "{}\n",
                serde_json::to_string_pretty(&json!({"capture":metadata,"output":value})).unwrap()
            ),
        )
        .unwrap();
    } else {
        let old: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(value, old["output"], "actual untouched019 behavior changed");
    }
}
