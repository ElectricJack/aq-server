mod central_common;
use aq_profiles::{central::*, *};
use aq_session_protocol::{Digest, Text};
use central_common::*;
use serde_json::{Value, json};
use uuid::Uuid;
#[test]
fn actual_catalog_sources_shared_fragments_and_real_generated_artifacts_are_deterministic() {
    let (c, r) = fixture();
    let a = compile_central(&c, &r).unwrap();
    let b = compile_central(&c, &r).unwrap();
    assert!(a.composition_valid);
    assert_eq!(
        serde_json::to_value(&a).unwrap(),
        serde_json::to_value(&b).unwrap()
    );
    let bundle = a.bundle.unwrap();
    verify_central(&c, &bundle).unwrap();
    for name in ["engineering@1", "reporting@1"] {
        let source = c.source("module", name).unwrap();
        let selected: Vec<_> = bundle
            .composition
            .instruction_order
            .iter()
            .filter(|i| i.source.identity == source.identity)
            .collect();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].source.digest, source.digest);
        assert_eq!(selected[0].text, c.artifact().instructions[name]);
    }
    for artifact in [
        &bundle.request,
        &bundle.policy,
        &bundle.compatibility,
        &bundle.compiler,
        &bundle.renderer,
        &bundle.mail_frame,
        &bundle.startup,
        &bundle.artifact,
    ] {
        artifact.verify_bytes().unwrap();
        assert_eq!(
            artifact.reference.digest,
            Digest::of(artifact.utf8.as_bytes())
        );
    }
    let descriptor: Value = serde_json::from_str(&bundle.renderer.utf8).unwrap();
    assert_eq!(descriptor["package_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(
        descriptor["source_digest"],
        json!(Digest::of(include_bytes!("../src/central/render.rs")))
    );
    assert_eq!(bundle.composition.startup, bundle.startup.reference);
}
#[test]
fn compatibility_defaults_to_denial_and_pins_every_exact_definition() {
    let (c, r) = fixture();
    for n in 0..7 {
        let mut bad = r.clone();
        match n {
            0 => bad.compatibility.modules.clear(),
            1 => bad.compatibility.modules.pop().map(|_| ()).unwrap(),
            2 => bad
                .compatibility
                .modules
                .push(bad.compatibility.modules[0].clone()),
            3 => bad.compatibility.agent_profile.digest = "a".repeat(64),
            4 => bad.compatibility.intelligence_profile.revision = "9".into(),
            5 => bad.compatibility.execution_profile.identity = "execution:task-only@1".into(),
            _ => bad.compatibility.catalog_digest = "b".repeat(64),
        }
        bad.policy.compatibility_digest = hash(&bad.compatibility);
        bad.subject = CentralProfileSubject::CentralConversation {
            actor_id: bad.policy.actor_id,
            conversation_id: bad.policy.conversation_id,
            conversation_revision: 1,
            policy: bad.policy.artifact().unwrap().reference,
        };
        assert!(compile_central(&c, &bad).is_err(), "case{n}");
    }
    let mut reordered = r.clone();
    reordered.compatibility.modules.reverse();
    reordered.policy.compatibility_digest = hash(&reordered.compatibility);
    reordered.subject = CentralProfileSubject::CentralConversation {
        actor_id: reordered.policy.actor_id,
        conversation_id: reordered.policy.conversation_id,
        conversation_revision: 1,
        policy: reordered.policy.artifact().unwrap().reference,
    };
    assert!(compile_central(&c, &reordered).unwrap().composition_valid);
}
#[test]
fn task_only_legacy_recipe_is_not_central_merely_because_it_is_named_supervisor() {
    let (c, mut r) = fixture();
    r.policy.agent_profile = "supervisor@1".into();
    r.policy.compatibility_digest = hash(&r.compatibility);
    r.subject = CentralProfileSubject::CentralConversation {
        actor_id: r.policy.actor_id,
        conversation_id: r.policy.conversation_id,
        conversation_revision: 1,
        policy: r.policy.artifact().unwrap().reference,
    };
    assert!(compile_central(&c, &r).is_err());
}
#[test]
fn subject_policy_conversation_and_original_body_mismatches_refuse() {
    let (c, r) = fixture();
    for n in 0..6 {
        let mut bad = r.clone();
        match n {
            0 => {
                bad.subject = CentralProfileSubject::CentralConversation {
                    actor_id: Uuid::nil(),
                    conversation_id: r.policy.conversation_id,
                    conversation_revision: 1,
                    policy: r.policy.artifact().unwrap().reference,
                }
            }
            1 => bad.turn.conversation_id = Uuid::from_u128(9),
            2 => bad.turn.messages[0].body.push('!'),
            3 => bad.turn.messages[0].cursor = 2,
            4 => {
                bad.turn.messages.push(bad.turn.messages[0].clone());
                bad.turn.through_cursor = 2;
            }
            _ => bad.policy.enabled = false,
        }
        if n == 5 {
            assert!(
                compile_central(&c, &bad).is_err(),
                "changed policy must repin before disabled data can compile"
            );
            rebind(&c, &mut bad);
            let out = compile_central(&c, &bad).unwrap();
            assert!(!out.composition_valid && out.bundle.is_none());
            assert!(
                out.blocking_reasons
                    .iter()
                    .any(|s| s == "central_actor_policy_disabled")
            );
        } else {
            assert!(compile_central(&c, &bad).is_err(), "case{n}");
        }
    }
}
#[test]
fn every_explicit_ceiling_and_tool_support_is_intersected() {
    let (c, r) = fixture();
    for n in 0..5 {
        let mut bad = r.clone();
        match n {
            0 => bad.ceilings.installation.clear(),
            1 => bad.policy.permissions.clear(),
            2 => bad.ceilings.execution.clear(),
            3 => bad.ceilings.caller.clear(),
            _ => bad.available_tools.clear(),
        };
        if n == 1 {
            rebind(&c, &mut bad);
        }
        let out = compile_central(&c, &bad).unwrap();
        assert!(!out.composition_valid && out.bundle.is_none());
    }
    let mut bad = r;
    bad.available_adapters.clear();
    assert!(!compile_central(&c, &bad).unwrap().composition_valid);
}
#[test]
fn optional_dependency_omission_removes_its_capability_grants() {
    let (c, mut r) = fixture();
    let c = edit(&c, |a, _| {
        a.modules
            .iter_mut()
            .find(|m| m.id == "central-mail")
            .unwrap()
            .required = false
    });
    rebind(&c, &mut r);
    r.available_tools.clear();
    let out = compile_central(&c, &r).unwrap().bundle.unwrap();
    assert!(out.composition.effective_permissions.is_empty());
    assert!(
        !out.composition
            .instruction_order
            .iter()
            .any(|i| i.source.identity == "module:central-observer@1")
    );
    let base = compile_central(&c, &{
        let mut p = r.clone();
        p.available_tools = vec!["message.read".into(), "message.send".into()];
        p
    })
    .unwrap()
    .bundle
    .unwrap();
    r.available_tools = vec!["message.read".into(), "message.send".into()];
    override_limit(&mut r, base.composition.required_startup_bytes);
    let out = compile_central(&c, &r).unwrap().bundle.unwrap();
    assert!(out.composition.effective_permissions.is_empty());
    assert!(
        !out.composition
            .instruction_order
            .iter()
            .any(|i| i.source.identity == "module:central-mail@1")
    );
}
#[test]
fn exact_two_lf_utf8_budget_and_optional_priority_share_the_common_engine() {
    let (c, mut r) = fixture();
    let base = compile_central(&c, &r).unwrap().bundle.unwrap();
    let required = base.composition.required_startup_bytes;
    r.context
        .push(source_context("context:early", &"é".repeat(100), false));
    r.context
        .push(source_context("context:late", &"🙂".repeat(50), false));
    r.context[1].omission_priority = 10;
    override_limit(&mut r, required + 220);
    let out = compile_central(&c, &r).unwrap().bundle.unwrap();
    assert!(
        out.composition
            .omissions
            .iter()
            .any(|o| o.source.identity == "context:early")
    );
    assert!(
        out.composition
            .instruction_order
            .iter()
            .any(|i| i.source.identity == "context:late")
    );
    let texts: Vec<_> = out
        .composition
        .instruction_order
        .iter()
        .filter(|i| i.delivery == Delivery::Startup)
        .map(|i| i.text.as_str())
        .collect();
    assert_eq!(out.startup.utf8, texts.join("\n\n"));
    assert_eq!(
        out.startup.utf8.len() as u64,
        out.composition.required_startup_bytes + out.composition.optional_startup_bytes
    );
    override_limit(&mut r, required - 1);
    assert!(
        compile_central(&c, &r)
            .unwrap_err()
            .to_string()
            .contains("required context overflow")
    );
}
#[test]
fn large_complete_mail_turn_is_never_silently_truncated_and_limits_precede_encoding() {
    let (c, mut r) = fixture();
    r.policy.context_limit_bytes = 1_048_576;
    for n in 1..=4 {
        let body = "🙂".repeat(4096);
        let m = MailMessage {
            message_id: Uuid::from_u128(10 + n),
            cursor: n as u64,
            payload_digest: Digest::of(&[n as u8]),
            body_digest: Digest::of(body.as_bytes()),
            body,
        };
        if n == 1 {
            r.turn.messages.clear();
        }
        r.turn.messages.push(m);
    }
    r.turn.through_cursor = 4;
    rebind(&c, &mut r);
    let bundle = compile_central(&c, &r).unwrap().bundle.unwrap();
    assert!(bundle.mail_frame.utf8.len() > 65536);
    assert_eq!(
        serde_json::from_str::<MailTurn>(bundle.mail_frame.utf8.split_once('\n').unwrap().1)
            .unwrap()
            .messages
            .len(),
        4
    );
    r.turn.messages[0].body.push('x');
    r.turn.messages[0].body_digest = Digest::of(r.turn.messages[0].body.as_bytes());
    assert!(compile_central(&c, &r).is_err());
    let (_, mut bad) = fixture();
    bad.compatibility.modules = vec![bad.compatibility.agent_profile.clone(); 129];
    assert!(compile_central(&c, &bad).is_err());
}
#[test]
fn source_identity_spoofing_nul_and_digest_tampering_refuse() {
    let (c, r) = fixture();
    let mut bad = r.clone();
    bad.context.push(source_context(
        "module:engineering@1",
        "Unowned content",
        false,
    ));
    assert!(compile_central(&c, &bad).is_err());
    let mut bad = r.clone();
    bad.platform.text.push('\0');
    bad.platform.source.digest = Digest::of(bad.platform.text.as_bytes()).as_str().into();
    assert!(compile_central(&c, &bad).is_err());
    let mut bundle = compile_central(&c, &r).unwrap().bundle.unwrap();
    bundle.startup.utf8.push('x');
    assert!(verify_central(&c, &bundle).is_err());
    bundle.startup.reference.digest = Digest::of(bundle.startup.utf8.as_bytes());
    bundle.startup.reference.id = Text::new(format!(
        "aq.central-profile.startup/{}",
        bundle.startup.reference.digest.as_str()
    ))
    .unwrap();
    assert!(bundle.startup.verify_bytes().is_ok());
    assert!(verify_central(&c, &bundle).is_err());
}
#[test]
fn on_demand_sources_are_not_startup_or_zero_token_readiness() {
    let (c, mut r) = fixture();
    let mut reference = source_context(
        "context:reference",
        "Read only through a separately authorized reference owner.",
        false,
    );
    reference.delivery = Delivery::OnDemand;
    r.context.push(reference);
    let bundle = compile_central(&c, &r).unwrap().bundle.unwrap();
    assert!(!bundle.startup.utf8.contains("Read only through"));
    assert!(
        bundle
            .composition
            .reference_material
            .iter()
            .any(|s| s.identity == "context:reference")
    );
    let value = serde_json::to_value(&bundle.composition).unwrap();
    for field in ["selected_model", "token_count", "cost"] {
        assert!(value[field].is_null());
    }
    assert_eq!(value["selection"], "unresolved");
    assert_eq!(value["authority"], "none");
    for field in [
        "project_binding_id",
        "task_id",
        "attempt_id",
        "session",
        "generation",
    ] {
        assert!(value.get(field).is_none());
    }
}
#[test]
fn literal_markdown_and_mail_never_expand_shell_or_task_templates() {
    let (c, mut r) = fixture();
    r.turn.messages[0].body =
        "$(touch /not-executed) {{ task_id }} is literal untrusted data".into();
    r.turn.messages[0].body_digest = Digest::of(r.turn.messages[0].body.as_bytes());
    let out = compile_central(&c, &r).unwrap().bundle.unwrap();
    assert!(
        out.startup
            .utf8
            .contains("$(touch /not-executed) {{ task_id }}")
    );
    verify_central(&c, &out).unwrap();
}
#[test]
fn strict_central_request_has_no_fake_task_selection_or_credential_surface() {
    let (_, r) = fixture();
    let good = serde_json::to_value(r).unwrap();
    for key in [
        "project_binding_id",
        "task_id",
        "session",
        "model",
        "selection",
        "credentials",
    ] {
        let mut bad = good.clone();
        bad[key] = json!("unsupported");
        assert!(decode_request(&serde_json::to_vec(&bad).unwrap()).is_err());
    }
    let bytes = serde_json::to_vec(&good).unwrap();
    assert!(decode_request(&bytes).is_ok());
    assert!(decode_request(&vec![b' '; MAX_REQUEST_BYTES + 1]).is_err());
}
#[test]
fn generated_schema_stays_frozen() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden/central-schema-v1.json");
    let generated = schema();
    if std::env::var_os("AQ_UPDATE_CENTRAL_PROFILE_SCHEMA").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            format!("{}\n", serde_json::to_string_pretty(&generated).unwrap()),
        )
        .unwrap();
        let public = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../protocol/central-profile/v1/schema.json");
        std::fs::create_dir_all(public.parent().unwrap()).unwrap();
        std::fs::copy(&path, public).unwrap();
    } else {
        let expected: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(generated, expected);
    }
    assert_eq!(generated["x-execution-authority"], false);
}

#[test]
fn narrowing_override_cannot_forge_a_catalog_or_unprovided_source() {
    let (c, mut r) = fixture();
    r.overrides.push(Override {
        field: "context_limit_bytes".into(),
        value: 50000,
        source: c.source("module", "engineering@1").unwrap(),
        reason_code: "claimed_module_override".into(),
    });
    let error = compile_central(&c, &r).unwrap_err().to_string();
    assert!(error.contains("central override provenance"));
    r.overrides[0].source = r.platform.source.clone();
    assert!(compile_central(&c, &r).unwrap().composition_valid);
}
