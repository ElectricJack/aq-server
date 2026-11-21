//! Plain text, Markdown, CSV and JSON: a strict decode, never a lossy one.
use super::{Res, Stop};

pub(super) fn decode(bytes: &[u8]) -> Res<String> {
    let text = match bytes {
        [0xff, 0xfe, rest @ ..] => utf16(rest, u16::from_le_bytes)?,
        [0xfe, 0xff, rest @ ..] => utf16(rest, u16::from_be_bytes)?,
        _ => {
            let body = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
            std::str::from_utf8(body)
                .map_err(|e| {
                    Stop::Unreadable(format!(
                        "the file is not valid UTF-8 (first bad byte at offset {}); \
                         re-save it as UTF-8 and upload it again",
                        e.valid_up_to() + (bytes.len() - body.len())
                    ))
                })?
                .to_owned()
        }
    };
    if text.contains('\0') {
        return Err(Stop::Unreadable(
            "the file contains NUL characters, so it is binary rather than text".into(),
        ));
    }
    Ok(text.replace("\r\n", "\n"))
}
fn utf16(rest: &[u8], unit: fn([u8; 2]) -> u16) -> Res<String> {
    let bad = || {
        Stop::Unreadable(
            "the file is not valid UTF-16; re-save it as UTF-8 and upload it again".into(),
        )
    };
    if !rest.len().is_multiple_of(2) {
        return Err(bad());
    }
    char::decode_utf16(rest.chunks_exact(2).map(|c| unit([c[0], c[1]])))
        .collect::<Result<String, _>>()
        .map_err(|_| bad())
}
