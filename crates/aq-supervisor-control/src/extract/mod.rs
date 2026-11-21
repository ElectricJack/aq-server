//! Text of a stored supervisor attachment, with an explicit state for every
//! outcome. Pure: the caller (aq-server) fetches the bytes behind its own
//! authorization and passes the stored [`Descriptor`] with them, so the
//! result always names the file it came from and never widens access.
//!
//! Nothing is dropped silently. A file whose text does not fit
//! [`Limits::max_text_bytes`], or whose decompression exceeds
//! [`Limits::max_inflated_bytes`], is [`State::LimitExceeded`] with no text,
//! never a prefix. Characters a PDF font cannot map to Unicode appear as
//! U+FFFD and are counted in the message.
use crate::attachments::{DOCX, Descriptor, MAX_FILE_BYTES};
use flate2::read::{DeflateDecoder, ZlibDecoder};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io::Read;

mod docx;
mod pdf;
#[cfg(test)]
mod tests;
mod text;

/// Ceiling on extracted text, in UTF-8 bytes. Four times the largest file.
pub const MAX_TEXT_BYTES: usize = 4 * MAX_FILE_BYTES;
/// Ceiling on bytes inflated while reading one file (a 1 MiB file may
/// otherwise expand a thousandfold).
pub const MAX_INFLATED_BYTES: usize = 32 << 20;

/// Work limits of one extraction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_text_bytes: usize,
    pub max_inflated_bytes: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_text_bytes: MAX_TEXT_BYTES,
            max_inflated_bytes: MAX_INFLATED_BYTES,
        }
    }
}

/// What happened to one attachment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// `text` holds the complete text of the file.
    Extracted,
    /// A valid file with no text in it (an image, a scanned PDF, an empty
    /// document). Nothing is wrong with the upload.
    NoExtractableText,
    /// The bytes are not a readable file of the declared type: damaged,
    /// truncated, encrypted or not decodable.
    Unreadable,
    /// The file looks valid but this extractor cannot read it (unsupported
    /// feature or media type) or its stored bytes disagree with the descriptor.
    ExtractionFailed,
    /// The text or the work needed exceeds a [`Limits`] value.
    LimitExceeded,
}

/// The outcome for one file: its source, a state, the text only when the
/// state is [`State::Extracted`], and a message for a person to act on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Extraction {
    pub source: Descriptor,
    pub state: State,
    pub text: Option<String>,
    pub message: String,
}

/// Why extraction stopped, before it is tied to a source.
pub(crate) enum Stop {
    Empty(String),
    Unreadable(String),
    Failed(String),
    Limit(String),
}
pub(crate) type Res<T> = Result<T, Stop>;

/// Inflated bytes still allowed for one extraction.
pub(crate) struct Budget(pub usize);
impl Budget {
    /// Inflates `raw` (zlib framed for PDF, raw deflate for ZIP).
    pub(crate) fn inflate(&mut self, raw: &[u8], zlib: bool) -> Res<Vec<u8>> {
        let (mut out, cap) = (Vec::new(), self.0 as u64 + 1);
        let read = if zlib {
            ZlibDecoder::new(raw).take(cap).read_to_end(&mut out)
        } else {
            DeflateDecoder::new(raw).take(cap).read_to_end(&mut out)
        };
        read.map_err(|e| Stop::Unreadable(format!("compressed data is damaged ({e})")))?;
        if out.len() > self.0 {
            return Err(Stop::Limit(
                "the file expands past the decompression limit of this server".into(),
            ));
        }
        self.0 -= out.len();
        Ok(out)
    }
}

/// Extracts the text of `bytes`, the stored content of `source`.
pub fn extract(source: &Descriptor, bytes: &[u8], limits: &Limits) -> Extraction {
    let outcome = run(source, bytes, limits);
    let (state, text, message) = match outcome {
        Ok((text, note)) => (
            State::Extracted,
            Some(text),
            if note.is_empty() {
                "text extracted".to_owned()
            } else {
                format!("text extracted; {note}")
            },
        ),
        Err(Stop::Empty(m)) => (State::NoExtractableText, None, m),
        Err(Stop::Unreadable(m)) => (State::Unreadable, None, m),
        Err(Stop::Failed(m)) => (State::ExtractionFailed, None, m),
        Err(Stop::Limit(m)) => (State::LimitExceeded, None, m),
    };
    Extraction {
        source: source.clone(),
        state,
        text,
        message,
    }
}

fn run(source: &Descriptor, bytes: &[u8], limits: &Limits) -> Res<(String, String)> {
    if bytes.len() as u64 != source.size_bytes
        || source.digest != format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
    {
        return Err(Stop::Failed(
            "the stored bytes do not match the attachment's recorded size and digest; \
             nothing was extracted"
                .into(),
        ));
    }
    if bytes.len() > MAX_FILE_BYTES {
        return Err(Stop::Limit(format!(
            "the file is larger than {MAX_FILE_BYTES} bytes"
        )));
    }
    let mut budget = Budget(limits.max_inflated_bytes);
    let (text, note) = match source.media_type.as_str() {
        "text/plain" | "text/markdown" | "text/csv" | "application/json" => {
            (text::decode(bytes)?, String::new())
        }
        "application/pdf" => pdf::extract(bytes, limits, &mut budget)?,
        DOCX => (docx::extract(bytes, &mut budget)?, String::new()),
        "image/png" | "image/jpeg" | "image/gif" | "image/webp" => {
            return Err(Stop::Empty(
                "an image has no text layer and this server does not run OCR; \
                 send the text itself or a text-based document"
                    .into(),
            ));
        }
        other => {
            return Err(Stop::Failed(format!(
                "no text extractor exists for media type {other}"
            )));
        }
    };
    if text.len() > limits.max_text_bytes {
        return Err(Stop::Limit(format!(
            "the extracted text is {} bytes, over the limit of {}; no part of it is returned",
            text.len(),
            limits.max_text_bytes
        )));
    }
    if text.trim().is_empty() {
        return Err(Stop::Empty("the file contains no text".into()));
    }
    Ok((text, note))
}
