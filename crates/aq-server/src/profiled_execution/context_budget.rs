//! The startup text's byte proof (spec 0014 §6.1, §8): the server holds no
//! model window and no measurement. The startup descriptor's token fields
//! stay on the wire until the protocol step removes them; each is the byte
//! count it stands beside.
//!
//! `Qualification` and its parts are no longer read by anything: they remain
//! only as the shape of the configuration key `context_qualifications`,
//! which is removed with the test fixtures that still write it (step 3c).
use aq_admission::{ArtifactRef, Digest, Text};
use aq_profiles::roles::StartupText;
use aq_selection::ResolvedSelection;
use aq_session_protocol::Validate;
use serde::{Deserialize, Serialize};
pub const MAX_QUALIFICATIONS: usize = 32;
pub const MAX_QUALIFICATION_BYTES: usize = 262_144;
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
/// The byte proof: no measured fact, no model window. Every token number is
/// the rendered byte count it stands beside, so the budget holds by construction.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Proof {
    pub schema_version: u32,
    pub renderer: ArtifactRef,
    pub selection_digest: Digest,
    /// The digest of the prepared launch the selection chose.
    pub launch_digest: Digest,
    pub rendering: Rendering,
    pub rendered_digest: Digest,
    pub rendered_bytes: u64,
    pub input_tokens_upper_bound: u64,
    pub reserved_output_tokens: u64,
    pub instructions: Vec<InstructionBound>,
}
impl Proof {
    /// The context plan's window: exactly the counted input plus the
    /// reserved output.
    pub fn window_tokens(&self) -> Option<u64> {
        (self.input_tokens_upper_bound).checked_add(self.reserved_output_tokens)
    }
}
/// What a startup descriptor names as its renderer qualification: the one
/// constant rendering rule.
pub struct Renderer {
    pub definition: ArtifactRef,
    pub body: Vec<u8>,
}
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
        })
    }
}
pub struct CountedStartup {
    pub bytes: Vec<u8>,
    pub proof: Proof,
}
/// The proof for an assembled startup text and the selection that chose it.
/// The text was already held to its byte limit when it was assembled; each
/// instruction's token bound is its byte count with its separator, so the
/// bounds sum to the rendered size.
pub fn count(
    startup: &StartupText,
    selection: &ResolvedSelection,
) -> Result<(CountedStartup, Renderer), Error> {
    let bytes = startup.rendered.as_bytes().to_vec();
    let output = selection.choice.max_output_tokens;
    if startup.instructions.is_empty()
        || output == 0
        || bytes.len() as u64 > selection.choice.resources.context_bytes
    {
        return Err(Error::Input);
    }
    let bounds: Vec<InstructionBound> = (startup.instructions.iter().enumerate())
        .map(|(index, (_, text))| InstructionBound {
            instruction_index: index,
            tokens_upper_bound: text.len() as u64 + if index == 0 { 0 } else { 2 },
        })
        .collect();
    let renderer = Renderer::rule()?;
    let proof = Proof {
        schema_version: 2,
        renderer: renderer.definition.clone(),
        selection_digest: aq_selection::digest(selection).map_err(|_| Error::Input)?,
        launch_digest: selection.choice.launch_digest.clone(),
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
