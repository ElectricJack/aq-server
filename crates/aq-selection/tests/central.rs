mod central_common;
mod common;
use aq_selection::{central::*, *};
use central_common::*;
use serde_json::{Value, json};
use uuid::Uuid;
#[test]
fn actual_central_compile_select_verify_has_exact_native_pins_and_no_execution_authority() {
    let (c, i) = fixture();
    let out = select_central(&c, &i).unwrap();
    verify_central_selection(&c, &i, &out).unwrap();
    let a = out.resolved.unwrap();
    assert_eq!(
        a.selection.choice.model_settings.provider.as_str(),
        "fixture-a"
    );
    assert_eq!(a.selection.choice.model_settings.reasoning.as_str(), "high");
    assert!(a.selection.choice.model_settings.spending_budget.is_none());
    assert_eq!(a.reference.digest, Digest::of(a.utf8.as_bytes()));
    assert_eq!(
        a.selection.choice.startup,
        i.compositions[0].startup.reference
    );
    assert_eq!(
        a.selection.reservation_policy,
        i.reservation.value.policy.reference
    );
    assert_eq!(a.selection.choice.resources.wall_time_ms, 10000);
    assert_eq!(a.selection.expires_at_ms, 4000);
    let encoded = serde_json::to_value(a.selection).unwrap();
    assert_eq!(encoded["accounting"], "unavailable");
    assert_eq!(encoded["authority"], "none");
    assert!(encoded["token_count"].is_null());
    assert!(encoded["spent_microunits"].is_null());
    for name in ["task_id", "project_id", "generation", "host_plan", "permit"] {
        assert!(encoded.get(name).is_none());
    }
}
#[test]
fn fallback_and_input_reordering_share_existing_selector_semantics() {
    let (c, mut i) = fixture();
    i.registry.value.observations[0].state = Availability::QuotaUnavailable;
    repin(&mut i);
    let a = select_central(&c, &i).unwrap();
    assert_eq!(
        a.resolved
            .unwrap()
            .selection
            .choice
            .model_settings
            .provider
            .as_str(),
        "fixture-b"
    );
    i.registry.value.credentials.reverse();
    i.registry.value.observations.reverse();
    repin(&mut i);
    let b = select_central(&c, &i).unwrap();
    assert_eq!(
        b.resolved
            .unwrap()
            .selection
            .choice
            .model_settings
            .provider
            .as_str(),
        "fixture-b"
    );
}
#[test]
fn each_real_policy_domain_is_independent_and_cannot_override_another() {
    for field in 0..8 {
        let (c, mut i) = fixture();
        match field {
            0 => i.reservation.value.policy.value.enabled = false,
            1 => i.policy.value.enabled = false,
            2 => i.policy.value.reservation_policy = common::artifact("wrong-policy"),
            3 => i.policy.value.composition_policy = common::artifact("wrong-composition-policy"),
            4 => i.policy.value.compatibility = common::artifact("wrong-compatibility"),
            5 => i.reservation.value.messages[0].payload_digest = Digest::of(b"changed original"),
            6 => i.reservation.value.conversation_revision += 1,
            _ => i.policy.value.intelligence_profile.digest = "a".repeat(64),
        };
        repin(&mut i);
        if field == 2 {
            i.policy.value.reservation_policy = common::artifact("wrong-policy");
            i.policy = Pinned::<CentralTurnPolicy>::new(i.policy.value).unwrap();
        }
        let out = select_central(&c, &i);
        if field < 2 {
            assert!(out.unwrap().resolved.is_none());
        } else {
            assert!(out.is_err(), "field{field}");
        }
    }
}
#[test]
fn forged_snapshot_and_original_artifact_bytes_refuse_even_after_rehash() {
    let (c, mut i) = fixture();
    i.registry.value.credentials[0].enabled = false;
    assert!(select_central(&c, &i).is_err());
    let (c, mut i) = fixture();
    i.compositions[0].startup.utf8.push('!');
    assert!(select_central(&c, &i).is_err());
    let (c, i) = fixture();
    let mut out = select_central(&c, &i).unwrap();
    out.resolved
        .as_mut()
        .unwrap()
        .selection
        .choice
        .account_reference = "other".into();
    assert!(verify_central_selection(&c, &i, &out).is_err());
}
#[test]
fn original_deadline_registry_installation_and_credential_expiry_do_not_retime() {
    for mode in 0..4 {
        let (c, mut i) = fixture();
        match mode {
            0 => i.original_expires_at_ms = i.now_ms,
            1 => i.registry.value.expires_at_ms = i.now_ms,
            2 => i.installation.value.expires_at_ms = i.now_ms,
            _ => {
                for b in &mut i.installation.value.credential_bindings {
                    b.expires_at_ms = i.now_ms;
                }
            }
        }
        repin(&mut i);
        assert!(select_central(&c, &i).unwrap().resolved.is_none());
    }
    let (c, mut i) = fixture();
    i.original_expires_at_ms = i.now_ms + 1;
    assert_eq!(
        select_central(&c, &i)
            .unwrap()
            .resolved
            .unwrap()
            .selection
            .expires_at_ms,
        i.now_ms + 1
    );
}
#[test]
fn accounting_or_monetary_requirements_never_become_zero_or_task_budget() {
    for mode in 0..4 {
        let (c, mut i) = fixture();
        match mode {
            0 => i.policy.value.allow_unknown_cost = false,
            1 => {
                i.policy.value.ceilings.cost_limit_microunits = Some(1);
                i.policy.value.ceilings.currency = Some("USD".into());
            }
            2 => i.policy.value.ceilings.require_hard_cost_enforcement = true,
            _ => i.policy.value.max_output_tokens = 1,
        };
        repin(&mut i);
        let out = select_central(&c, &i).unwrap();
        assert!(out.resolved.is_none());
        assert!(
            out.traces
                .iter()
                .all(|t| t.state == CandidateState::PolicyDenied)
        );
    }
}
#[test]
fn exact_rendered_startup_length_counts_separator_bytes() {
    let (c, mut i) = fixture();
    let startup = i.compositions[0].startup.utf8.len() as u64;
    let raw: u64 = i.compositions[0]
        .composition
        .instruction_order
        .iter()
        .filter(|x| x.delivery == aq_profiles::Delivery::Startup)
        .map(|x| x.text.len() as u64)
        .sum();
    assert!(startup > raw);
    i.policy.value.ceilings.context_bytes = raw;
    repin(&mut i);
    assert!(select_central(&c, &i).unwrap().resolved.is_none());
    i.policy.value.ceilings.context_bytes = startup;
    repin(&mut i);
    assert!(select_central(&c, &i).unwrap().resolved.is_some());
}
#[test]
fn full_environment_pin_not_friendly_name_and_native_reasoning_output_are_required() {
    for mode in 0..3 {
        let (c, mut i) = fixture();
        let h = &mut i.installation.value.harnesses[0];
        match mode {
            0 => h.environment.digest = Digest::of(b"replaced"),
            1 => h.models.clear(),
            _ => h.limits.max_output_tokens = 1,
        };
        repin(&mut i);
        assert!(select_central(&c, &i).unwrap().resolved.is_none());
    }
}
#[test]
fn worker_feedback_and_worker_preparation_cannot_qualify_central() {
    let (c, mut i) = fixture();
    for o in &mut i.registry.value.observations {
        o.scope.node_id = Some(Uuid::from_u128(900));
        o.state = Availability::QuotaUnavailable;
        o.source = ObservationSource::Worker {
            node_id: Uuid::from_u128(900),
            boot_id: Uuid::from_u128(901),
            attempt_id: Uuid::from_u128(902),
            selection_digest: Digest::of(b"worker"),
        };
    }
    repin(&mut i);
    assert!(select_central(&c, &i).unwrap().resolved.is_none());
    let (_, i) = fixture();
    let mut value = serde_json::to_value(Document::Inputs(Box::new(i))).unwrap();
    value["data"]["installation"]["value"]["credential_bindings"][0]["preparation"] = json!({"target":{"kind":"worker","node_id":Uuid::from_u128(903),"boot_id":Uuid::from_u128(904)}});
    assert!(decode(&serde_json::to_vec(&value).unwrap()).is_err());
}
#[test]
fn account_endpoint_revision_scope_cannot_make_foreign_observations_authorize() {
    for mode in 0..4 {
        let (c, mut i) = fixture();
        for o in &mut i.registry.value.observations {
            match mode {
                0 => o.scope.account_reference = "other".into(),
                1 => o.scope.endpoint_reference = "other".into(),
                2 => o.scope.credential_revision += 1,
                _ => o.scope.node_id = Some(Uuid::from_u128(5000)),
            }
        }
        repin(&mut i);
        assert!(select_central(&c, &i).unwrap().resolved.is_none());
    }
}
#[test]
fn closed_decode_rejects_unknown_and_duplicate_fields_without_a_dynamic_map() {
    let (_, i) = fixture();
    let bytes = serde_json::to_vec(&Document::Inputs(Box::new(i))).unwrap();
    assert!(matches!(decode(&bytes).unwrap(), Document::Inputs(_)));
    let text = String::from_utf8(bytes).unwrap();
    assert!(
        decode(
            text.replacen(
                "\"schema_version\":1",
                "\"schema_version\":1,\"schema_version\":1",
                1
            )
            .as_bytes()
        )
        .is_err()
    );
    assert!(
        decode(
            text.replacen(
                "\"schema_version\":1",
                "\"schema_version\":1,\"untrusted\":true",
                1
            )
            .as_bytes()
        )
        .is_err()
    );
    assert!(
        decode(
            text.replacen("\"schema_version\":1", "\"schema_version\":1.5", 1)
                .as_bytes()
        )
        .is_err()
    );
    assert!(decode(br#"{"document":"inputs","document":"inputs","data":{}}"#).is_err());
    let too_big = vec![b' '; MAX_INPUT_BYTES + 1];
    assert!(decode(&too_big).is_err());
}
#[test]
fn actual_schema_and_normative_vectors_are_frozen() {
    let (c, i) = fixture();
    let outcome = select_central(&c, &i).unwrap();
    let mut restricted = i.clone();
    for credential in &mut restricted.registry.value.credentials {
        credential.allowed_backends = vec![BackendKind::DockerLocal];
    }
    repin(&mut restricted);
    let denied = select_central(&c, &restricted).unwrap();
    let documents = json!([
        serde_json::to_value(Document::Inputs(Box::new(i))).unwrap(),
        serde_json::to_value(Document::Outcome(Box::new(outcome))).unwrap(),
        serde_json::to_value(Document::Inputs(Box::new(restricted))).unwrap(),
        serde_json::to_value(Document::Outcome(Box::new(denied))).unwrap()
    ]);
    let schema = schema();
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    for (name, value) in [
        ("central-selection-schema-v1.json", schema),
        ("central-selection-examples-v1.json", documents),
    ] {
        let file = dir.join(name);
        if std::env::var_os("AQ_UPDATE_CENTRAL_SELECTION_SCHEMA").is_some() {
            std::fs::write(
                &file,
                format!("{}\n", serde_json::to_string_pretty(&value).unwrap()),
            )
            .unwrap();
            let public = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../protocol/central-selection/v1")
                .join(if name.contains("schema") {
                    "schema.json"
                } else {
                    "examples.json"
                });
            std::fs::write(
                public,
                format!("{}\n", serde_json::to_string_pretty(&value).unwrap()),
            )
            .unwrap();
        } else {
            let expected: Value = serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
            assert_eq!(value, expected);
        }
    }
}

#[test]
fn actual_intelligence_cost_policies_cannot_be_overridden_by_turn_permission() {
    for mode in 0..5 {
        let (c, mut i) = fixture();
        let original_composition_policy = i.policy.value.composition_policy.clone();
        let original_compatibility = i.policy.value.compatibility.clone();
        let catalog = change_intelligence_policy(&c, &mut i, |p| match mode {
            0 => p.budget.unknown_cost = aq_profiles::UnknownCostPolicy::Deny,
            1 => p.budget.require_complete_accounting = true,
            2 => {
                p.budget.max_cost_microunits = Some(100);
                p.budget.currency = Some("USD".into());
                p.budget.require_hard_cost_enforcement = true;
                p.budget.require_complete_accounting = true;
                p.budget.unknown_cost = aq_profiles::UnknownCostPolicy::Deny;
            }
            3 => {
                p.budget.max_cost_microunits = Some(100);
                p.budget.currency = Some("USD".into());
            }
            _ => p
                .required_capabilities
                .push(aq_session_protocol::Capability {
                    id: Text::new("aq.budget.task-cost").unwrap(),
                    version: 1,
                }),
        });
        assert_ne!(
            original_composition_policy,
            i.policy.value.composition_policy
        );
        assert_ne!(original_compatibility, i.policy.value.compatibility);
        aq_profiles::central::verify_central(&catalog, &i.compositions[0]).unwrap();
        let out = select_central(&catalog, &i).unwrap();
        assert!(out.resolved.is_none(), "policy{mode}");
        assert!(out.traces.iter().all(|t| {
            t.reason_codes
                .iter()
                .any(|r| r == "central_cost_accounting_unavailable")
        }));
    }
}

#[test]
fn actual_required_native_capability_is_not_inferred_from_model_or_qualification() {
    let (c, mut i) = fixture();
    let c = change_intelligence_policy(&c, &mut i, |p| {
        p.required_capabilities
            .push(aq_session_protocol::Capability {
                id: Text::new("aq.harness.transcript").unwrap(),
                version: 1,
            })
    });
    let out = select_central(&c, &i).unwrap();
    assert!(out.resolved.is_none());
    assert!(
        out.traces
            .iter()
            .all(|t| t.state == CandidateState::Unsupported)
    );
    i.installation.value.harnesses[0]
        .capabilities
        .push(aq_session_protocol::Capability {
            id: Text::new("aq.harness.transcript").unwrap(),
            version: 1,
        });
    repin(&mut i);
    assert!(select_central(&c, &i).unwrap().resolved.is_some());
}

#[test]
fn central_selection_cannot_bypass_restricted_unknown_backend() {
    let (catalog, mut input) = fixture();
    for credential in &mut input.registry.value.credentials {
        credential.allowed_backends = vec![BackendKind::DockerLocal];
    }
    repin(&mut input);
    let out = select_central(&catalog, &input).unwrap();
    assert!(out.resolved.is_none());
    assert!(
        out.traces
            .iter()
            .all(|t| t.state == CandidateState::PolicyDenied)
    );
    assert!(
        out.traces[0]
            .reason_codes
            .contains(&"credential_backend_unknown".into())
    );
}
