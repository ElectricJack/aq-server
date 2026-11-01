//! Actual outputs/errors captured on untouched selector0.1.3 before central extraction.
mod common;
use aq_selection::*;
use common::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use uuid::Uuid;
fn record(
    cases: &mut BTreeMap<String, Value>,
    name: &str,
    catalog: &aq_profiles::ValidatedCatalog,
    input: &SelectionInputs,
) {
    let result = match select(catalog, input) {
        Ok(v) => json!({"ok":v}),
        Err(e) => json!({"error":e.to_string()}),
    };
    cases.insert(
        name.into(),
        json!({"input_digest":digest(&(catalog.artifact(),input)).unwrap(),"outcome":result}),
    );
}
#[test]
fn task_selection_wire_errors_match_untouched_013() {
    let mut cases = BTreeMap::new();
    let (c, i) = fixture();
    record(&mut cases, "base", &c, &i);
    for kind in 0..22 {
        let (mut c, mut i) = fixture();
        match kind {
            0 => i.credentials.reverse(),
            1 => i.observations.reverse(),
            2 => i.observations[0].state = Availability::QuotaUnavailable,
            3 => i.credentials[0].enabled = false,
            4 => i.credentials[0].provisionable = false,
            5 => i.node.credential_bindings.clear(),
            6 => i.node.expires_at_ms = i.now_ms,
            7 => i.node.harnesses[0].models.clear(),
            8 => i.node.harnesses[0].limits.max_output_tokens = 1,
            9 => i.project.allowed_environments = vec!["denied".into()],
            10 => i.budget.attempts_started = 3,
            11 => {
                i.budget.attempts_started = 1;
                i.budget.last_attempt_id = Some(Uuid::from_u128(600));
            }
            12 => i.budget.elapsed_ms = None,
            13 => {
                i.project.ceilings.cost_limit_microunits = Some(1);
                i.project.ceilings.currency = Some("USD".into());
            }
            14 => i.observations.clear(),
            15 => {
                let mut p = policy();
                p.allow_unknown_availability = true;
                c = change_policy(&c, p, &mut i);
                i.observations.clear();
            }
            16 => {
                let mut p = policy();
                p.fallback_on.clear();
                c = change_policy(&c, p, &mut i);
                i.credentials[0].enabled = false;
            }
            17 => i.project.revision += 1,
            18 => i.project.lineage.task_id = Uuid::nil(),
            19 => i.compositions[0].composition.instruction_order[0]
                .text
                .push('!'),
            20 => {
                i.node.target = ExecutionTarget::Central {
                    authority_id: Uuid::from_u128(333),
                    generation: 1,
                }
            }
            _ => i.observations[0].received_at_ms = i.now_ms + 1,
        }
        record(&mut cases, &format!("vector_{kind:02}"), &c, &i);
    }
    let actual = json!({"source":"untouched aq-selection0.1.3","cases":cases});
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/task-selection-0.1.3.json");
    if std::env::var_os("AQ_CAPTURE_UNTOUCHED_SELECTION_013").is_some() {
        assert_eq!(
            env!("CARGO_PKG_VERSION"),
            "0.1.3",
            "only untouched selector may capture baseline"
        );
        std::fs::write(
            &path,
            format!("{}\n", serde_json::to_string_pretty(&actual).unwrap()),
        )
        .unwrap();
    } else {
        let expected: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(actual, expected);
    }
}
