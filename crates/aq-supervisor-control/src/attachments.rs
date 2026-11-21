//! Owner attachment rules for the supervisor conversation (agile-vault-27):
//! the accepted media types, their content checks, the limits discovery
//! advertises, and the structured refusals the upload routes return. aq-server
//! owns storage and authorization; everything here is pure.
use axum::{
    Json,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};

/// Most files one `Create` may bind, whatever the configuration: the
/// `ordinal` column of the store holds 0 to 3. The default limit.
pub const MAX_PER_MESSAGE: usize = 4;
/// Largest file body, whatever the configuration: the `size_bytes` column of
/// the store holds up to this. The default limit.
pub const MAX_FILE_BYTES: usize = 1 << 20;
/// Staged (not yet attached) files one conversation may hold at once.
pub const MAX_STAGED_FILES: i64 = 16;
/// Staged bytes one conversation may hold at once.
pub const MAX_STAGED_BYTES: i64 = 16 << 20;
/// A staged file is purged this long after its upload unless a Create binds it.
pub const STAGED_TTL_MS: i64 = 24 * 60 * 60 * 1000;
/// Largest file name, in UTF-8 bytes.
pub const MAX_NAME_BYTES: usize = 255;
pub const DOCX: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
const TEXT: [&str; 4] = [
    "text/plain",
    "text/markdown",
    "text/csv",
    "application/json",
];
/// Every accepted `Content-Type`, in the order discovery lists them.
pub const MEDIA_TYPES: [&str; 10] = [
    "text/plain",
    "text/markdown",
    "text/csv",
    "application/json",
    "application/pdf",
    DOCX,
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
];

/// `[supervisor_attachments]`: the effective per-file and per-message limits.
/// Both default to, and may not exceed, [`MAX_FILE_BYTES`] and
/// [`MAX_PER_MESSAGE`]; reading stored files always uses those ceilings, so
/// lowering a limit never hides a file that was accepted earlier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    /// Largest accepted file body in bytes (the PUT body limit).
    pub max_file_bytes: usize,
    /// Files one `Create` may bind (also `limits.attachments`).
    pub max_files_per_message: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_file_bytes: MAX_FILE_BYTES,
            max_files_per_message: MAX_PER_MESSAGE,
        }
    }
}
impl Limits {
    /// Whether both limits are at least 1 and within their ceilings.
    pub fn valid(&self) -> bool {
        (1..=MAX_FILE_BYTES).contains(&self.max_file_bytes)
            && (1..=MAX_PER_MESSAGE).contains(&self.max_files_per_message)
    }
}

/// Discovery's `attachments` object: what this server accepts right now.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Capability {
    pub media_types: &'static [&'static str],
    pub max_file_bytes: u32,
    pub max_files_per_message: u8,
    pub max_staged_files: u32,
    pub max_staged_bytes: u32,
    pub staged_ttl_ms: u32,
    pub max_name_bytes: u16,
}
/// What `limits` enforces, advertised to clients.
pub fn capability(limits: &Limits) -> Capability {
    Capability {
        media_types: &MEDIA_TYPES,
        max_file_bytes: limits.max_file_bytes as u32,
        max_files_per_message: limits.max_files_per_message as u8,
        max_staged_files: MAX_STAGED_FILES as u32,
        max_staged_bytes: MAX_STAGED_BYTES as u32,
        staged_ttl_ms: STAGED_TTL_MS as u32,
        max_name_bytes: MAX_NAME_BYTES as u16,
    }
}

pub fn allowed(media_type: &str) -> bool {
    MEDIA_TYPES.contains(&media_type)
}
pub fn digest_valid(s: &str) -> bool {
    s.strip_prefix("sha256:").is_some_and(|h| {
        h.len() == 64
            && h.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}
/// One path segment: no control characters, slashes or edge whitespace.
pub fn name_valid(name: &str) -> bool {
    (1..=MAX_NAME_BYTES).contains(&name.len())
        && name.trim() == name
        && name != "."
        && name != ".."
        && !name
            .chars()
            .any(|c| c.is_control() || c == '/' || c == '\\')
}
/// Whether `bytes` is a readable file of the declared (allowed) type.
pub fn content_valid(media_type: &str, bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return false;
    }
    if TEXT.contains(&media_type) {
        let Ok(text) = std::str::from_utf8(bytes) else {
            return false;
        };
        return !text.contains('\0')
            && (media_type != "application/json"
                || serde_json::from_str::<serde::de::IgnoredAny>(text).is_ok());
    }
    match media_type {
        // A truncated PDF has no end-of-file marker near its end.
        "application/pdf" => {
            bytes.starts_with(b"%PDF-")
                && bytes[bytes.len().saturating_sub(1024)..]
                    .windows(5)
                    .any(|w| w == b"%%EOF")
        }
        DOCX => docx(bytes),
        "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/jpeg" => bytes.starts_with(b"\xff\xd8\xff"),
        "image/gif" => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        "image/webp" => bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"),
        _ => false,
    }
}
/// A DOCX is a ZIP whose central directory names `[Content_Types].xml` and
/// `word/document.xml`. Entry data is not inflated.
fn docx(bytes: &[u8]) -> bool {
    let u16_at = |i: usize| {
        bytes
            .get(i..i + 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]) as usize)
    };
    let u32_at = |i: usize| {
        bytes
            .get(i..i + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
    };
    if !bytes.starts_with(b"PK\x03\x04") || bytes.len() < 22 {
        return false;
    }
    // The end-of-central-directory record ends with a comment of at most 64 KiB.
    let floor = bytes.len().saturating_sub(22 + 0xffff);
    let Some(end) = (floor..=bytes.len() - 22)
        .rev()
        .find(|&i| bytes[i..].starts_with(b"PK\x05\x06"))
    else {
        return false;
    };
    let (Some(count), Some(size), Some(start)) =
        (u16_at(end + 10), u32_at(end + 12), u32_at(end + 16))
    else {
        return false;
    };
    if start.checked_add(size).is_none_or(|e| e > end) {
        return false;
    }
    let (mut at, mut types, mut document) = (start, false, false);
    for _ in 0..count {
        if !bytes
            .get(at..)
            .is_some_and(|b| b.starts_with(b"PK\x01\x02"))
        {
            return false;
        }
        let (Some(n), Some(x), Some(c)) = (u16_at(at + 28), u16_at(at + 30), u16_at(at + 32))
        else {
            return false;
        };
        let Some(name) = bytes.get(at + 46..at + 46 + n) else {
            return false;
        };
        types |= name == b"[Content_Types].xml";
        document |= name == b"word/document.xml";
        at += 46 + n + x + c;
    }
    at <= end && types && document
}

/// An inert download of stored bytes whatever their declared type: the
/// stored `Content-Type`, an RFC 5987 file name, `nosniff`, a sandbox CSP and
/// `x-aq-content-digest`. `None` when a header value cannot be encoded.
pub fn serve(name: &str, media_type: &str, digest: &str, bytes: Vec<u8>) -> Option<Response> {
    let encoded: String = name
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    let value = |s: &str| HeaderValue::from_str(s).ok();
    let mut response = bytes.into_response();
    let h = response.headers_mut();
    h.insert(header::CONTENT_TYPE, value(media_type)?);
    h.insert(
        header::CONTENT_DISPOSITION,
        value(&format!("attachment; filename*=UTF-8''{encoded}"))?,
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("sandbox; default-src 'none'"),
    );
    h.insert("x-aq-content-digest", value(digest)?);
    Some(response)
}

/// A structured attachment refusal: `{"error": code, "message": text}`, the
/// same body shape as every other AQ API error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    MediaTypeUnsupported,
    TooLarge,
    NameInvalid,
    Unreadable,
    IdConflict,
    CapacityExhausted,
    NotFound,
    Bound,
}
impl Refusal {
    pub fn status(self) -> StatusCode {
        match self {
            Self::MediaTypeUnsupported => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Self::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::NameInvalid | Self::Unreadable => StatusCode::BAD_REQUEST,
            Self::IdConflict | Self::Bound => StatusCode::CONFLICT,
            Self::CapacityExhausted => StatusCode::SERVICE_UNAVAILABLE,
            Self::NotFound => StatusCode::NOT_FOUND,
        }
    }
    pub fn code(self) -> &'static str {
        match self {
            Self::MediaTypeUnsupported => "attachment_media_type_unsupported",
            Self::TooLarge => "attachment_too_large",
            Self::NameInvalid => "attachment_name_invalid",
            Self::Unreadable => "attachment_unreadable",
            Self::IdConflict => "attachment_id_conflict",
            Self::CapacityExhausted => "attachment_capacity_exhausted",
            Self::NotFound => "attachment_not_found",
            Self::Bound => "attachment_bound",
        }
    }
    fn message(self) -> &'static str {
        match self {
            Self::MediaTypeUnsupported => "Content-Type is not an accepted attachment media type",
            Self::TooLarge => "attachment exceeds max_file_bytes of this server",
            Self::NameInvalid => "name must be one path segment of 1 to 255 UTF-8 bytes",
            Self::Unreadable => {
                "attachment bytes are empty or not a readable file of the declared type"
            }
            Self::IdConflict => "attachment id already holds a different upload",
            Self::CapacityExhausted => {
                "staged attachment capacity of this conversation is exhausted"
            }
            Self::NotFound => "attachment not found",
            Self::Bound => "attachment is bound to a message and lives as long as it",
        }
    }
}
impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        let body = serde_json::json!({"error": self.code(), "message": self.message()});
        (self.status(), Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// A stored (uncompressed) ZIP with the named empty entries.
    pub(super) fn zip(names: &[&str]) -> Vec<u8> {
        let (mut out, mut directory) = (Vec::new(), Vec::new());
        for name in names {
            let offset = out.len() as u32;
            out.extend(b"PK\x03\x04\x14\0");
            out.extend([0; 20]);
            out.extend((name.len() as u16).to_le_bytes());
            out.extend([0, 0]);
            out.extend(name.as_bytes());
            directory.extend(b"PK\x01\x02\x14\0\x14\0");
            directory.extend([0; 20]);
            directory.extend((name.len() as u16).to_le_bytes());
            directory.extend([0; 12]);
            directory.extend(offset.to_le_bytes());
            directory.extend(name.as_bytes());
        }
        let start = out.len() as u32;
        out.extend(&directory);
        out.extend(b"PK\x05\x06\0\0\0\0");
        out.extend((names.len() as u16).to_le_bytes());
        out.extend((names.len() as u16).to_le_bytes());
        out.extend((directory.len() as u32).to_le_bytes());
        out.extend(start.to_le_bytes());
        out.extend([0, 0]);
        out
    }
    #[test]
    fn content_checks_follow_the_declared_type() {
        assert!(content_valid("text/plain", b"hello"));
        assert!(content_valid("text/markdown", "# Plan\n- é".as_bytes()));
        assert!(!content_valid("text/plain", b""));
        assert!(!content_valid("text/plain", b"\xff\xfe"));
        assert!(!content_valid("text/plain", b"a\0b"));
        assert!(content_valid("application/json", br#"{"a":1}"#));
        assert!(!content_valid("application/json", b"{"));
        assert!(content_valid(
            "application/pdf",
            b"%PDF-1.7\n1 0 obj\n%%EOF\n"
        ));
        assert!(
            !content_valid("application/pdf", b"%PDF-1.7\n1 0 obj\n"),
            "truncated"
        );
        assert!(!content_valid("application/pdf", b"PK\x03\x04%%EOF"));
        assert!(content_valid("image/png", b"\x89PNG\r\n\x1a\nrest"));
        assert!(!content_valid("image/png", b"GIF89a"));
        assert!(content_valid("image/webp", b"RIFF\0\0\0\0WEBPVP8 "));
        assert!(!content_valid("image/webp", b"RIFF\0\0\0\0WAVEfmt "));
        assert!(!allowed("text/html"));
        assert!(!allowed("application/msword"));
        assert!(!allowed("text/plain; charset=utf-8"));
        assert!(allowed(DOCX));
    }
    #[test]
    fn docx_must_be_a_wordprocessing_package() {
        let good = zip(&["[Content_Types].xml", "_rels/.rels", "word/document.xml"]);
        assert!(content_valid(DOCX, &good));
        let mut commented = good.clone();
        let n = commented.len();
        commented[n - 2] = 3;
        commented.extend(b"abc");
        assert!(content_valid(DOCX, &commented), "trailing zip comment");
        for bad in [
            zip(&["[Content_Types].xml", "xl/workbook.xml"]),
            zip(&["word/document.xml"]),
            good[..good.len() - 1].to_vec(),
            good[..40].to_vec(),
            b"%PDF-1.7 %%EOF".to_vec(),
            b"PK\x03\x04".to_vec(),
        ] {
            assert!(!content_valid(DOCX, &bad), "{bad:?}");
        }
        let mut lying = good.clone();
        let n = lying.len();
        lying[n - 6..n - 2].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(
            !content_valid(DOCX, &lying),
            "directory offset out of range"
        );
    }
    #[test]
    fn limits_default_to_the_ceilings_and_never_exceed_them() {
        assert_eq!(Limits::default().max_file_bytes, 1 << 20);
        assert_eq!(Limits::default().max_files_per_message, 4);
        assert!(Limits::default().valid());
        let parsed = |t: &str| serde_json::from_str::<Limits>(t);
        assert_eq!(parsed("{}").unwrap(), Limits::default());
        let half = parsed(r#"{"max_file_bytes":524288}"#).unwrap();
        assert_eq!(
            (half.max_file_bytes, half.max_files_per_message),
            (524288, 4)
        );
        assert!(half.valid());
        for bad in [
            r#"{"max_file_bytes":0}"#,
            r#"{"max_file_bytes":1048577}"#,
            r#"{"max_files_per_message":0}"#,
            r#"{"max_files_per_message":5}"#,
        ] {
            assert!(!parsed(bad).unwrap().valid(), "{bad}");
        }
        assert!(parsed(r#"{"max_file_bytes":-1}"#).is_err());
        assert!(parsed(r#"{"max_files":2}"#).is_err());
    }
    #[test]
    fn names_are_single_safe_path_segments() {
        assert!(name_valid("notes v2.md"));
        assert!(name_valid(&"é".repeat(127)));
        for bad in [
            "",
            " a",
            "a ",
            ".",
            "..",
            "a/b",
            "a\\b",
            "a\nb",
            &"x".repeat(256),
            &"é".repeat(128),
        ] {
            assert!(!name_valid(bad), "{bad:?}");
        }
    }
    #[test]
    fn capability_lists_the_enforced_limits() {
        let c = serde_json::to_value(capability(&Limits::default())).unwrap();
        assert_eq!(c["max_file_bytes"], 1048576);
        assert_eq!(c["max_files_per_message"], 4);
        let lowered = Limits {
            max_file_bytes: 2048,
            max_files_per_message: 2,
        };
        let c = serde_json::to_value(capability(&lowered)).unwrap();
        assert_eq!(c["max_file_bytes"], 2048);
        assert_eq!(c["max_files_per_message"], 2);
        assert_eq!(
            c["media_types"].as_array().unwrap().len(),
            MEDIA_TYPES.len()
        );
        assert!(MEDIA_TYPES.iter().all(|t| allowed(t)));
        assert_eq!(Refusal::TooLarge.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }
}
