//! Qualified startup counting/capacity facts. No default tokenizer, guessed
//! native window, provider call or executable hook is supplied by this owner.
use super::registration::RegisteredRuntime;
use aq_admission::{ArtifactRef, Digest, Text};
use aq_profiles::{Delivery, FrozenComposition};
use aq_selection::{ExecutionTarget, ResolvedSelection};
use aq_session_protocol::Validate;
use serde::{Deserialize, Serialize};
pub const MAX_QUALIFICATIONS: usize = 32;
pub const MAX_QUALIFICATION_BYTES: usize = 262_144;
const MAX_PROMPT_BYTES: usize = 1_048_576;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Subject {
    pub runtime: ArtifactRef,
    pub runtime_qualification: ArtifactRef,
    pub harness: String,
    pub cli_version: String,
    pub adapter: ArtifactRef,
    pub environment: ArtifactRef,
    pub harness_qualification: ArtifactRef,
    pub provider: String,
    pub model: String,
    pub native_reasoning: String,
    pub reasoning_configuration_reference: String,
    pub credential_profile: ArtifactRef,
    pub credential_revision: u64,
    pub account_reference: String,
    pub endpoint_reference: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rendering {
    Utf8DoubleNewlineV1,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapacityKind {
    QualifiedModelWindow,
    QualifiedConservativePlanningCeiling,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Qualification {
    pub schema_version: u32,
    pub subject: Subject,
    pub rendering: Rendering,
    /// This multiplier is an explicitly qualified upper bound for this exact
    /// model/adapter, not a universal statement about tokenizers.
    pub maximum_tokens_per_utf8_byte: u32,
    /// Qualified framing/role/CLI overhead beyond the rendered input bytes.
    pub fixed_overhead_tokens: u64,
    pub counting_evidence: ArtifactRef,
    pub capacity_kind: CapacityKind,
    pub capacity_tokens: u64,
    pub capacity_evidence: ArtifactRef,
    pub observed_at_ms: u64,
    pub expires_at_ms: u64,
}
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("invalid qualified context facts")]
    InvalidFacts,
    #[error("qualified context facts are unavailable, expired or ambiguous")]
    Unavailable,
    #[error("qualified context subject differs from the sealed selection")]
    Subject,
    #[error("startup context is invalid or exceeds its byte bound")]
    Input,
    #[error("startup context exceeds the qualified planning capacity")]
    Capacity,
}
fn require(value: bool) -> Result<(), Error> {
    if value {
        Ok(())
    } else {
        Err(Error::InvalidFacts)
    }
}
fn text(value: &str) -> bool {
    !value.is_empty() && value.len() <= 512 && !value.chars().any(char::is_control)
}
fn artifact(value: &ArtifactRef) -> Result<(), Error> {
    value.id.validate().map_err(|_| Error::InvalidFacts)?;
    value.version.validate().map_err(|_| Error::InvalidFacts)
}
impl Qualification {
    pub fn validate(&self) -> Result<(), Error> {
        let s = &self.subject;
        require(
            self.schema_version == 1
                && s.credential_revision > 0
                && s.credential_revision <= i64::MAX as u64,
        )?;
        for value in [
            &s.runtime,
            &s.runtime_qualification,
            &s.adapter,
            &s.environment,
            &s.harness_qualification,
            &s.credential_profile,
            &self.counting_evidence,
            &self.capacity_evidence,
        ] {
            artifact(value)?;
        }
        for value in [
            &s.harness,
            &s.cli_version,
            &s.provider,
            &s.model,
            &s.native_reasoning,
            &s.reasoning_configuration_reference,
            &s.account_reference,
            &s.endpoint_reference,
        ] {
            require(text(value))?;
        }
        require(
            (1..=64).contains(&self.maximum_tokens_per_utf8_byte)
                && self.capacity_tokens > 0
                && self.capacity_tokens <= 16_777_216
                && self.fixed_overhead_tokens < self.capacity_tokens,
        )?;
        require(
            self.observed_at_ms > 0
                && self.expires_at_ms > self.observed_at_ms
                && self.expires_at_ms - self.observed_at_ms <= 86_400_000
                && self.expires_at_ms <= i64::MAX as u64,
        )
    }
    /// A content-addressed definition cannot reuse one identity/version after
    /// changing facts. The artifact itself does not prove external qualification.
    pub fn artifact(&self) -> Result<ArtifactRef, Error> {
        self.validate()?;
        let digest = aq_selection::digest(self).map_err(|_| Error::InvalidFacts)?;
        Ok(ArtifactRef {
            id: Text::new(format!("aq.qualified-context/{}", digest.as_str()))
                .map_err(|_| Error::InvalidFacts)?,
            version: Text::new("1").map_err(|_| Error::InvalidFacts)?,
            digest,
        })
    }
    #[cfg(not(feature = "any-model"))]
    fn matches(&self, runtime: &RegisteredRuntime, selection: &ResolvedSelection) -> bool {
        let s = &self.subject;
        let c = &selection.choice;
        s.runtime == runtime.runtime
            && s.runtime_qualification == runtime.qualification
            && s.harness == c.harness
            && s.cli_version == c.cli_version
            && s.adapter == c.adapter
            && s.environment == c.environment
            && s.harness_qualification == c.qualification
            && s.provider == c.provider
            && s.model == c.model
            && s.native_reasoning == c.native_reasoning
            && s.reasoning_configuration_reference == c.reasoning_configuration_reference
            && s.credential_profile == c.credential_profile
            && s.credential_revision == selection.credential_revision
            && s.account_reference == c.account_reference
            && s.endpoint_reference == c.endpoint_reference
    }
}
pub fn validate_catalog(facts: &[Qualification]) -> Result<(), Error> {
    require(facts.len() <= MAX_QUALIFICATIONS)?;
    require(
        serde_json::to_vec(facts)
            .map_err(|_| Error::InvalidFacts)?
            .len()
            <= MAX_QUALIFICATION_BYTES,
    )?;
    let mut subjects = std::collections::BTreeSet::new();
    for fact in facts {
        fact.validate()?;
        require(
            subjects.insert(aq_selection::digest(&fact.subject).map_err(|_| Error::InvalidFacts)?),
        )?;
    }
    Ok(())
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstructionBound {
    pub instruction_index: usize,
    pub tokens_upper_bound: u64,
}
#[cfg(not(feature = "any-model"))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Proof {
    pub schema_version: u32,
    pub qualification: ArtifactRef,
    pub counting_evidence: ArtifactRef,
    pub capacity_evidence: ArtifactRef,
    pub capacity_kind: CapacityKind,
    pub selection_digest: Digest,
    pub composition_digest: Digest,
    pub rendering: Rendering,
    pub rendered_digest: Digest,
    pub rendered_bytes: u64,
    pub input_tokens_upper_bound: u64,
    pub reserved_output_tokens: u64,
    pub capacity_tokens: u64,
    pub instructions: Vec<InstructionBound>,
    pub observed_at_ms: u64,
    pub expires_at_ms: u64,
}
/// The byte proof: no measured fact, no model window. Every token number is
/// the rendered byte count it stands beside, so the budget holds by construction.
#[cfg(feature = "any-model")]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Proof {
    pub schema_version: u32,
    pub renderer: ArtifactRef,
    pub selection_digest: Digest,
    pub composition_digest: Digest,
    pub rendering: Rendering,
    pub rendered_digest: Digest,
    pub rendered_bytes: u64,
    pub input_tokens_upper_bound: u64,
    pub reserved_output_tokens: u64,
    pub instructions: Vec<InstructionBound>,
}
impl Proof {
    /// The renderer qualification this proof was counted under.
    pub fn renderer(&self) -> &ArtifactRef {
        #[cfg(not(feature = "any-model"))]
        return &self.qualification;
        #[cfg(feature = "any-model")]
        return &self.renderer;
    }
    /// The context plan's window: the fact's capacity, or with `any-model`
    /// exactly the counted input plus the reserved output.
    pub fn window_tokens(&self) -> Option<u64> {
        #[cfg(not(feature = "any-model"))]
        return Some(self.capacity_tokens);
        #[cfg(feature = "any-model")]
        return (self.input_tokens_upper_bound).checked_add(self.reserved_output_tokens);
    }
}
/// What a startup descriptor names as its renderer qualification: a measured
/// fact, or with `any-model` the one constant rendering rule.
pub struct Renderer {
    pub definition: ArtifactRef,
    pub body: Vec<u8>,
    /// Tokens the renderer adds beyond the rendered bytes.
    pub implicit_tokens: u64,
    /// Definitions the offer retains with it (a fact's evidence).
    pub evidence: Vec<ArtifactRef>,
    /// When a measured fact expires; the rendering rule never does.
    pub expires_at_ms: Option<u64>,
}
#[cfg(feature = "any-model")]
impl Renderer {
    /// The same Definition for every model: its body states only the rendering rule.
    pub fn rule() -> Result<Self, Error> {
        let body = serde_json::to_vec(&serde_json::json!({
            "rendering": Rendering::Utf8DoubleNewlineV1,
            "schema_version": 1,
        }))
        .map_err(|_| Error::InvalidFacts)?;
        let digest = Digest::of(&body);
        Ok(Self {
            definition: ArtifactRef {
                id: Text::new(format!("aq.startup-renderer/{}", digest.as_str()))
                    .map_err(|_| Error::InvalidFacts)?,
                version: Text::new("1").map_err(|_| Error::InvalidFacts)?,
                digest,
            },
            body,
            implicit_tokens: 0,
            evidence: vec![],
            expires_at_ms: None,
        })
    }
}
pub struct CountedStartup {
    pub bytes: Vec<u8>,
    pub proof: Proof,
}
/// The selection names this runtime boot and this exact composition.
fn checked(
    runtime: &RegisteredRuntime,
    selection: &ResolvedSelection,
    composition: &FrozenComposition,
    now_ms: u64,
) -> Result<(), Error> {
    let ExecutionTarget::Worker { node_id, boot_id } = selection.target else {
        return Err(Error::Subject);
    };
    if node_id.is_nil() || boot_id.is_nil() || boot_id != runtime.boot_id || now_ms == 0 {
        return Err(Error::Subject);
    }
    composition.validate_native_v2().map_err(|_| Error::Input)?;
    if aq_selection::digest(composition).map_err(|_| Error::Input)?
        != selection.choice.composition_digest
        || composition.harness != selection.choice.harness
        || (composition.environment != selection.choice.environment.id.as_str()
            && composition.environment
                != format!(
                    "{}@{}",
                    selection.choice.environment.id.as_str(),
                    selection.choice.environment.version.as_str()
                ))
    {
        return Err(Error::Subject);
    }
    Ok(())
}
/// The startup instructions joined by a blank line, within the byte limits,
/// and each one's token bound at `tokens_per_byte`.
fn rendered(
    selection: &ResolvedSelection,
    composition: &FrozenComposition,
    tokens_per_byte: u64,
) -> Result<(Vec<u8>, Vec<InstructionBound>), Error> {
    let mut bytes = Vec::new();
    let mut bounds = Vec::new();
    for (index, instruction) in composition.instruction_order.iter().enumerate() {
        if instruction.delivery != Delivery::Startup {
            continue;
        }
        let separator = if bounds.is_empty() { 0 } else { 2 };
        let size = bytes
            .len()
            .checked_add(separator)
            .and_then(|n| n.checked_add(instruction.text.len()))
            .ok_or(Error::Input)?;
        if size > MAX_PROMPT_BYTES
            || size as u64
                > selection
                    .choice
                    .resources
                    .context_bytes
                    .min(composition.context_limit_bytes)
        {
            return Err(Error::Input);
        }
        if separator != 0 {
            bytes.extend_from_slice(b"\n\n");
        }
        bytes.extend_from_slice(instruction.text.as_bytes());
        let tokens = (instruction.text.len() as u64)
            .checked_add(separator as u64)
            .and_then(|n| n.checked_mul(tokens_per_byte))
            .ok_or(Error::Capacity)?;
        bounds.push(InstructionBound {
            instruction_index: index,
            tokens_upper_bound: tokens,
        });
    }
    Ok((bytes, bounds))
}
/// Consumes only daemon-owned configured facts plus already selected values.
/// Callers must separately authenticate/fence runtime, configuration, registry,
/// project and original facts after waits. This result is never a grant.
#[cfg(not(feature = "any-model"))]
pub fn evaluate(
    facts: &[Qualification],
    runtime: &RegisteredRuntime,
    selection: &ResolvedSelection,
    composition: &FrozenComposition,
    now_ms: u64,
) -> Result<CountedStartup, Error> {
    validate_catalog(facts)?;
    checked(runtime, selection, composition, now_ms)?;
    let fact = facts
        .iter()
        .find(|fact| {
            fact.matches(runtime, selection)
                && fact.observed_at_ms <= now_ms
                && now_ms < fact.expires_at_ms
        })
        .ok_or(Error::Unavailable)?;
    let (bytes, mut bounds) = rendered(
        selection,
        composition,
        fact.maximum_tokens_per_utf8_byte as u64,
    )?;
    let first = bounds.first_mut().ok_or(Error::Input)?;
    first.tokens_upper_bound = first
        .tokens_upper_bound
        .checked_add(fact.fixed_overhead_tokens)
        .ok_or(Error::Capacity)?;
    let input = bounds
        .iter()
        .try_fold(0u64, |n, b| n.checked_add(b.tokens_upper_bound))
        .ok_or(Error::Capacity)?;
    let output = selection.choice.max_output_tokens;
    if output == 0
        || input
            .checked_add(output)
            .is_none_or(|n| n > fact.capacity_tokens)
    {
        return Err(Error::Capacity);
    }
    let proof = Proof {
        schema_version: 1,
        qualification: fact.artifact()?,
        counting_evidence: fact.counting_evidence.clone(),
        capacity_evidence: fact.capacity_evidence.clone(),
        capacity_kind: fact.capacity_kind,
        selection_digest: aq_selection::digest(selection).map_err(|_| Error::Input)?,
        composition_digest: selection.choice.composition_digest.clone(),
        rendering: fact.rendering,
        rendered_digest: Digest::of(&bytes),
        rendered_bytes: bytes.len() as u64,
        input_tokens_upper_bound: input,
        reserved_output_tokens: output,
        capacity_tokens: fact.capacity_tokens,
        instructions: bounds,
        observed_at_ms: fact.observed_at_ms,
        expires_at_ms: fact.expires_at_ms,
    };
    Ok(CountedStartup { bytes, proof })
}
/// The startup render under the byte limits alone. No fact is consulted and no
/// model window compared: each instruction's token bound is its byte count, so
/// the bounds sum to the rendered size and the window is that plus the output.
#[cfg(feature = "any-model")]
pub fn evaluate(
    runtime: &RegisteredRuntime,
    selection: &ResolvedSelection,
    composition: &FrozenComposition,
    now_ms: u64,
) -> Result<(CountedStartup, Renderer), Error> {
    checked(runtime, selection, composition, now_ms)?;
    let (bytes, bounds) = rendered(selection, composition, 1)?;
    let output = selection.choice.max_output_tokens;
    if bounds.is_empty() || output == 0 {
        return Err(Error::Input);
    }
    let renderer = Renderer::rule()?;
    let proof = Proof {
        schema_version: 2,
        renderer: renderer.definition.clone(),
        selection_digest: aq_selection::digest(selection).map_err(|_| Error::Input)?,
        composition_digest: selection.choice.composition_digest.clone(),
        rendering: Rendering::Utf8DoubleNewlineV1,
        rendered_digest: Digest::of(&bytes),
        rendered_bytes: bytes.len() as u64,
        input_tokens_upper_bound: bytes.len() as u64,
        reserved_output_tokens: output,
        instructions: bounds,
    };
    proof.window_tokens().ok_or(Error::Input)?;
    Ok((CountedStartup { bytes, proof }, renderer))
}

#[cfg(all(test, not(feature = "any-model")))]
mod tests {
    use super::super::config::{ProfiledConfig, QualifiedRuntime};
    use super::*;
    use uuid::Uuid;
    fn reference(id: &str) -> ArtifactRef {
        ArtifactRef {
            id: Text::new(id).unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(id.as_bytes()),
        }
    }
    fn case() -> (
        RegisteredRuntime,
        ResolvedSelection,
        FrozenComposition,
        Qualification,
    ) {
        let raw: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/selection-v2.json")).unwrap();
        let catalog = aq_profiles::ValidatedCatalog::from_artifact(
            raw["catalog_artifact_json"].as_str().unwrap().as_bytes(),
        )
        .unwrap();
        let input: aq_selection::SelectionInputs =
            serde_json::from_value(raw["inputs"].clone()).unwrap();
        let selection = aq_selection::select(&catalog, &input)
            .unwrap()
            .resolved
            .unwrap();
        let ExecutionTarget::Worker { boot_id, .. } = selection.target else {
            unreachable!()
        };
        let runtime = RegisteredRuntime {
            runtime: reference("synthetic-only-runtime"),
            qualification: reference("synthetic-only-runtime-qualification"),
            boot_id,
        };
        let c = &selection.choice;
        let qualification = Qualification {
            schema_version: 1,
            subject: Subject {
                runtime: runtime.runtime.clone(),
                runtime_qualification: runtime.qualification.clone(),
                harness: c.harness.clone(),
                cli_version: c.cli_version.clone(),
                adapter: c.adapter.clone(),
                environment: c.environment.clone(),
                harness_qualification: c.qualification.clone(),
                provider: c.provider.clone(),
                model: c.model.clone(),
                native_reasoning: c.native_reasoning.clone(),
                reasoning_configuration_reference: c.reasoning_configuration_reference.clone(),
                credential_profile: c.credential_profile.clone(),
                credential_revision: selection.credential_revision,
                account_reference: c.account_reference.clone(),
                endpoint_reference: c.endpoint_reference.clone(),
            },
            rendering: Rendering::Utf8DoubleNewlineV1,
            maximum_tokens_per_utf8_byte: 2,
            fixed_overhead_tokens: 37,
            counting_evidence: reference("synthetic-only-byte-counting-proof"),
            capacity_kind: CapacityKind::QualifiedConservativePlanningCeiling,
            capacity_tokens: 16384,
            capacity_evidence: reference("synthetic-only-safe-planning-ceiling-proof"),
            observed_at_ms: 1000,
            expires_at_ms: 10000,
        };
        (
            runtime,
            selection,
            input.compositions[0].composition.clone(),
            qualification,
        )
    }
    #[test]
    fn qualified_count_binds_actual_utf8_rendering_overhead_and_reserved_output() {
        let (runtime, mut selection, mut composition, fact) = case();
        let assignment = composition.instruction_order.last_mut().unwrap();
        assignment.text = "Exact assignment: 🦀 café".into();
        assignment.source.digest = Digest::of(assignment.text.as_bytes()).as_str().into();
        selection.choice.composition_digest = aq_selection::digest(&composition).unwrap();
        let counted = evaluate(
            std::slice::from_ref(&fact),
            &runtime,
            &selection,
            &composition,
            2000,
        )
        .unwrap();
        let expected = composition
            .instruction_order
            .iter()
            .filter(|i| i.delivery == Delivery::Startup)
            .map(|i| i.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        assert_eq!(counted.bytes, expected.as_bytes());
        assert!(expected.len() > expected.chars().count());
        assert_eq!(
            counted.proof.rendered_digest,
            Digest::of(expected.as_bytes())
        );
        assert_eq!(
            counted.proof.input_tokens_upper_bound,
            (expected.len() as u64) * 2 + 37
        );
        assert_eq!(
            counted
                .proof
                .instructions
                .iter()
                .map(|i| i.tokens_upper_bound)
                .sum::<u64>(),
            counted.proof.input_tokens_upper_bound
        );
        assert_eq!(
            counted.proof.reserved_output_tokens,
            selection.choice.max_output_tokens
        );
        assert_eq!(counted.proof.qualification, fact.artifact().unwrap());
        assert_eq!(
            counted.proof.capacity_kind,
            CapacityKind::QualifiedConservativePlanningCeiling
        );
    }
    #[test]
    fn missing_expired_or_wrong_exact_subject_never_infers_model_facts_from_bytes() {
        let (runtime, selection, composition, fact) = case();
        assert!(matches!(
            evaluate(&[], &runtime, &selection, &composition, 2000),
            Err(Error::Unavailable)
        ));
        assert!(matches!(
            evaluate(
                std::slice::from_ref(&fact),
                &runtime,
                &selection,
                &composition,
                fact.expires_at_ms
            ),
            Err(Error::Unavailable)
        ));
        for field in [
            "provider",
            "model",
            "native_reasoning",
            "reasoning_configuration_reference",
            "harness",
            "cli_version",
            "account_reference",
            "endpoint_reference",
        ] {
            let mut value = serde_json::to_value(&fact).unwrap();
            value["subject"][field] = serde_json::json!("different");
            let changed: Qualification = serde_json::from_value(value).unwrap();
            assert!(
                matches!(
                    evaluate(&[changed], &runtime, &selection, &composition, 2000),
                    Err(Error::Unavailable)
                ),
                "{field}"
            );
        }
        for field in [
            "runtime",
            "runtime_qualification",
            "adapter",
            "environment",
            "harness_qualification",
            "credential_profile",
        ] {
            let mut value = serde_json::to_value(&fact).unwrap();
            value["subject"][field] = serde_json::json!(reference("different"));
            let changed: Qualification = serde_json::from_value(value).unwrap();
            assert!(
                matches!(
                    evaluate(&[changed], &runtime, &selection, &composition, 2000),
                    Err(Error::Unavailable)
                ),
                "{field}"
            );
        }
        let mut changed = fact.clone();
        changed.subject.credential_revision += 1;
        assert!(matches!(
            evaluate(&[changed], &runtime, &selection, &composition, 2000),
            Err(Error::Unavailable)
        ));
        let mut stale = runtime;
        stale.boot_id = Uuid::from_u128(999);
        assert!(matches!(
            evaluate(&[fact], &stale, &selection, &composition, 2000),
            Err(Error::Subject)
        ));
    }
    #[test]
    fn capacity_overflow_zero_unknown_and_ambiguous_definitions_refuse() {
        let (runtime, selection, composition, fact) = case();
        let counted = evaluate(
            std::slice::from_ref(&fact),
            &runtime,
            &selection,
            &composition,
            2000,
        )
        .unwrap();
        let mut insufficient = fact.clone();
        insufficient.capacity_tokens =
            counted.proof.input_tokens_upper_bound + selection.choice.max_output_tokens - 1;
        assert!(matches!(
            evaluate(&[insufficient], &runtime, &selection, &composition, 2000),
            Err(Error::Capacity)
        ));
        let mut overflowing = selection.clone();
        overflowing.choice.max_output_tokens = u64::MAX;
        assert!(matches!(
            evaluate(
                std::slice::from_ref(&fact),
                &runtime,
                &overflowing,
                &composition,
                2000
            ),
            Err(Error::Capacity)
        ));
        for (field, value) in [
            ("capacity_tokens", 0),
            ("maximum_tokens_per_utf8_byte", 0),
            ("maximum_tokens_per_utf8_byte", u64::MAX),
            ("fixed_overhead_tokens", u64::MAX),
            ("observed_at_ms", 0),
        ] {
            let mut raw = serde_json::to_value(&fact).unwrap();
            raw[field] = serde_json::json!(value);
            let rejected = serde_json::from_value::<Qualification>(raw)
                .map_or(true, |f| f.validate().is_err());
            assert!(rejected, "{field}");
        }
        assert!(validate_catalog(&[fact.clone(), fact.clone()]).is_err());
        let mut changed = fact.clone();
        changed.maximum_tokens_per_utf8_byte = 3;
        assert_ne!(fact.artifact().unwrap(), changed.artifact().unwrap());
        let mut unknown = serde_json::to_value(fact).unwrap();
        unknown["capacity_tokens"] = serde_json::Value::Null;
        assert!(serde_json::from_value::<Qualification>(unknown).is_err());
    }
    #[test]
    fn on_demand_content_is_not_claimed_as_zero_token_startup_and_tampering_refuses() {
        let (runtime, mut selection, mut composition, fact) = case();
        let mut reference = composition.instruction_order.last().unwrap().clone();
        reference.required = false;
        reference.delivery = Delivery::OnDemand;
        reference.text = "Deferred reference".into();
        reference.source.digest = Digest::of(reference.text.as_bytes()).as_str().into();
        composition.instruction_order.push(reference);
        selection.choice.composition_digest = aq_selection::digest(&composition).unwrap();
        let result = evaluate(
            std::slice::from_ref(&fact),
            &runtime,
            &selection,
            &composition,
            2000,
        )
        .unwrap();
        assert_eq!(
            result.proof.instructions.len(),
            composition.instruction_order.len() - 1
        );
        assert!(
            !String::from_utf8(result.bytes)
                .unwrap()
                .contains("Deferred reference")
        );
        composition.instruction_order[0].text.push_str(" altered");
        assert!(evaluate(&[fact], &runtime, &selection, &composition, 2000).is_err());
    }
    #[test]
    fn optional_config_defaults_preserve_old_digest_and_facts_require_owned_runtime() {
        let (runtime, _, _, fact) = case();
        let raw = serde_json::json!({"revision":1,"qualified_runtimes":[],"projects":[]});
        let config: ProfiledConfig = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(serde_json::to_value(&config).unwrap(), raw);
        let mut with = config;
        with.context_qualifications = vec![fact.clone()];
        assert!(with.validate().is_err());
        with.qualified_runtimes = vec![QualifiedRuntime {
            runtime: runtime.runtime,
            qualification: runtime.qualification,
            capabilities: vec![],
            features: vec![],
            material_delivery: None,
        }];
        with.validate().unwrap();
        assert_ne!(
            aq_selection::digest(&with).unwrap(),
            aq_selection::digest(&raw).unwrap()
        );
        let mut ambiguous = with.clone();
        ambiguous.context_qualifications.push(fact);
        assert!(ambiguous.validate().is_err());
    }
}
