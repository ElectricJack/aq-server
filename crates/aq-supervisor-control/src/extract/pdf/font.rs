use super::document::Doc;
use super::object::{Dict, Lex, Obj};
use super::{Budget, MAX_CMAP_ENTRIES, Res, Stop};
use std::{collections::BTreeMap, rc::Rc};

/// Unicode mapping of one font.
pub(super) enum Font {
    /// Codes of `width` bytes through the `ToUnicode` CMap.
    Mapped {
        width: usize,
        map: BTreeMap<u32, String>,
    },
    /// Single bytes read as Latin-1.
    Latin1,
    /// No way to map the codes to Unicode.
    Unmapped { width: usize },
}

fn too_big() -> Stop {
    Stop::Limit("a PDF font mapping is over the size limit".into())
}

fn utf16(bytes: &[u8]) -> String {
    char::decode_utf16(
        bytes
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]])),
    )
    .map(|r| r.unwrap_or('\u{FFFD}'))
    .collect()
}

fn code(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0u32, |a, &x| a << 8 | x as u32)
}

#[derive(Default)]
struct CharacterMap {
    width: usize,
    map: BTreeMap<u32, String>,
}

impl CharacterMap {
    fn apply(&mut self, word: &[u8], ops: &[Obj]) -> Res<()> {
        match word {
            b"endcodespacerange" => self.code_space(ops),
            b"endbfchar" => self.characters(ops),
            b"endbfrange" => {
                for triple in ops.chunks_exact(3) {
                    self.range(triple);
                }
            }
            _ => {}
        }
        if self.map.len() > MAX_CMAP_ENTRIES {
            return Err(too_big());
        }
        Ok(())
    }

    fn code_space(&mut self, ops: &[Obj]) {
        for pair in ops.chunks_exact(2) {
            if let Obj::Str(low) = &pair[0] {
                self.width = self.width.max(low.len());
            }
        }
    }

    fn characters(&mut self, ops: &[Obj]) {
        for pair in ops.chunks_exact(2) {
            if let (Obj::Str(src), Obj::Str(dst)) = (&pair[0], &pair[1]) {
                self.map.insert(code(src), utf16(dst));
            }
        }
    }

    fn range(&mut self, triple: &[Obj]) {
        let (Obj::Str(lo), Obj::Str(hi)) = (&triple[0], &triple[1]) else {
            return;
        };
        let (lo, hi) = (code(lo), code(hi));
        if hi < lo || hi - lo > 0xffff {
            return;
        }
        match &triple[2] {
            Obj::Str(dst) => self.sequential(lo, hi, dst),
            Obj::Array(list) => {
                for (k, obj) in list.iter().enumerate() {
                    if let Obj::Str(dst) = obj {
                        self.map.insert(lo + k as u32, utf16(dst));
                    }
                }
            }
            _ => {}
        }
    }

    fn sequential(&mut self, lo: u32, hi: u32, dst: &[u8]) {
        let mut prefix = utf16(dst);
        let Some(last) = prefix.pop() else { return };
        for k in 0..=(hi - lo) {
            let mut s = prefix.clone();
            s.push(char::from_u32(last as u32 + k).unwrap_or('\u{FFFD}'));
            self.map.insert(lo + k, s);
        }
    }

    fn font(self, two_byte: bool) -> Font {
        let default = if two_byte { 2 } else { 1 };
        let width = if self.width == 0 {
            default
        } else {
            self.width.min(4)
        };
        Font::Mapped {
            width,
            map: self.map,
        }
    }
}

fn parse_cmap(data: &[u8], two_byte: bool) -> Res<Font> {
    let mut lex = Lex::new(data, 0);
    let mut map = CharacterMap::default();
    let mut ops = Vec::new();
    loop {
        if let Some(obj) = lex.object(0) {
            ops.push(obj);
            if ops.len() > 1_000_000 {
                return Err(too_big());
            }
            continue;
        }
        lex.skip_ws();
        if lex.i >= data.len() {
            return Ok(map.font(two_byte));
        }
        let word = lex.word();
        if word.is_empty() {
            lex.i += 1;
            continue;
        }
        map.apply(word, &ops)?;
        ops.clear();
    }
}

/// Fonts of one resource dictionary, by resource name.
pub(super) fn fonts(
    doc: &Doc<'_>,
    resources: Option<&Obj>,
    cache: &mut BTreeMap<u32, Rc<Font>>,
    budget: &mut Budget,
) -> Res<BTreeMap<String, Rc<Font>>> {
    let mut out = BTreeMap::new();
    let Some(list) = resources
        .and_then(|r| doc.dict(r))
        .and_then(|r| r.get("Font"))
        .and_then(|f| doc.dict(f))
    else {
        return Ok(out);
    };
    for (name, entry) in list {
        let key = match entry {
            Obj::Ref(n) => Some(*n),
            _ => None,
        };
        if let Some(font) = key.and_then(|k| cache.get(&k)) {
            out.insert(name.clone(), font.clone());
            continue;
        }
        let Some(d) = doc.dict(entry) else { continue };
        let font = read_font(doc, d, budget)?;
        let font = Rc::new(font);
        if let Some(k) = key {
            cache.insert(k, font.clone());
        }
        out.insert(name.clone(), font);
    }
    Ok(out)
}

fn read_font(doc: &Doc<'_>, d: &Dict, budget: &mut Budget) -> Res<Font> {
    let subtype = d.get("Subtype").and_then(Obj::name).unwrap_or("");
    let two_byte = subtype == "Type0";
    Ok(match d.get("ToUnicode") {
        Some(Obj::Ref(n)) => parse_cmap(&doc.stream(*n, budget)?, two_byte)?,
        _ => {
            let custom = doc
                .dict(d.get("Encoding").unwrap_or(&Obj::Null))
                .is_some_and(|e| e.contains_key("Differences"));
            if two_byte {
                Font::Unmapped { width: 2 }
            } else if custom || subtype == "Type3" {
                Font::Unmapped { width: 1 }
            } else {
                Font::Latin1
            }
        }
    })
}
