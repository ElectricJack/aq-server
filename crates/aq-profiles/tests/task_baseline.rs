//! Captured against untouched compiler0.1.1 before central-core refactoring.
mod common;
use aq_profiles::*;
use common::*;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use uuid::Uuid;
fn outcome<T: Serialize>(result: Result<T>) -> Value {
    match result {
        Ok(value) => json!({"ok":value}),
        Err(error) => json!({"error":error.to_string()}),
    }
}
fn record(
    cases: &mut BTreeMap<String, Value>,
    name: &str,
    c: &ValidatedCatalog,
    r: &CompileRequest,
) {
    cases.insert(name.into(),json!({"input_digest":digest(&(c.artifact(),r,access())).unwrap(),"legacy":outcome(compile(c,r,&access())),"draft":outcome(compile_draft(c,r))}));
}
#[test]
fn task_wire_and_errors_match_precentral_011_capture() {
    let c = seed();
    let mut cases = BTreeMap::new();
    record(&mut cases, "base", &c, &request(&c));
    for role in ["reviewer", "researcher", "supervisor"] {
        let mut r = request(&c);
        r.profile.agent_profile = format!("{role}@1");
        record(&mut cases, &format!("role_{role}"), &c, &r);
    }
    for n in 0..5 {
        let mut r = request(&c);
        match n {
            0 => r.ceilings.installation.clear(),
            1 => r.ceilings.project.clear(),
            2 => r.ceilings.execution.clear(),
            3 => r.ceilings.caller.clear(),
            _ => r.available_tools.clear(),
        };
        record(&mut cases, &format!("denied_ceiling_{n}"), &c, &r);
    }
    let mut r = request(&c);
    r.available_adapters.clear();
    record(&mut cases, "missing_adapters", &c, &r);
    let mut r = request(&c);
    r.profile.catalog_digest = "b".repeat(64);
    record(&mut cases, "wrong_catalog", &c, &r);
    let mut r = request(&c);
    r.profile.project_binding_id = Uuid::nil();
    record(&mut cases, "nil_task_binding", &c, &r);
    let mut r = request(&c);
    r.assignment.source.digest = "b".repeat(64);
    record(&mut cases, "changed_context", &c, &r);
    for (name, value, field) in [
        ("narrowed", 4096, "context_limit_bytes"),
        ("required_overflow", 1, "context_limit_bytes"),
        ("protected_override", 1, "permissions"),
    ] {
        let mut r = request(&c);
        r.overrides.push(Override {
            field: field.into(),
            value,
            source: r.project.source.clone(),
            reason_code: "capture".into(),
        });
        record(&mut cases, name, &c, &r);
    }
    let mut r = request(&c);
    let required = compile(&c, &r, &access())
        .unwrap()
        .explanation
        .required_bytes;
    r.context
        .push(context("context:early", &"a".repeat(200), false));
    r.context
        .push(context("context:late", &"b".repeat(200), false));
    r.context[1].omission_priority = 10;
    r.overrides.push(Override {
        field: "context_limit_bytes".into(),
        value: required + 250,
        source: r.project.source.clone(),
        reason_code: "capture".into(),
    });
    record(&mut cases, "optional_budget_order", &c, &r);
    let optional = modified(&c, |a, _| {
        a.modules[0].required = false;
        a.modules[1].required = false;
        a.modules[2].required = false;
        a.agents[0].behaviors.clear();
        a.agents[0].capabilities = vec!["aq-work@1".into()];
    })
    .unwrap();
    let mut r = request(&optional);
    let required = compile(&optional, &r, &access())
        .unwrap()
        .explanation
        .required_bytes;
    r.overrides.push(Override {
        field: "context_limit_bytes".into(),
        value: required + 1,
        source: r.project.source.clone(),
        reason_code: "capture".into(),
    });
    record(&mut cases, "optional_dependency_budget", &optional, &r);
    let missing = modified(&optional, |a, _| {
        a.modules[0]
            .required_adapters
            .push("missing-adapter".into())
    })
    .unwrap();
    record(
        &mut cases,
        "optional_dependency_missing",
        &missing,
        &request(&missing),
    );
    let native = modified(&c, |a, _| {
        for e in &mut a.execution {
            for p in &mut e.permissions {
                p.scope_reference = if p.operation == "source.write" {
                    format!("repository:{}", Uuid::from_u128(2))
                } else {
                    format!("project:{}", Uuid::from_u128(1))
                };
            }
        }
        for m in &mut a.modules {
            for r in &mut m.requirements {
                r.permission.scope_reference = if r.permission.operation == "source.write" {
                    format!("repository:{}", Uuid::from_u128(2))
                } else {
                    format!("project:{}", Uuid::from_u128(1))
                };
            }
        }
    })
    .unwrap();
    let r = request(&native);
    record(&mut cases, "native_input", &native, &r);
    let ctx = json!({"observed_at":"1970-01-01T00:00:01Z","projection":"authorized_scope","root_ids":[Uuid::from_u128(3)],"nodes":[{"id":Uuid::from_u128(3),"kind":"task","version":"1","content_revision":"1","title":"Normative task","body":"Bounded work","acceptance_criteria":"Verify","labels":[],"task_state":"active","verification":null,"content_type":null,"retired":false,"deleted":false}],"edges":[],"pinned_revisions":[],"coverage":{"status":"complete","complete_for":"authorized_projection","reasons":[]}});
    let draft = compile_draft(&native, &r).unwrap().composition.unwrap();
    let sealed = draft
        .seal(
            GraphAccessScope {
                repository_id: Uuid::from_u128(2),
                workspace_id: Uuid::from_u128(4),
                task_id: Uuid::from_u128(3),
                record_ids: vec![Uuid::from_u128(3)],
                edge_types: vec![],
            },
            &ctx,
        )
        .unwrap();
    cases.insert(
        "native_sealed".into(),
        json!({"composition":sealed.composition,"graph_policy":sealed.graph_access_policy}),
    );
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden/task-compatibility-0.1.1.json");
    if std::env::var_os("AQ_CAPTURE_TASK_011").is_some() {
        assert_eq!(
            env!("CARGO_PKG_VERSION"),
            "0.1.1",
            "capture must precede version bump/refactor"
        );
        std::fs::write(&path,format!("{}\n",serde_json::to_string_pretty(&json!({"captured_compiler":"aq-profiles0.1.1","compiler_source_digest":aq_session_protocol::Digest::of(include_bytes!("../src/compile.rs")).as_str(),"cases":cases})).unwrap())).unwrap();
    } else {
        let expected: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(
            json!(cases),
            expected["cases"],
            "task wire/error corpus changed"
        );
    }
}
