//! DOCX: a ZIP package whose `word/document.xml` holds the body text.
//! Headers, footers, footnotes and text inside embedded objects are not read.
use super::{Budget, Res, Stop};
use flate2::Crc;

const MAX_ENTRIES: usize = 4096;

struct Entry<'a> {
    name: &'a [u8],
    flags: u16,
    method: u16,
    crc: u32,
    packed: usize,
    size: usize,
    local: usize,
}

pub(super) fn extract(bytes: &[u8], budget: &mut Budget) -> Res<String> {
    if bytes.starts_with(b"\xd0\xcf\x11\xe0") {
        return Err(Stop::Unreadable(
            "this is a legacy or password-protected Office file, not a DOCX package; \
             remove the password or save it as .docx and upload it again"
                .into(),
        ));
    }
    let entries = directory(bytes)?;
    let part = |name: &[u8]| entries.iter().find(|e| e.name == name);
    if part(b"[Content_Types].xml").is_none() {
        return Err(damaged("the package has no [Content_Types].xml"));
    }
    let Some(document) = part(b"word/document.xml") else {
        return Err(damaged("the package has no word/document.xml"));
    };
    let xml = read(bytes, document, budget)?;
    body_text(&xml)
}
fn damaged(why: &str) -> Stop {
    Stop::Unreadable(format!(
        "the DOCX is damaged or truncated ({why}); upload the file again or re-save it"
    ))
}

fn directory(bytes: &[u8]) -> Res<Vec<Entry<'_>>> {
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
        return Err(damaged("it is not a ZIP package"));
    }
    let floor = bytes.len().saturating_sub(22 + 0xffff);
    let end = (floor..=bytes.len() - 22)
        .rev()
        .find(|&i| bytes[i..].starts_with(b"PK\x05\x06"))
        .ok_or_else(|| damaged("the ZIP end record is missing"))?;
    let (Some(count), Some(size), Some(start)) =
        (u16_at(end + 10), u32_at(end + 12), u32_at(end + 16))
    else {
        return Err(damaged("the ZIP end record is cut short"));
    };
    if count == 0xffff || size == 0xffff_ffff || start == 0xffff_ffff {
        return Err(Stop::Failed("ZIP64 packages are not supported".into()));
    }
    if count > MAX_ENTRIES {
        return Err(Stop::Limit(format!(
            "the package has {count} entries, over the limit of {MAX_ENTRIES}"
        )));
    }
    if start.checked_add(size).is_none_or(|e| e > end) {
        return Err(damaged("the ZIP directory is out of range"));
    }
    let (mut at, mut out) = (start, Vec::with_capacity(count));
    for _ in 0..count {
        if !bytes
            .get(at..)
            .is_some_and(|b| b.starts_with(b"PK\x01\x02"))
        {
            return Err(damaged("a ZIP directory entry is invalid"));
        }
        let field = |i, f: &dyn Fn(usize) -> Option<usize>| {
            f(at + i).ok_or_else(|| damaged("a ZIP directory entry is cut short"))
        };
        let (n, x, c) = (
            field(28, &u16_at)?,
            field(30, &u16_at)?,
            field(32, &u16_at)?,
        );
        out.push(Entry {
            name: bytes
                .get(at + 46..at + 46 + n)
                .ok_or_else(|| damaged("a ZIP entry name is cut short"))?,
            flags: field(8, &u16_at)? as u16,
            method: field(10, &u16_at)? as u16,
            crc: field(16, &u32_at)? as u32,
            packed: field(20, &u32_at)?,
            size: field(24, &u32_at)?,
            local: field(42, &u32_at)?,
        });
        at += 46 + n + x + c;
    }
    if at > end {
        return Err(damaged("the ZIP directory is out of range"));
    }
    Ok(out)
}

/// The verified, inflated content of one entry.
fn read(bytes: &[u8], e: &Entry<'_>, budget: &mut Budget) -> Res<Vec<u8>> {
    if e.flags & 1 != 0 {
        return Err(Stop::Unreadable(
            "the DOCX is encrypted; remove the password and upload it again".into(),
        ));
    }
    let head = bytes
        .get(e.local..e.local + 30)
        .filter(|h| h.starts_with(b"PK\x03\x04"))
        .ok_or_else(|| damaged("a ZIP entry header is invalid"))?;
    let skip = u16::from_le_bytes([head[26], head[27]]) as usize
        + u16::from_le_bytes([head[28], head[29]]) as usize;
    let data = e
        .local
        .checked_add(30 + skip)
        .and_then(|from| bytes.get(from..from.checked_add(e.packed)?))
        .ok_or_else(|| damaged("a ZIP entry is cut short"))?;
    if e.size > budget.0 {
        return Err(Stop::Limit(
            "the document expands past the decompression limit of this server".into(),
        ));
    }
    let out = match e.method {
        0 => {
            budget.0 -= data.len().min(budget.0);
            data.to_vec()
        }
        8 => budget.inflate(data, false)?,
        m => {
            return Err(Stop::Failed(format!(
                "ZIP compression method {m} is not supported"
            )));
        }
    };
    let mut crc = Crc::new();
    crc.update(&out);
    if out.len() != e.size || crc.sum() != e.crc {
        return Err(damaged("a ZIP entry fails its checksum"));
    }
    Ok(out)
}

/// Text of `w:t` runs, with tabs and breaks, one line per paragraph. Names
/// match by local part so `m:t` and `a:t` text is read too; `w:delText` and
/// `w:instrText` (deleted text, field codes) are not body text.
fn body_text(xml: &[u8]) -> Res<String> {
    let xml =
        std::str::from_utf8(xml).map_err(|_| damaged("word/document.xml is not UTF-8 XML"))?;
    let (mut out, mut rest, mut in_text) = (String::new(), xml, false);
    while let Some(lt) = rest.find('<') {
        if in_text {
            unescape(&rest[..lt], &mut out)?;
        }
        rest = &rest[lt..];
        if let Some(cdata) = rest.strip_prefix("<![CDATA[") {
            let end = cdata
                .find("]]>")
                .ok_or_else(|| damaged("unterminated CDATA in word/document.xml"))?;
            if in_text {
                out.push_str(&cdata[..end]);
            }
            rest = &cdata[end + 3..];
            continue;
        }
        let skip = if rest.starts_with("<!--") {
            rest.find("-->").map(|i| i + 3)
        } else {
            tag_end(rest).map(|i| i + 1)
        }
        .ok_or_else(|| damaged("word/document.xml is cut short inside a tag"))?;
        let tag = &rest[1..skip - 1];
        rest = &rest[skip..];
        let (closing, tag) = tag.strip_prefix('/').map_or((false, tag), |t| (true, t));
        let selfclosing = tag.ends_with('/');
        let name = tag
            .split(|c: char| c.is_whitespace() || c == '/')
            .next()
            .unwrap_or("");
        let local = name.rsplit(':').next().unwrap_or(name);
        match (local, closing) {
            ("t", false) => in_text = !selfclosing,
            ("t", true) => in_text = false,
            ("tab", false) => out.push('\t'),
            ("br" | "cr", false) => out.push('\n'),
            ("noBreakHyphen", false) => out.push('-'),
            ("p", true) => out.push('\n'),
            _ => {}
        }
        if out.len() > super::MAX_TEXT_BYTES * 2 {
            return Err(Stop::Limit(
                "the document text is far over the extraction limit".into(),
            ));
        }
    }
    Ok(out.trim_end().to_owned())
}
/// Index of the `>` ending the tag at the start of `s`, outside quotes.
fn tag_end(s: &str) -> Option<usize> {
    let mut quote = None;
    for (i, c) in s.char_indices() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, '>') => return Some(i),
            _ => {}
        }
    }
    None
}
fn unescape(s: &str, out: &mut String) -> Res<()> {
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let end = rest[amp..]
            .find(';')
            .ok_or_else(|| damaged("a character reference is unterminated"))?;
        let name = &rest[amp + 1..amp + end];
        let c = match name {
            "lt" => '<',
            "gt" => '>',
            "amp" => '&',
            "quot" => '"',
            "apos" => '\'',
            _ => name
                .strip_prefix('#')
                .and_then(|n| match n.strip_prefix(['x', 'X']) {
                    Some(h) => u32::from_str_radix(h, 16).ok(),
                    None => n.parse().ok(),
                })
                .and_then(char::from_u32)
                .ok_or_else(|| damaged("a character reference is invalid"))?,
        };
        out.push(c);
        rest = &rest[amp + end + 1..];
    }
    out.push_str(rest);
    Ok(())
}
