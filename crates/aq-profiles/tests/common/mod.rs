#![allow(dead_code)]
use aq_profiles::*;
use aq_session_api::{ProfileRequest, Source};
use std::path::PathBuf;
pub fn seed() -> ValidatedCatalog {
    load(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/catalog")).unwrap()
}
pub fn context(id: &str, body: &str, required: bool) -> ContextInput {
    ContextInput {
        source: Source {
            identity: id.into(),
            digest: aq_session_protocol::Digest::of(body.as_bytes())
                .as_str()
                .into(),
            revision: "1".into(),
        },
        text: body.into(),
        delivery: Delivery::Startup,
        required,
        omission_priority: 0,
    }
}
pub fn request(c: &ValidatedCatalog) -> CompileRequest {
    let permissions = c.artifact().catalog.execution[0].permissions.clone();
    CompileRequest {
        profile: ProfileRequest {
            catalog_digest: c.digest().into(),
            agent_profile: "implementer@1".into(),
            project_binding_id: uuid::Uuid::from_u128(1),
            expected_project_revision: 1,
            task_context_revision: "task:1".into(),
        },
        ceilings: Ceilings {
            installation: permissions.clone(),
            project: permissions.clone(),
            execution: permissions.clone(),
            caller: permissions.clone(),
        },
        available_tools: permissions.iter().map(|p| p.operation.clone()).collect(),
        available_adapters: vec![
            "codex".into(),
            "claude-code".into(),
            "gemini".into(),
            "opencode".into(),
        ],
        harness: "codex".into(),
        environment: "confined-worktree".into(),
        platform: context("platform:1", "Platform requirements.\n", true),
        project: context("project:1", "Project conventions.\n", true),
        assignment: context("assignment:1", "Exact assigned outcome.\n", true),
        context: vec![],
        overrides: vec![],
        intelligence_override: None,
    }
}
pub fn access() -> String {
    "a".repeat(64)
}
pub fn modified(
    c: &ValidatedCatalog,
    edit: impl FnOnce(&mut Catalog, &mut std::collections::BTreeMap<String, String>),
) -> Result<ValidatedCatalog> {
    let mut catalog = c.artifact().catalog.clone();
    let mut instructions = c.artifact().instructions.clone();
    edit(&mut catalog, &mut instructions);
    validate(catalog, instructions)
}
