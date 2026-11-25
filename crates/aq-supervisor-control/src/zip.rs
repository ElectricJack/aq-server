//! Directory metadata shared by DOCX type validation and text extraction.

pub(super) fn u16_at(bytes: &[u8], i: usize) -> Option<usize> {
    Some(u16::from_le_bytes(bytes.get(i..i + 2)?.try_into().ok()?) as usize)
}

pub(super) fn u32_at(bytes: &[u8], i: usize) -> Option<usize> {
    Some(u32::from_le_bytes(bytes.get(i..i + 4)?.try_into().ok()?) as usize)
}

/// Locate the end-of-central-directory record and its optional 64 KiB comment.
pub(super) fn end_record(bytes: &[u8]) -> Option<usize> {
    let last = bytes.len().checked_sub(22)?;
    let floor = last.saturating_sub(0xffff);
    (floor..=last)
        .rev()
        .find(|&i| bytes[i..].starts_with(b"PK\x05\x06"))
}
