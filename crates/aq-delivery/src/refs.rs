use super::{DeliveryError, ObjectId, Result, Validate};
use unicode_normalization::UnicodeNormalization;

pub(super) fn oid(value: &ObjectId, allow_zero: bool) -> Result<()> {
    value
        .validate()
        .map_err(|_| DeliveryError::InvalidInput("invalid typed OID"))?;
    if !allow_zero && value.hex.bytes().all(|byte| byte == b'0') {
        return Err(DeliveryError::InvalidInput("invalid typed OID"));
    }
    Ok(())
}

fn branch_byte(byte: u8) -> bool {
    !byte.is_ascii_control()
        && !matches!(byte, b' ' | b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
}
fn branch_component(component: &str) -> bool {
    !component.is_empty() && !component.starts_with('.') && !component.ends_with(".lock")
}
pub(super) fn branch(target: &str) -> Result<()> {
    let full_branch = target.starts_with("refs/heads/") && !target.ends_with('.');
    let separators = !target.contains("..") && !target.contains("@{");
    let valid = full_branch
        && separators
        && target.bytes().all(branch_byte)
        && target.split('/').all(branch_component);
    if !valid {
        return Err(DeliveryError::InvalidInput(
            "invalid full publication branch",
        ));
    }
    Ok(())
}
// Fence case and canonically equivalent Unicode spellings conservatively;
// aq-git still receives and authorizes the caller's exact branch spelling.
pub(super) fn destination_key(target: &str) -> String {
    target
        .nfc()
        .collect::<String>()
        .to_lowercase()
        .nfc()
        .collect()
}
