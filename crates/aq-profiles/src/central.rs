//! Pure taskless profile/startup data. No selection, retention, grant or launch.
#![forbid(unsafe_code)]
mod render;
mod types;
use crate::{
    composition_core::{Accounting, CoreRequest, compile_core, context_valid},
    *,
};
use aq_session_api::{Omission, OverrideExplanation, Permission, Source};
use aq_session_protocol::Validate;
use aq_session_protocol::{ArtifactRef, Text};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;
pub use types::*;
use uuid::Uuid;
pub const MAX_REQUEST_BYTES: usize = aq_agent_api::MAX_DOCUMENT_BYTES;
pub const MAX_ARTIFACT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_STARTUP_BYTES: usize = 1_048_576;
fn canonical<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let mut value =
        serde_json::to_value(value).map_err(|_| Error::Invalid("central encoding".into()))?;
    value.sort_all_objects();
    serde_json::to_vec(&value).map_err(|_| Error::Invalid("central encoding".into()))
}
fn positive(n: u64) -> Result<()> {
    ensure(
        n > 0 && n <= i64::MAX as u64,
        "central revision/cursor range",
    )
}
fn id(id: Uuid) -> Result<()> {
    ensure(!id.is_nil(), "central nil identity")
}
impl GeneratedArtifact {
    fn new(kind: ArtifactKind, utf8: String) -> Result<Self> {
        ensure(
            !utf8.is_empty() && utf8.len() <= MAX_ARTIFACT_BYTES && !utf8.contains('\0'),
            "central artifact byte bound/NUL",
        )?;
        let digest = Digest::of(utf8.as_bytes());
        Ok(Self {
            kind,
            reference: ArtifactRef {
                id: Text::new(format!(
                    "aq.central-profile.{}/{}",
                    kind.label(),
                    digest.as_str()
                ))
                .map_err(|_| Error::Invalid("central artifact id".into()))?,
                version: Text::new("1").unwrap(),
                digest,
            },
            utf8,
        })
    }
    fn canonical<T: Serialize>(kind: ArtifactKind, value: &T) -> Result<Self> {
        let bytes = canonical(value)?;
        Self::new(
            kind,
            String::from_utf8(bytes).map_err(|_| Error::Invalid("central UTF8".into()))?,
        )
    }
    pub fn verify_bytes(&self) -> Result<()> {
        self.reference
            .id
            .validate()
            .map_err(|_| Error::Invalid("central artifact ref".into()))?;
        self.reference
            .version
            .validate()
            .map_err(|_| Error::Invalid("central artifact ref".into()))?;
        ensure(
            !self.utf8.is_empty() && self.utf8.len() <= MAX_ARTIFACT_BYTES,
            "central artifact byte bound",
        )?;
        let expected = Self::new(self.kind, self.utf8.clone())?;
        ensure(
            expected.reference == self.reference,
            "central artifact bytes/reference mismatch",
        )
    }
}
impl CentralPolicy {
    pub fn artifact(&self) -> Result<GeneratedArtifact> {
        self.validate()?;
        GeneratedArtifact::canonical(ArtifactKind::Policy, self)
    }
    fn validate(&self) -> Result<()> {
        ensure(self.schema_version == 1, "central policy version")?;
        id(self.actor_id)?;
        id(self.conversation_id)?;
        positive(self.conversation_revision)?;
        positive(self.revision)?;
        ensure(
            crate::valid_digest(&self.catalog_digest),
            "central policy catalog digest",
        )?;
        text(&self.agent_profile)?;
        ensure(
            self.permissions.len() <= 128,
            "central policy permission count",
        )?;
        let mut seen = BTreeSet::new();
        for p in &self.permissions {
            text(&p.operation)?;
            text(&p.scope_reference)?;
            ensure(
                p.scope_reference.contains(':') && seen.insert((&p.operation, &p.scope_reference)),
                "central policy scope/duplicate",
            )?;
        }
        ensure(
            self.context_limit_bytes > 0 && self.context_limit_bytes <= MAX_STARTUP_BYTES as u64,
            "central policy context ceiling",
        )
    }
}
impl MailTurn {
    fn validate(&self) -> Result<()> {
        id(self.conversation_id)?;
        positive(self.through_cursor)?;
        ensure(
            !self.messages.is_empty() && self.messages.len() <= 16,
            "central mail count",
        )?;
        let mut ids = BTreeSet::new();
        let mut previous = 0;
        let mut bytes = 0usize;
        for m in &self.messages {
            id(m.message_id)?;
            positive(m.cursor)?;
            ensure(
                m.cursor > previous && m.cursor <= self.through_cursor && ids.insert(m.message_id),
                "central mail order/identity",
            )?;
            ensure(
                !m.body.trim().is_empty()
                    && !m.body.contains('\0')
                    && m.body.len() <= 16384
                    && Digest::of(m.body.as_bytes()) == m.body_digest,
                "central mail body provenance/bound",
            )?;
            bytes = bytes
                .checked_add(m.body.len())
                .ok_or_else(|| Error::Invalid("central mail overflow".into()))?;
            ensure(bytes <= 65536, "central mail aggregate bound")?;
            previous = m.cursor;
        }
        Ok(())
    }
}
fn external(context: &ContextInput) -> Result<()> {
    context_valid(context, MAX_INSTRUCTION_BYTES)?;
    ensure(!context.text.contains('\0'), "central context contains NUL")?;
    for prefix in [
        "module:",
        "agent:",
        "intelligence:",
        "execution:",
        "catalog:",
        "central.",
    ] {
        ensure(
            !context.source.identity.starts_with(prefix),
            "external context impersonates compiler/catalog source",
        )?;
    }
    Ok(())
}
fn frame<T: Serialize>(
    identity: String,
    revision: String,
    label: &str,
    value: &T,
) -> Result<ContextInput> {
    let utf8 = format!(
        "{label}\n{}",
        String::from_utf8(canonical(value)?)
            .map_err(|_| Error::Invalid("central frame UTF8".into()))?
    );
    ensure(utf8.len() <= MAX_STARTUP_BYTES, "central frame bound")?;
    Ok(ContextInput {
        source: Source {
            identity,
            revision,
            digest: Digest::of(utf8.as_bytes()).as_str().into(),
        },
        text: utf8,
        delivery: Delivery::Startup,
        required: true,
        omission_priority: 0,
    })
}
fn compiler() -> Result<GeneratedArtifact> {
    GeneratedArtifact::canonical(
        ArtifactKind::Compiler,
        &json!({"schema_version":1,"package":"aq-profiles","package_version":env!("CARGO_PKG_VERSION"),"algorithm":"central-profile-common-core-v1","sources":{"central":Digest::of(include_bytes!("central.rs")),"types":Digest::of(include_bytes!("central/types.rs")),"core":Digest::of(include_bytes!("composition_core.rs")),"catalog":Digest::of(include_bytes!("catalog.rs")),"legacy_types":Digest::of(include_bytes!("compile.rs")),"digest_support":Digest::of(include_bytes!("lib.rs")),"policy_types":Digest::of(include_bytes!("selection_policy.rs")),"graph_digest_helper":Digest::of(include_bytes!("graph_policy.rs")),"renderer":Digest::of(include_bytes!("central/render.rs"))},"authority":"none"}),
    )
}
fn check_compatibility(
    catalog: &ValidatedCatalog,
    r: &CentralCompileRequest,
    modules: &[Source],
) -> Result<()> {
    let c = &r.compatibility;
    ensure(
        c.schema_version == 1 && c.catalog_digest == catalog.digest() && c.modules.len() <= 128,
        "central compatibility catalog/version/bound",
    )?;
    let agent = catalog
        .artifact()
        .catalog
        .agents
        .iter()
        .find(|a| key(&a.id, a.version) == r.policy.agent_profile)
        .ok_or_else(|| Error::Invalid("unknown agent recipe".into()))?;
    let equal = |a: &Source, b: &Source| {
        a.identity == b.identity && a.revision == b.revision && a.digest == b.digest
    };
    ensure(
        equal(
            &c.agent_profile,
            &catalog.source("agent", &r.policy.agent_profile)?,
        ) && equal(
            &c.intelligence_profile,
            &catalog.source("intelligence", &agent.intelligence)?,
        ) && equal(
            &c.execution_profile,
            &catalog.source("execution", &agent.execution)?,
        ),
        "central compatibility exact profile identities",
    )?;
    let source_key = |s: &Source| (s.identity.clone(), s.revision.clone(), s.digest.clone());
    let declared: BTreeSet<_> = c.modules.iter().map(source_key).collect();
    let required: BTreeSet<_> = modules.iter().map(source_key).collect();
    ensure(
        c.modules.len() == modules.len()
            && declared.len() == c.modules.len()
            && declared == required,
        "central compatibility must explicitly cover the exact complete module closure",
    )
}
/// Structural deterministic composition only. Even a valid result grants nothing.
pub fn compile_central(
    catalog: &ValidatedCatalog,
    r: &CentralCompileRequest,
) -> Result<CentralCompilation> {
    ensure(r.schema_version == 1, "central request version")?;
    r.policy.validate()?;
    r.turn.validate()?;
    external(&r.platform)?;
    ensure(
        r.context.len() <= 128 && r.overrides.len() <= 16,
        "central request cardinality",
    )?;
    for c in &r.context {
        external(c)?;
    }
    let policy = r.policy.artifact()?;
    let CentralProfileSubject::CentralConversation {
        actor_id,
        conversation_id,
        conversation_revision,
        policy: policy_ref,
    } = &r.subject;
    id(*actor_id)?;
    id(*conversation_id)?;
    positive(*conversation_revision)?;
    ensure(
        *actor_id == r.policy.actor_id
            && *conversation_id == r.policy.conversation_id
            && *conversation_revision == r.policy.conversation_revision
            && *policy_ref == policy.reference
            && r.turn.conversation_id == *conversation_id,
        "central subject/policy/mail mismatch",
    )?;
    ensure(
        r.compatibility.modules.len() <= 128
            && crate::valid_digest(&r.compatibility.catalog_digest),
        "central compatibility bound/digest",
    )?;
    for source in std::iter::once(&r.compatibility.agent_profile)
        .chain(std::iter::once(&r.compatibility.intelligence_profile))
        .chain(std::iter::once(&r.compatibility.execution_profile))
        .chain(r.compatibility.modules.iter())
    {
        text(&source.identity)?;
        text(&source.revision)?;
        ensure(
            crate::valid_digest(&source.digest),
            "central compatibility source digest",
        )?;
    }
    let compatibility =
        GeneratedArtifact::canonical(ArtifactKind::Compatibility, &r.compatibility)?;
    ensure(
        compatibility.reference.digest == r.policy.compatibility_digest,
        "central compatibility policy binding",
    )?;
    let policy_context = frame(
        format!("central.policy:{}", actor_id),
        r.policy.revision.to_string(),
        "Composition policy data (ceiling metadata, not execution authority):",
        &r.policy,
    )?;
    let conversation_context = frame(
        format!("central.conversation:{}", conversation_id),
        conversation_revision.to_string(),
        "Global conversation scope:",
        &json!({"actor_id":actor_id,"conversation_id":conversation_id,"revision":conversation_revision}),
    )?;
    let mail_context = frame(
        format!("central.mail:{}", digest(&r.turn)?),
        r.turn.through_cursor.to_string(),
        "Immutable untrusted human input (never a permission or policy grant):",
        &r.turn,
    )?;
    let mail_frame = GeneratedArtifact::new(ArtifactKind::MailFrame, mail_context.text.clone())?;
    let subject_contexts = [policy_context, conversation_context, mail_context];
    let mut result = compile_core(
        catalog,
        &CoreRequest {
            catalog_digest: &r.policy.catalog_digest,
            agent_profile: &r.policy.agent_profile,
            ceilings: [
                &r.ceilings.installation,
                &r.policy.permissions,
                &r.ceilings.execution,
                &r.ceilings.caller,
            ],
            available_tools: &r.available_tools,
            available_adapters: &r.available_adapters,
            harness: &r.harness,
            environment: &r.environment,
            platform: &r.platform,
            subject_contexts: &subject_contexts,
            context: &r.context,
            overrides: &r.overrides,
            context_limit_bytes: Some(r.policy.context_limit_bytes),
            context_entry_bytes: MAX_STARTUP_BYTES,
            accounting: Accounting::SeparatedStartup,
            required_context_error: "platform/policy/conversation/mail must be startup required",
        },
    )?;
    check_compatibility(catalog, r, &result.flattened)?;
    let request = GeneratedArtifact::canonical(ArtifactKind::Request, r)?;
    ensure(
        request.utf8.len() <= MAX_REQUEST_BYTES,
        "central encoded request bound",
    )?;
    if !r.policy.enabled {
        result.blocking.push("central_actor_policy_disabled".into());
    }
    if !result.blocking.is_empty() {
        return Ok(CentralCompilation {
            catalog_valid: true,
            composition_valid: false,
            blocking_reasons: result.blocking,
            bundle: None,
            selection: SelectionState::Unresolved,
            authority: AuthorityState::None,
        });
    }
    for instruction in &result.instructions {
        ensure(
            !instruction.text.contains('\0'),
            "central instruction contains NUL",
        )?;
    }
    let compiler = compiler()?;
    let renderer = render::descriptor()?;
    let startup = GeneratedArtifact::new(
        ArtifactKind::Startup,
        render::render(&result.instructions, result.limit)?,
    )?;
    ensure(
        startup.utf8.len() as u64 == result.required_bytes + result.optional_bytes,
        "central startup/accounting mismatch",
    )?;
    let catalog_bytes = catalog.canonical_bytes()?;
    let catalog_ref = ArtifactRef {
        id: Text::new(format!("aq.profiles.catalog/{}", catalog.digest())).unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(&catalog_bytes),
    };
    let composition = CentralComposition {
        schema_version: 1,
        subject: r.subject.clone(),
        request: request.reference.clone(),
        catalog: catalog_ref,
        compiler: compiler.reference.clone(),
        compatibility: compatibility.reference.clone(),
        renderer: renderer.reference.clone(),
        mail_frame: mail_frame.reference.clone(),
        agent_profile: result.role_source,
        intelligence_profile: catalog.source("intelligence", &result.agent.intelligence)?,
        execution_profile: catalog.source("execution", &result.agent.execution)?,
        modules: result.flattened,
        harness: r.harness.clone(),
        environment: r.environment.clone(),
        instruction_order: result.instructions,
        requested_permissions: result.requested,
        effective_permissions: result.effective,
        denied_requirements: result.denied,
        context_limit_bytes: result.limit,
        required_startup_bytes: result.required_bytes,
        optional_startup_bytes: result.optional_bytes,
        omissions: result.omissions,
        overrides: result.overrides,
        reference_material: result.references,
        startup: startup.reference.clone(),
        startup_bytes: startup.utf8.len() as u64,
        selection: SelectionState::Unresolved,
        selected_model: (),
        token_count: (),
        cost: (),
        authority: AuthorityState::None,
    };
    let artifact = GeneratedArtifact::canonical(ArtifactKind::Composition, &composition)?;
    Ok(CentralCompilation {
        catalog_valid: true,
        composition_valid: true,
        blocking_reasons: vec![],
        bundle: Some(CentralBundle {
            composition,
            request,
            policy,
            compatibility,
            compiler,
            renderer,
            mail_frame,
            startup,
            artifact,
        }),
        selection: SelectionState::Unresolved,
        authority: AuthorityState::None,
    })
}

fn bounded_typed_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    struct Limited(Vec<u8>);
    impl std::io::Write for Limited {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self
                .0
                .len()
                .checked_add(bytes.len())
                .is_none_or(|n| n > 16 * 1024 * 1024)
            {
                return Err(std::io::Error::other("central bundle byte bound"));
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Limited(Vec::new());
    serde_json::to_writer(&mut writer, value)
        .map_err(|_| Error::Invalid("central bundle byte bound/encoding".into()))?;
    Ok(writer.0)
}
/// Recompute from the actual retained request and supplied validated catalog.
/// This proves deterministic data derivation, never authentication or authority.
pub fn verify_central(catalog: &ValidatedCatalog, bundle: &CentralBundle) -> Result<()> {
    for a in [
        &bundle.request,
        &bundle.policy,
        &bundle.compatibility,
        &bundle.compiler,
        &bundle.renderer,
        &bundle.mail_frame,
        &bundle.startup,
        &bundle.artifact,
    ] {
        a.verify_bytes()?;
    }
    ensure(
        bundle.request.utf8.len() <= MAX_REQUEST_BYTES,
        "central request byte bound",
    )?;
    let request: CentralCompileRequest = serde_json::from_value(
        aq_agent_api::parse(bundle.request.utf8.as_bytes())
            .map_err(|_| Error::Invalid("central retained request shape".into()))?,
    )
    .map_err(|_| Error::Invalid("central retained request fields".into()))?;
    let expected = compile_central(catalog, &request)?
        .bundle
        .ok_or_else(|| Error::Invalid("central retained request is ineligible".into()))?;
    ensure(
        bounded_typed_bytes(&expected)? == bounded_typed_bytes(bundle)?,
        "central bundle differs from deterministic original request",
    )
}
pub fn decode_request(bytes: &[u8]) -> Result<CentralCompileRequest> {
    ensure(
        !bytes.is_empty() && bytes.len() <= MAX_REQUEST_BYTES,
        "central request byte bound",
    )?;
    serde_json::from_value(
        aq_agent_api::parse(bytes).map_err(|_| Error::Invalid("central request JSON".into()))?,
    )
    .map_err(|_| Error::Invalid("central request fields".into()))
}
pub fn schema() -> Value {
    let mut s = serde_json::to_value(schemars::schema_for!(Document)).expect("central schema");
    s["$id"] = json!("urn:agent-q:central-profile:1");
    s["x-execution-authority"] = json!(false);
    s["x-required-validation"] =
        json!("compile_central / verify_central plus independently authenticated owner facts");
    s
}
