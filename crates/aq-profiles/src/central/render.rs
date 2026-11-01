//! Versioned deterministic text joining only. No template evaluation or host claim.
use super::*;
pub(super) fn descriptor() -> Result<GeneratedArtifact> {
    GeneratedArtifact::canonical(
        ArtifactKind::Renderer,
        &serde_json::json!({"schema_version":1,"algorithm":"central-startup-two-lf-utf8-v1","package":"aq-profiles","package_version":env!("CARGO_PKG_VERSION"),"source_digest":Digest::of(include_bytes!("render.rs")),"token_counter":null,"installation_qualification":null}),
    )
}
pub(super) fn render(instructions: &[Instruction], limit: u64) -> Result<String> {
    ensure(instructions.len() <= 128, "central instruction count")?;
    let mut text = String::new();
    for instruction in instructions {
        if instruction.delivery != Delivery::Startup {
            continue;
        }
        ensure(
            !instruction.text.contains('\0'),
            "central startup contains NUL",
        )?;
        let separator = if text.is_empty() { 0 } else { 2 };
        let length = text
            .len()
            .checked_add(separator)
            .and_then(|n| n.checked_add(instruction.text.len()))
            .ok_or_else(|| Error::Invalid("central startup overflow".into()))?;
        ensure(
            length <= MAX_STARTUP_BYTES && length as u64 <= limit,
            "central rendered startup exceeds byte ceiling",
        )?;
        if separator != 0 {
            text.push_str("\n\n");
        }
        text.push_str(&instruction.text);
    }
    ensure(!text.is_empty(), "central empty startup")?;
    Ok(text)
}
