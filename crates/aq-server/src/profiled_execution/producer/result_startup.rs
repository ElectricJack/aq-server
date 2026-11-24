//! One supported data-only result channel opt-in, before composition/counting.
//! Exact public bytes come from the existing protected definition owner. This
//! module creates no client, credential, local channel or execution authority.
use super::refused;
use crate::{AppState, error::AppError, profiled_execution::definitions::Definition};
use aq_admission::{ArtifactRef, DOCKER_LOCAL_PROFILES, Digest, DockerLocalProfile, Text};
use serde::Deserialize;

pub(super) const STARTUP_ID: &str = "aq.agent-result-startup";
const STARTUP_DIGEST: &str = "667d48a537baa35335fb3da1e589846e9d294a2bc24a8df58cf682181b15229a";
/// Byte length of the pinned startup instruction.
const STARTUP_LEN: usize = 1929;
const CONTRACT_DIGEST: &str = "4fe711c405ead85b55a54de2d7446b2c106c4b8809b16bc57497ba2af1d35ef9";
const ADAPTER_ID: &str = "aq.codex-result-local-adapter";
/// Docker-local executors (long phase, no terminal input): `DOCKER_LOCAL_PROFILES`.
fn result_adapter(id: &str) -> bool {
    id == ADAPTER_ID || DOCKER_LOCAL_PROFILES.iter().any(|p| p.adapter == id)
}
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
    /// Terminal input is enabled for the harness.
    pub interactive: bool,
    /// The adapter is qualified for the long (schema 3) phase policy.
    pub long_phase: bool,
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
    if !result_adapter(adapter.id.as_str()) {
        return Ok(None);
    }
    let opt = selected_opt_in(adapter, definitions)?;
    // A docker-local adapter identity carries only its own profile.
    let sandbox = DOCKER_LOCAL_PROFILES
        .iter()
        .find(|p| p.adapter == adapter.id.as_str());
    if sandbox.is_some_and(|p| p.profile != opt.profile) {
        return Err(refused());
    }
    if !supported_opt_in(&opt, adapter)? {
        return Err(refused());
    }
    let (artifact, text) = startup_instruction(definitions)?;
    let interactive = opt.profile == INTERACTIVE_PROFILE;
    Ok(Some(ReportingInstruction {
        artifact,
        text,
        interactive,
        long_phase: interactive || sandbox.is_some(),
    }))
}
/// The opt-in metadata of the whole definition selected for `adapter`.
fn selected_opt_in(adapter: &ArtifactRef, definitions: &[Definition]) -> Result<OptIn, AppError> {
    // Definition revisions advance when whole runtime/environment bytes change.
    // The Definition owner validates the revision; protocol versions stay below.
    let selected = definitions
        .iter()
        .find(|d| d.artifact() == adapter)
        .ok_or_else(|| refused())?;
    if Digest::of(selected.body().as_bytes()) != adapter.digest {
        return Err(refused());
    }
    serde_json::from_str(selected.body()).map_err(|_| refused())
}
/// Schema one, the recipe pinned for the profile and the public contract.
fn supported_opt_in(opt: &OptIn, adapter: &ArtifactRef) -> Result<bool, AppError> {
    Ok(opt.schema_version == 1
        && recipe_matches(opt, adapter)?
        && opt.contract == public_reference("aq.agent-result-local", CONTRACT_DIGEST)?)
}
/// The opt-in names the recipe its profile pins for `adapter`.
fn recipe_matches(opt: &OptIn, adapter: &ArtifactRef) -> Result<bool, AppError> {
    let recipe = &opt.recipe;
    let version_one = recipe.version.as_str() == "1";
    Ok(match opt.profile.as_str() {
        PROFILE => recipe.id.as_str() == "aq.codex-result-local-recipe" && version_one,
        INTERACTIVE_PROFILE => {
            *recipe
                == public_reference(
                    "aq.codex-interactive-result-local-recipe",
                    INTERACTIVE_RECIPE_DIGEST,
                )?
        }
        profile => {
            version_one
                && DOCKER_LOCAL_PROFILES
                    .iter()
                    .filter(|p| p.profile == profile && p.adapter == adapter.id.as_str())
                    .any(|p| pins_recipe(p, recipe))
        }
    })
}
/// The docker-local profile pins `recipe`'s id and, where it has one, digest.
fn pins_recipe(sandbox: &DockerLocalProfile, recipe: &ArtifactRef) -> bool {
    sandbox.recipe_id == recipe.id.as_str()
        && sandbox
            .recipe_digest
            .is_none_or(|d| d == recipe.digest.as_str())
}
/// The exact public startup instruction, by pinned reference and length.
fn startup_instruction(definitions: &[Definition]) -> Result<(ArtifactRef, String), AppError> {
    let artifact = public_reference(STARTUP_ID, STARTUP_DIGEST)?;
    let instruction = definitions
        .iter()
        .find(|d| d.artifact() == &artifact)
        .ok_or_else(|| refused())?;
    if instruction.body().len() != STARTUP_LEN
        || Digest::of(instruction.body().as_bytes()) != artifact.digest
    {
        return Err(refused());
    }
    Ok((artifact, instruction.body().to_owned()))
}
/// Profiles that run under the long (schema 3) phase policy.
fn long_phase_profile(profile: &str) -> bool {
    profile == INTERACTIVE_PROFILE || DOCKER_LOCAL_PROFILES.iter().any(|p| p.profile == profile)
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
    let result_definitions = definitions
        .iter()
        .filter(|d| result_adapter(d.artifact().id.as_str()));
    for definition in result_definitions {
        let pin: RuntimePin = serde_json::from_str(definition.body()).map_err(|_| refused())?;
        if pin.runtime == *runtime && long_phase_profile(&pin.profile) {
            return Ok(from_definitions(definition.artifact(), definitions)?
                .is_some_and(|value| value.long_phase));
        }
    }
    Ok(false)
}
pub(super) fn for_adapter(
    state: &AppState,
    adapter: &ArtifactRef,
) -> Result<Option<ReportingInstruction>, AppError> {
    if !result_adapter(adapter.id.as_str()) {
        return Ok(None);
    }
    let config = state
        .config
        .profiled_execution
        .as_ref()
        .ok_or_else(|| refused())?;
    // A derived adapter (spec 0017) reports as the boot adapter it derives from.
    #[cfg(feature = "project_environments")]
    let adapter = &(aq_environments::boot_adapter(
        adapter,
        config.definition_bodies.iter().map(Definition::artifact),
    ))
    .unwrap_or_else(|| adapter.clone());
    from_definitions(adapter, &config.definition_bodies)
}
impl ReportingInstruction {
    pub fn validate_rendered(&self, count: usize, bytes: &[u8]) -> Result<(), AppError> {
        if count != 1 || occurrences(bytes, self.text.as_bytes()) != 1 {
            return Err(refused());
        }
        Ok(())
    }
}
/// Possibly overlapping occurrences of a non-empty `needle` in `bytes`.
fn occurrences(bytes: &[u8], needle: &[u8]) -> usize {
    bytes
        .windows(needle.len())
        .filter(|part| *part == needle)
        .count()
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
    fn interactive_body() -> Value {
        let mut value = body();
        value["profile"] = json!(INTERACTIVE_PROFILE);
        value["recipe"] = json!(
            public_reference(
                "aq.codex-interactive-result-local-recipe",
                INTERACTIVE_RECIPE_DIGEST,
            )
            .unwrap()
        );
        value
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
    }
    #[test]
    fn docker_local_profiles_run_the_long_phase_with_their_pinned_recipe_only() {
        for sandbox in &DOCKER_LOCAL_PROFILES {
            let (_, startup) = definitions(body());
            let select = |adapter: &str, recipe: ArtifactRef| {
                let mut value = body();
                value["profile"] = json!(sandbox.profile);
                value["recipe"] = json!(recipe);
                let bytes = serde_json::to_string(&value).unwrap();
                let mut adapter = reference(adapter);
                adapter.digest = Digest::of(bytes.as_bytes());
                let mut defs = startup.clone();
                defs[0] =
                    Definition::new(adapter.clone(), ArtifactKind::Definition, bytes).unwrap();
                (adapter, defs)
            };
            let pin = public_reference(sandbox.recipe_id, sandbox.recipe_digest.unwrap()).unwrap();
            let (adapter, defs) = select(sandbox.adapter, pin.clone());
            let reporting = from_definitions(&adapter, &defs).unwrap().unwrap();
            assert!(!reporting.interactive && reporting.long_phase);
            assert!(interactive_runtime(&reference("runtime-code"), &defs).unwrap());
            // Another recipe body under the same profile is refused.
            let (adapter, defs) = select(sandbox.adapter, reference(sandbox.recipe_id));
            assert!(
                from_definitions(&adapter, &defs).is_err(),
                "{}",
                sandbox.harness
            );
            // A docker-local profile under the Codex result adapter identity is refused.
            let (adapter, defs) = select(ADAPTER_ID, pin);
            assert!(
                from_definitions(&adapter, &defs).is_err(),
                "{}",
                sandbox.harness
            );
        }
    }
    #[test]
    fn interactive_profile_requires_its_exact_public_recipe_and_keeps_counted_startup() {
        let mut value = interactive_body();
        let (adapter, defs) = definitions(value.clone());
        let reporting = from_definitions(&adapter, &defs).unwrap().unwrap();
        assert_eq!(reporting.text, TEXT);
        assert!(reporting.interactive);
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
    fn definition_revision_two_preserves_schema_recipe_and_contract_one() {
        fn revision_two(body: Value) -> (ArtifactRef, Vec<Definition>) {
            let (mut adapter, mut defs) = definitions(body);
            adapter.version = Text::new("2").unwrap();
            defs[0] = Definition::new(
                adapter.clone(),
                ArtifactKind::Definition,
                defs[0].body().to_owned(),
            )
            .unwrap();
            (adapter, defs)
        }
        let value = interactive_body();
        let (adapter, defs) = revision_two(value.clone());
        assert!(
            from_definitions(&adapter, &defs)
                .unwrap()
                .unwrap()
                .interactive
        );
        assert!(interactive_runtime(&reference("runtime-code"), &defs).unwrap());
        for (field, bad) in [
            ("schema_version", json!(2)),
            ("profile", json!("unqualified-profile")),
            ("recipe", json!(reference("unqualified-recipe"))),
            ("contract", json!(reference("unqualified-contract"))),
        ] {
            let mut changed = value.clone();
            changed[field] = bad;
            let (adapter, defs) = revision_two(changed);
            assert!(from_definitions(&adapter, &defs).is_err(), "{field}");
        }
        let mut wrong_hash = adapter.clone();
        wrong_hash.digest = Digest::of(b"other body");
        assert!(from_definitions(&wrong_hash, &defs).is_err());
        let mut malformed = serde_json::to_value(adapter).unwrap();
        malformed["version"] = json!("");
        assert!(serde_json::from_value::<ArtifactRef>(malformed).is_err());
    }
    #[test]
    fn long_session_factory_uses_actual_interactive_definition_and_runtime() {
        let runtime = reference("runtime-code");
        let (_, defs) = definitions(body());
        assert!(!interactive_runtime(&runtime, &defs).unwrap());
        let mut value = interactive_body();
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
