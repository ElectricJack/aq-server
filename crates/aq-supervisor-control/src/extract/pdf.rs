//! PDF text without a PDF library: objects are found by scanning, page order
//! follows the page tree, and text comes from the content streams' text
//! operators. Fonts map through their `ToUnicode` CMap; a simple font without
//! one reads as Latin-1 unless it has custom glyph names, and a font that
//! cannot be mapped yields U+FFFD, counted in the returned note. Not handled:
//! encrypted files (reported), non-Flate stream filters (reported), OCR.
mod document;
mod font;
mod object;
mod page;

use super::{Budget, Limits, Res, Stop};
use document::Doc;
use font::fonts;
use object::Obj;
use page::{Page, interpret};
use std::collections::BTreeMap;

const MAX_OBJECTS: usize = 200_000;
const MAX_DEPTH: usize = 32;
const MAX_CMAP_ENTRIES: usize = 200_000;
const MAX_PAGES: usize = 10_000;

fn unreadable(why: &str) -> Stop {
    Stop::Unreadable(format!(
        "the PDF is damaged or truncated ({why}); upload the file again or re-export it"
    ))
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}
pub(super) fn extract(bytes: &[u8], limits: &Limits, budget: &mut Budget) -> Res<(String, String)> {
    validate_header(bytes)?;
    let doc = Doc::load(bytes, budget)?;
    let pages = doc.pages();
    if pages.is_empty() {
        return Err(unreadable("it has no pages"));
    }
    let (mut cache, mut texts) = (BTreeMap::new(), Vec::new());
    let (mut mapped, mut unmapped) = (0, 0);
    let limit = limits.max_text_bytes.saturating_mul(2);
    let mut total_bytes = 0;
    for (n, resources) in &pages {
        let Some(page) = doc.dict_of(*n) else {
            continue;
        };
        let fonts = fonts(
            &doc,
            page.get("Resources").or(*resources),
            &mut cache,
            budget,
        )?;
        let mut state = Page {
            out: String::new(),
            font: None,
            fonts: &fonts,
            last_y: None,
            mapped: 0,
            unmapped: 0,
            limit,
        };
        for c in &content_streams(page.get("Contents"), &doc) {
            let Obj::Ref(r) = c else { continue };
            let data = doc.stream(*r, budget)?;
            interpret(&data, &mut state)?;
            state.newline();
        }
        mapped += state.mapped;
        unmapped += state.unmapped;
        total_bytes += state.out.len();
        texts.push(state.out);
        if total_bytes > limit {
            return Err(Stop::Limit(
                "the PDF text is far over the extraction limit".into(),
            ));
        }
    }
    extraction_text(texts, mapped, unmapped)
}

fn validate_header(bytes: &[u8]) -> Res<()> {
    if !bytes.starts_with(b"%PDF-") {
        return Err(unreadable("the %PDF- header is missing"));
    }
    if !bytes[bytes.len().saturating_sub(2048)..]
        .windows(5)
        .any(|w| w == b"%%EOF")
    {
        return Err(unreadable("the %%EOF marker is missing"));
    }
    Ok(())
}

fn content_streams(content: Option<&Obj>, doc: &Doc<'_>) -> Vec<Obj> {
    match content {
        Some(c) => match doc.resolve(c) {
            Obj::Array(list) => list.clone(),
            Obj::Null => vec![],
            _ => vec![c.clone()],
        },
        None => vec![],
    }
}

fn extraction_text(texts: Vec<String>, mapped: usize, unmapped: usize) -> Res<(String, String)> {
    if mapped == 0 {
        return Err(if unmapped > 0 {
            Stop::Failed(format!(
                "the PDF has {unmapped} text characters but its fonts carry no Unicode \
                 mapping, so they cannot be read; export it again with embedded text \
                 or send a text version"
            ))
        } else {
            Stop::Empty(
                "the PDF has no extractable text (it may be scanned images; \
                 this server does not run OCR)"
                    .into(),
            )
        });
    }
    let text = texts
        .iter()
        .map(|t| t.trim())
        .collect::<Vec<_>>()
        .join("\n\n");
    let note = if unmapped > 0 {
        format!("{unmapped} characters have no Unicode mapping in the PDF and appear as U+FFFD")
    } else {
        String::new()
    };
    Ok((text, note))
}
