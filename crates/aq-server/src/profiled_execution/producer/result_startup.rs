//! One supported data-only result channel opt-in, before composition/counting.
//! Exact public bytes come from the existing protected definition owner. This
//! module creates no client, credential, local channel or execution authority.
use super::refused;
use crate::{AppState, error::AppError, profiled_execution::definitions::Definition};
use aq_admission::{ArtifactRef, Digest, Text};
use aq_profiles::{ContextInput, Delivery};
use aq_session_api::Source;
use serde::Deserialize;

pub(super) const STARTUP_ID: &str = "aq.agent-result-startup";
const STARTUP_DIGEST: &str = "667d48a537baa35335fb3da1e589846e9d294a2bc24a8df58cf682181b15229a";
const CONTRACT_DIGEST: &str = "4fe711c405ead85b55a54de2d7446b2c106c4b8809b16bc57497ba2af1d35ef9";
const ADAPTER_ID: &str = "aq.codex-result-local-adapter";
const PROFILE: &str = "codex-0.166-trusted-node-named-readonly-result-local-v1";
const INTERACTIVE_PROFILE: &str = "codex-0.166-chatgpt-interactive-result-local-v1";
const INTERACTIVE_RECIPE_DIGEST: &str =
    "563a2303cd64d6f481b662327ab5dcb3ccbe34554443c446850574a0f14fe254";
// This is a metadata projection of the independently selected whole definition,
// not a second executable-adapter parser. Its installed owner validates the full
// body; ignoring other fields here neither executes nor qualifies them.
#[derive(Deserialize)]
struct OptIn {
    schema_version: u32,
    profile: String,
    contract: ArtifactRef,
    recipe: ArtifactRef,
}
#[derive(Clone)]
pub(super) struct ReportingInstruction {
    pub artifact: ArtifactRef,
    pub text: String,
    pub interactive: bool,
}
fn public_reference(id: &str, digest: &str) -> Result<ArtifactRef, AppError> {
    Ok(ArtifactRef {
        id: Text::new(id).map_err(|_| refused())?,
        version: Text::new("1").map_err(|_| refused())?,
        digest: serde_json::from_value(serde_json::json!(digest)).map_err(|_| refused())?,
    })
}
pub(super) fn from_definitions(
    adapter: &ArtifactRef,
    definitions: &[Definition],
) -> Result<Option<ReportingInstruction>, AppError> {
    if adapter.id.as_str() != ADAPTER_ID {
        return Ok(None);
    }
    if adapter.version.as_str() != "1" {
        return Err(refused());
    }
    let selected = definitions
        .iter()
        .find(|d| d.artifact() == adapter)
        .ok_or_else(refused)?;
    if Digest::of(selected.body().as_bytes()) != adapter.digest {
        return Err(refused());
    }
    let opt: OptIn = serde_json::from_str(selected.body()).map_err(|_| refused())?;
    let recipe_matches = match opt.profile.as_str() {
        PROFILE => {
            opt.recipe.id.as_str() == "aq.codex-result-local-recipe"
                && opt.recipe.version.as_str() == "1"
        }
        INTERACTIVE_PROFILE => {
            opt.recipe
                == public_reference(
                    "aq.codex-interactive-result-local-recipe",
                    INTERACTIVE_RECIPE_DIGEST,
                )?
        }
        _ => false,
    };
    if opt.schema_version != 1
        || !recipe_matches
        || opt.contract != public_reference("aq.agent-result-local", CONTRACT_DIGEST)?
    {
        return Err(refused());
    }
    let artifact = public_reference(STARTUP_ID, STARTUP_DIGEST)?;
    let instruction = definitions
        .iter()
        .find(|d| d.artifact() == &artifact)
        .ok_or_else(refused)?;
    if instruction.body().len() != 1929
        || Digest::of(instruction.body().as_bytes()) != artifact.digest
    {
        return Err(refused());
    }
    Ok(Some(ReportingInstruction {
        artifact,
        text: instruction.body().to_owned(),
        interactive: opt.profile == INTERACTIVE_PROFILE,
    }))
}
pub(super) fn interactive_runtime(
    runtime: &ArtifactRef,
    definitions: &[Definition],
) -> Result<bool, AppError> {
    #[derive(Deserialize)]
    struct RuntimePin {
        runtime: ArtifactRef,
        profile: String,
    }
    for definition in definitions {
        if definition.artifact().id.as_str() != ADAPTER_ID {
            continue;
        }
        let pin: RuntimePin = serde_json::from_str(definition.body()).map_err(|_| refused())?;
        if pin.runtime == *runtime && pin.profile == INTERACTIVE_PROFILE {
            return Ok(from_definitions(definition.artifact(), definitions)?
                .is_some_and(|value| value.interactive));
        }
    }
    Ok(false)
}
pub(super) fn for_adapter(
    state: &AppState,
    adapter: &ArtifactRef,
) -> Result<Option<ReportingInstruction>, AppError> {
    if adapter.id.as_str() != ADAPTER_ID {
        return Ok(None);
    }
    let config = state
        .config
        .profiled_execution
        .as_ref()
        .ok_or_else(refused)?;
    from_definitions(adapter, &config.definition_bodies)
}
impl ReportingInstruction {
    pub fn context(&self) -> ContextInput {
        ContextInput {
            source: Source {
                identity: self.artifact.id.as_str().into(),
                revision: self.artifact.version.as_str().into(),
                digest: self.artifact.digest.as_str().into(),
            },
            text: self.text.clone(),
            delivery: Delivery::Startup,
            required: true,
            omission_priority: 0,
        }
    }
    pub fn validate_rendered(&self, count: usize, bytes: &[u8]) -> Result<(), AppError> {
        if count != 1
            || bytes
                .windows(self.text.len())
                .filter(|part| *part == self.text.as_bytes())
                .count()
                != 1
        {
            return Err(refused());
        }
        Ok(())
    }
    pub fn matches(&self, source: &Source) -> bool {
        source.identity == self.artifact.id.as_str()
            && source.revision == self.artifact.version.as_str()
            && source.digest == self.artifact.digest.as_str()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aq_admission::ArtifactKind;
    use serde_json::{Value, json};
    const TEXT: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/agent-result-startup-v1.md"
    ));
    fn reference(id: &str) -> ArtifactRef {
        public_reference(id, Digest::of(id.as_bytes()).as_str()).unwrap()
    }
    fn body() -> Value {
        json!({"schema_version":1,"profile":PROFILE,"runtime":reference("runtime-code"),"environment":reference("environment"),
            "client_manifest":reference("aq.agent-result-client-installation"),"client_executable":reference("client"),
            "contract":public_reference("aq.agent-result-local",CONTRACT_DIGEST).unwrap(),"recipe":reference("aq.codex-result-local-recipe"),
            "permission_digest":Digest::of(b"whole existing compiled permission plan"),"reserved_client_bytes":67108864})
    }
    fn definitions(body: Value) -> (ArtifactRef, Vec<Definition>) {
        let bytes = serde_json::to_string(&body).unwrap();
        let mut adapter = reference(ADAPTER_ID);
        adapter.digest = Digest::of(bytes.as_bytes());
        let definitions = vec![
            Definition::new(adapter.clone(), ArtifactKind::Definition, bytes).unwrap(),
            Definition::new(
                public_reference(STARTUP_ID, STARTUP_DIGEST).unwrap(),
                ArtifactKind::Definition,
                TEXT.into(),
            )
            .unwrap(),
        ];
        (adapter, definitions)
    }
    #[test]
    fn plain_adapter_needs_no_reporting_definition_and_preserves_empty_context_path() {
        assert!(
            from_definitions(&reference("plain-adapter"), &[])
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn supported_selected_definition_supplies_exact_public_required_startup() {
        let (adapter, defs) = definitions(body());
        let reporting = from_definitions(&adapter, &defs).unwrap().unwrap();
        assert_eq!(reporting.text, TEXT);
        assert_eq!(reporting.text.len(), 1929);
        assert!(!reporting.interactive);
        assert_eq!(
            reporting.artifact,
            public_reference(STARTUP_ID, STARTUP_DIGEST).unwrap()
        );
        let context = reporting.context();
        assert!(context.required && context.delivery == Delivery::Startup);
        assert!(reporting.matches(&context.source));
        let mut changed = context.source;
        changed.digest = Digest::of(b"changed").as_str().into();
        assert!(!reporting.matches(&changed));
    }
    #[test]
    fn interactive_profile_requires_its_exact_public_recipe_and_keeps_counted_startup() {
        let mut value = body();
        value["profile"] = json!(INTERACTIVE_PROFILE);
        value["recipe"] = json!(
            public_reference(
                "aq.codex-interactive-result-local-recipe",
                INTERACTIVE_RECIPE_DIGEST,
            )
            .unwrap()
        );
        let (adapter, defs) = definitions(value.clone());
        let reporting = from_definitions(&adapter, &defs).unwrap().unwrap();
        assert_eq!(reporting.text, TEXT);
        assert!(reporting.interactive);
        assert!(reporting.context().required);
        for recipe in [
            reference("aq.codex-interactive-result-local-recipe"),
            public_reference("aq.codex-result-local-recipe", INTERACTIVE_RECIPE_DIGEST).unwrap(),
        ] {
            value["recipe"] = json!(recipe);
            let (adapter, defs) = definitions(value.clone());
            assert!(from_definitions(&adapter, &defs).is_err());
        }
    }
    #[test]
    fn long_session_factory_uses_actual_interactive_definition_and_runtime() {
        let runtime = reference("runtime-code");
        let (_, defs) = definitions(body());
        assert!(!interactive_runtime(&runtime, &defs).unwrap());
        let mut value = body();
        value["profile"] = json!(INTERACTIVE_PROFILE);
        value["recipe"] = json!(
            public_reference(
                "aq.codex-interactive-result-local-recipe",
                INTERACTIVE_RECIPE_DIGEST,
            )
            .unwrap()
        );
        let (_, defs) = definitions(value.clone());
        assert!(interactive_runtime(&runtime, &defs).unwrap());
        assert!(!interactive_runtime(&reference("other-runtime"), &defs).unwrap());
        value["recipe"] = json!(reference("unqualified-recipe"));
        let (_, defs) = definitions(value);
        assert!(interactive_runtime(&runtime, &defs).is_err());
    }
    #[test]
    fn unsupported_profile_or_contract_and_missing_or_changed_public_bytes_refuse() {
        for (field, value) in [
            ("schema_version", json!(2)),
            ("profile", json!("unknown")),
            ("contract", json!(reference("other-contract"))),
            ("recipe", json!(reference("other-recipe"))),
        ] {
            let mut value_body = body();
            value_body[field] = value;
            let (adapter, defs) = definitions(value_body);
            assert!(from_definitions(&adapter, &defs).is_err());
        }
        let (adapter, mut defs) = definitions(body());
        defs.pop();
        assert!(from_definitions(&adapter, &defs).is_err());
        let changed = TEXT.replace("Finish by submitting", "Do not submit");
        let mut changed_ref = public_reference(STARTUP_ID, STARTUP_DIGEST).unwrap();
        changed_ref.digest = Digest::of(changed.as_bytes());
        defs.push(Definition::new(changed_ref, ArtifactKind::Definition, changed).unwrap());
        assert!(from_definitions(&adapter, &defs).is_err());
    }
    #[test]
    fn counted_rendering_contains_exact_instruction_once_and_never_appends_after_count() {
        let (adapter, defs) = definitions(body());
        let r = from_definitions(&adapter, &defs).unwrap().unwrap();
        let payload = format!("platform\n\n{TEXT}\n\nassignment");
        r.validate_rendered(1, payload.as_bytes()).unwrap();
        assert!(r.validate_rendered(0, payload.as_bytes()).is_err());
        assert!(r.validate_rendered(2, payload.as_bytes()).is_err());
        assert!(
            r.validate_rendered(1, format!("{TEXT}\n\n{TEXT}").as_bytes())
                .is_err()
        );
        assert!(
            r.validate_rendered(1, b"assignment without instruction")
                .is_err()
        );
    }
}
