//! The startup text's byte proof (spec 0014 §6.1, §8): the server holds no
//! model window and no measurement. The startup descriptor's token fields
//! stay on the wire until the protocol step removes them; each is the byte
//! count it stands beside.
use aq_admission::{ArtifactRef, Digest, Text};
use aq_profiles::roles::StartupText;
use aq_selection::ResolvedSelection;
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rendering {
    Utf8DoubleNewlineV1,
}
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("the startup renderer definition cannot be built")]
    Renderer,
    #[error("startup context is invalid or exceeds its byte bound")]
    Input,
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
        .map_err(|_| Error::Renderer)?;
        let digest = Digest::of(&body);
        Ok(Self {
            definition: ArtifactRef {
                id: Text::new(format!("aq.startup-renderer/{}", digest.as_str()))
                    .map_err(|_| Error::Renderer)?,
                version: Text::new("1").map_err(|_| Error::Renderer)?,
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
