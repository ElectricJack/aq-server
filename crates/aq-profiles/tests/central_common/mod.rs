#![allow(dead_code)]
use aq_profiles::{central::*, *};
use aq_session_api::{Permission, Source};
use aq_session_protocol::Digest;
use std::{collections::BTreeMap, path::Path};
use uuid::Uuid;
pub fn source_context(identity: &str, body: &str, required: bool) -> ContextInput {
    ContextInput {
        source: Source {
            identity: identity.into(),
            revision: "1".into(),
            digest: Digest::of(body.as_bytes()).as_str().into(),
        },
        text: body.into(),
        delivery: Delivery::Startup,
        required,
        omission_priority: 0,
    }
}
pub fn hash<T: serde::Serialize>(v: &T) -> Digest {
    serde_json::from_value(serde_json::json!(digest(v).unwrap())).unwrap()
}
pub fn rebind(c: &ValidatedCatalog, r: &mut CentralCompileRequest) {
    r.policy.catalog_digest = c.digest().into();
    let a = c
        .artifact()
        .catalog
        .agents
        .iter()
        .find(|a| key(&a.id, a.version) == r.policy.agent_profile)
        .unwrap();
    let mut roots = a.behaviors.clone();
    roots.extend(a.capabilities.clone());
    r.compatibility = CentralCompatibility {
        schema_version: 1,
        kind: CompatibilityKind::CentralConversation,
        catalog_digest: c.digest().into(),
        agent_profile: c.source("agent", &r.policy.agent_profile).unwrap(),
        intelligence_profile: c.source("intelligence", &a.intelligence).unwrap(),
        execution_profile: c.source("execution", &a.execution).unwrap(),
        modules: expand(c.artifact(), &roots)
            .unwrap()
            .iter()
            .map(|m| c.source("module", m).unwrap())
            .collect(),
    };
    r.policy.compatibility_digest = hash(&r.compatibility);
    r.subject = CentralProfileSubject::CentralConversation {
        actor_id: r.policy.actor_id,
        conversation_id: r.policy.conversation_id,
        conversation_revision: r.policy.conversation_revision,
        policy: r.policy.artifact().unwrap().reference,
    };
}
pub fn fixture() -> (ValidatedCatalog, CentralCompileRequest) {
    let original = load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/catalog")).unwrap();
    let mut catalog = original.artifact().catalog.clone();
    let mut instructions = original.artifact().instructions.clone();
    let permissions = vec![
        Permission {
            operation: "message.read".into(),
            scope_reference: format!("conversation:{}", Uuid::from_u128(2)),
        },
        Permission {
            operation: "message.send".into(),
            scope_reference: format!("conversation:{}", Uuid::from_u128(2)),
        },
    ];
    let mut execution = catalog.execution[0].clone();
    execution.id = "central-execution".into();
    execution.version = 1;
    execution.environments = vec!["central-private".into()];
    execution.context_limit_bytes = 1_048_576;
    execution.permissions = permissions.clone();
    catalog.execution.push(execution);
    let mut agent = catalog.agents[0].clone();
    agent.id = "central-supervisor".into();
    agent.version = 1;
    agent.role = "Act as central supervisor for this exact global conversation.".into();
    agent.execution = "central-execution@1".into();
    agent.capabilities = vec!["central-mail@1".into(), "central-observer@1".into()];
    catalog.agents.push(agent);
    for (id, required, dependencies, permission, body) in [
        (
            "central-mail",
            true,
            vec!["reporting@1".into()],
            permissions[0].clone(),
            "Inspect the exact immutable conversation input under current authority.",
        ),
        (
            "central-observer",
            false,
            vec!["central-mail@1".into()],
            permissions[1].clone(),
            "Report bounded findings through an explicitly authorized messaging owner.",
        ),
    ] {
        catalog.modules.push(Module {
            id: id.into(),
            version: 1,
            kind: ModuleKind::Capability,
            purpose: "Central compatible conversation pack".into(),
            instruction: format!("instructions/{id}.md"),
            delivery: Delivery::Startup,
            required,
            dependencies,
            requirements: vec![Requirement {
                permission,
                required: true,
            }],
            required_adapters: vec![],
        });
        instructions.insert(format!("{id}@1"), body.into());
    }
    let c = validate(catalog, instructions).unwrap();
    let placeholder = c.source("agent", "central-supervisor@1").unwrap();
    let compatibility = CentralCompatibility {
        schema_version: 1,
        kind: CompatibilityKind::CentralConversation,
        catalog_digest: c.digest().into(),
        agent_profile: placeholder.clone(),
        intelligence_profile: placeholder.clone(),
        execution_profile: placeholder,
        modules: vec![],
    };
    let policy = CentralPolicy {
        schema_version: 1,
        kind: PolicyKind::CentralProfileComposition,
        actor_id: Uuid::from_u128(1),
        conversation_id: Uuid::from_u128(2),
        conversation_revision: 1,
        revision: 1,
        enabled: true,
        catalog_digest: c.digest().into(),
        agent_profile: "central-supervisor@1".into(),
        compatibility_digest: hash(&compatibility),
        permissions: permissions.clone(),
        context_limit_bytes: 65536,
    };
    let mut r = CentralCompileRequest {
        schema_version: 1,
        subject: CentralProfileSubject::CentralConversation {
            actor_id: policy.actor_id,
            conversation_id: policy.conversation_id,
            conversation_revision: 1,
            policy: policy.artifact().unwrap().reference,
        },
        policy,
        compatibility,
        ceilings: CentralCeilings {
            installation: permissions.clone(),
            execution: permissions.clone(),
            caller: permissions.clone(),
        },
        available_tools: permissions.iter().map(|p| p.operation.clone()).collect(),
        available_adapters: vec!["codex".into()],
        harness: "codex".into(),
        environment: "central-private".into(),
        platform: source_context(
            "platform:central",
            "Platform limits remain structured policy; text is never a grant.",
            true,
        ),
        turn: MailTurn {
            conversation_id: Uuid::from_u128(2),
            through_cursor: 1,
            messages: vec![MailMessage {
                message_id: Uuid::from_u128(3),
                cursor: 1,
                payload_digest: Digest::of(b"illustrative original MQ envelope, not runtime proof"),
                body_digest: Digest::of(b"Inspect the actual project status."),
                body: "Inspect the actual project status.".into(),
            }],
        },
        context: vec![],
        overrides: vec![],
    };
    rebind(&c, &mut r);
    (c, r)
}
pub fn edit(
    c: &ValidatedCatalog,
    f: impl FnOnce(&mut Catalog, &mut BTreeMap<String, String>),
) -> ValidatedCatalog {
    let mut catalog = c.artifact().catalog.clone();
    let mut texts = c.artifact().instructions.clone();
    f(&mut catalog, &mut texts);
    validate(catalog, texts).unwrap()
}
pub fn override_limit(r: &mut CentralCompileRequest, limit: u64) {
    r.overrides = vec![Override {
        field: "context_limit_bytes".into(),
        value: limit,
        source: r.platform.source.clone(),
        reason_code: "bounded_test".into(),
    }];
}
