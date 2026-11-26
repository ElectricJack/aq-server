//! Pure actual compiler/catalog qualification; no human/source owner evidence.
use aq_playbook_host::model::*;
use aq_server::playbooks::{
    domain::{GrantScope, Recipient},
    review_domain::*,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;
fn input() -> DecisionInput {
    let report_id = Uuid::new_v4();
    DecisionInput {
        recipient: Recipient::Task {
            workspace_id: Uuid::new_v4(),
            project_id: Uuid::new_v4(),
            task_id: Uuid::new_v4(),
        },
        report_id,
        source_operation_id: Uuid::new_v4(),
        expected_subject: aq_session_protocol::ArtifactRef {
            id: aq_session_protocol::Text::new(format!("aq.review-subject/{report_id}")).unwrap(),
            version: aq_session_protocol::Text::new("1").unwrap(),
            digest: aq_session_protocol::Digest::of(b"synthetic expected subject"),
        },
        eligible_owners: vec![Uuid::new_v4()],
        eligibility_project_revision: 1,
        expires_at_ms: 100000,
    }
}
fn decision_playbook(id: String, command: &CommandContract) -> Playbook {
    Playbook {
        id,
        purpose: "synthetic finite decision command compilation; not an executed policy".into(),
        scope: "project".into(),
        entries: BTreeMap::from([(
            "main".into(),
            EntryPoint {
                input: Schema::Null,
                outcomes: BTreeMap::from([("done".into(), Schema::Null)]),
                start: "question".into(),
            },
        )]),
        steps: BTreeMap::from([
            (
                "question".into(),
                Step {
                    op: Operation::Invoke {
                        command: ContractRef {
                            name: command.name.clone(),
                            fingerprint: command.fingerprint().unwrap(),
                        },
                        input: Expr::literal(json!(input())),
                        bind: "question".into(),
                        retry: None,
                    },
                    edges: command
                        .outcomes
                        .keys()
                        .map(|key| (key.clone(), "done".into()))
                        .collect(),
                    on_error: Some("done".into()),
                },
            ),
            (
                "done".into(),
                Step {
                    op: Operation::Return {
                        outcome: "done".into(),
                        output: Expr::literal(Value::Null),
                    },
                    edges: BTreeMap::new(),
                    on_error: None,
                },
            ),
        ]),
        triggers: vec![],
        limits: Limits::default(),
        required_capabilities: BTreeSet::from(["decision.request".into()]),
    }
}
fn compile_input(registry: Registry) -> aq_playbooks_compiler::CompileInput {
    let command = decision_contract();
    let header = SourceHeader {
        format_version: 1,
        id: "review-fixture".into(),
        scope: "project".into(),
        interfaces: BTreeSet::from(["main".into()]),
        definitions: BTreeSet::new(),
    };
    aq_playbooks_compiler::CompileInput {
        source: format!(
            "---aq-playbook\n{}\n---\nSynthetic catalog compilation only.\n",
            serde_json::to_string(&header).unwrap()
        ),
        proposal: aq_playbooks_compiler::Proposal {
            playbook: decision_playbook(header.id, &command),
            source_map: BTreeMap::new(),
        },
        registry,
        dependencies: BTreeMap::new(),
        definitions: BTreeMap::new(),
        compiler: aq_playbooks_compiler::COMPILER_VERSION.into(),
    }
}
#[test]
fn catalog1_bytes_and_capabilities_do_not_silently_gain_human_decision_authority() {
    let frozen: Value = serde_json::from_str(include_str!(
        "../../../../protocol/playbook-host/v1/registry.json"
    ))
    .unwrap();
    assert_eq!(json!(CatalogProfile::Mail1.registry()), frozen["registry"]);
    let i = input();
    let (w, p, _) = i.recipient.scope();
    let mut scope = GrantScope {
        workspace_id: w,
        project_id: p,
        recipients: vec![i.recipient],
        capabilities: BTreeSet::from(["message.send".into()]),
    };
    CatalogProfile::Mail1.validate_scope(&scope).unwrap();
    scope.capabilities.insert("decision.request".into());
    assert!(CatalogProfile::Mail1.validate_scope(&scope).is_err());
    CatalogProfile::HumanReview2.validate_scope(&scope).unwrap();
    assert!(
        !CatalogProfile::HumanReview2
            .authority(Uuid::new_v4(), 1, &scope)
            .unwrap()
            .capabilities
            .contains("operation.get")
    );
    scope.capabilities = BTreeSet::from(["graph.read".into()]);
    assert!(CatalogProfile::HumanReview2.validate_scope(&scope).is_err());
}
#[test]
fn actual_compiler_refuses_old_catalog_and_accepts_explicit_registered_async_owner_contract() {
    assert!(
        aq_playbooks_compiler::compile(&compile_input(CatalogProfile::Mail1.registry())).is_err()
    );
    let candidate = compile_input(CatalogProfile::HumanReview2.registry());
    let result = aq_playbooks_compiler::compile(&candidate);
    assert!(result.is_ok(), "{result:?}");
    let mut missing = candidate.clone();
    missing.registry.commands.remove("operation.get");
    assert!(aq_playbooks_compiler::compile(&missing).is_err());
    let mut missing = candidate;
    missing.registry.events.remove("aq.decision.completed");
    assert!(aq_playbooks_compiler::compile(&missing).is_err());
}
#[test]
fn decision_input_refuses_authority_fields_duplicate_owners_and_mismatched_subject_id() {
    let i = input();
    i.validate().unwrap();
    decision_contract().input.validate(&json!(i)).unwrap();
    let mut forged = json!(i);
    forged["owner_user_id"] = json!(Uuid::new_v4());
    assert!(serde_json::from_value::<DecisionInput>(forged.clone()).is_err());
    assert!(decision_contract().input.validate(&forged).is_err());
    let mut bad = i.clone();
    bad.eligible_owners.push(bad.eligible_owners[0]);
    assert!(bad.validate().is_err());
    let mut bad = i;
    bad.report_id = Uuid::new_v4();
    assert!(bad.validate().is_err());
    let accepted = json!({"handle":Uuid::new_v4().to_string()});
    assert!(
        decision_contract().outcomes["accepted"]
            .validate(&accepted)
            .is_ok()
    );
    assert!(
        decision_contract()
            .asynchronous
            .unwrap()
            .completion
            .validate(&accepted)
            .is_err()
    );
}
#[test]
fn catalog2_snapshot_is_generated_and_operation_lookup_has_no_generic_service_or_url() {
    let snapshot: Value = serde_json::from_str(include_str!(
        "../../../../protocol/playbook-host/v2/registry.json"
    ))
    .unwrap();
    assert_eq!(
        snapshot["registry"],
        json!(CatalogProfile::HumanReview2.registry())
    );
    let i = input();
    let good = json!({"kind":"human_decision","handle":Uuid::new_v4().to_string(),"recipient":i.recipient});
    operation_contract().input.validate(&good).unwrap();
    for field in ["url", "service", "actor", "passed"] {
        let mut forged = good.clone();
        forged[field] = json!("arbitrary");
        assert!(operation_contract().input.validate(&forged).is_err());
    }
    let mut wrong = good;
    wrong["kind"] = json!("arbitrary_service");
    assert!(operation_contract().input.validate(&wrong).is_err());
}
