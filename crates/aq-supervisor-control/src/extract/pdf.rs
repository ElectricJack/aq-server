//! PDF text without a PDF library: objects are found by scanning, page order
//! follows the page tree, and text comes from the content streams' text
//! operators. Fonts map through their `ToUnicode` CMap; a simple font without
//! one reads as Latin-1 unless it has custom glyph names, and a font that
//! cannot be mapped yields U+FFFD, counted in the returned note. Not handled:
//! encrypted files (reported), non-Flate stream filters (reported), OCR.
use super::{Budget, Limits, Res, Stop};
use std::collections::{BTreeMap, HashSet};
use std::rc::Rc;

const MAX_OBJECTS: usize = 200_000;
const MAX_DEPTH: usize = 32;
const MAX_CMAP_ENTRIES: usize = 200_000;
const MAX_PAGES: usize = 10_000;

type Dict = BTreeMap<String, Obj>;
#[derive(Clone, Debug)]
enum Obj {
    Null,
    Int(i64),
    Real(f64),
    Name(String),
    Str(Vec<u8>),
    Array(Vec<Obj>),
    Dict(Dict),
    Ref(u32),
}
impl Obj {
    fn num(&self) -> Option<f64> {
        match self {
            Self::Int(i) => Some(*i as f64),
            Self::Real(r) => Some(*r),
            _ => None,
        }
    }
    fn name(&self) -> Option<&str> {
        match self {
            Self::Name(n) => Some(n),
            _ => None,
        }
    }
}

fn is_ws(b: u8) -> bool {
    matches!(b, 0 | 9 | 10 | 12 | 13 | 32)
}
fn is_delim(b: u8) -> bool {
    b"()<>[]{}/%".contains(&b)
}
fn is_regular(b: u8) -> bool {
    !is_ws(b) && !is_delim(b)
}

struct Lex<'a> {
    b: &'a [u8],
    i: usize,
}
impl<'a> Lex<'a> {
    fn new(b: &'a [u8], i: usize) -> Self {
        Self { b, i }
    }
    fn skip_ws(&mut self) {
        while let Some(&c) = self.b.get(self.i) {
            if is_ws(c) {
                self.i += 1;
            } else if c == b'%' {
                while self
                    .b
                    .get(self.i)
                    .is_some_and(|&c| c != b'\n' && c != b'\r')
                {
                    self.i += 1;
                }
            } else {
                break;
            }
        }
    }
    /// A run of regular characters (a number, keyword or operator).
    fn word(&mut self) -> &'a [u8] {
        let from = self.i;
        while self.b.get(self.i).is_some_and(|&c| is_regular(c)) {
            self.i += 1;
        }
        &self.b[from..self.i]
    }
    /// One object, or `None` at an operator, a stray delimiter or the end.
    /// The position is left on a `None` operator so the caller can read it.
    fn object(&mut self, depth: usize) -> Option<Obj> {
        self.skip_ws();
        if depth > MAX_DEPTH {
            return None;
        }
        let c = *self.b.get(self.i)?;
        match c {
            b'/' => {
                self.i += 1;
                Some(Obj::Name(self.name()))
            }
            b'(' => Some(Obj::Str(self.literal())),
            b'<' if self.b.get(self.i + 1) == Some(&b'<') => {
                self.i += 2;
                let mut d = Dict::new();
                loop {
                    self.skip_ws();
                    if self.b[self.i..].starts_with(b">>") {
                        self.i += 2;
                        return Some(Obj::Dict(d));
                    }
                    if self.b.get(self.i) != Some(&b'/') {
                        return None;
                    }
                    self.i += 1;
                    let key = self.name();
                    let value = self.object(depth + 1)?;
                    d.insert(key, value);
                }
            }
            b'<' => Some(Obj::Str(self.hex())),
            b'[' => {
                self.i += 1;
                let mut v = Vec::new();
                loop {
                    self.skip_ws();
                    if self.b.get(self.i) == Some(&b']') {
                        self.i += 1;
                        return Some(Obj::Array(v));
                    }
                    v.push(self.object(depth + 1)?);
                }
            }
            b'+' | b'-' | b'.' | b'0'..=b'9' => self.number(),
            _ if is_regular(c) => {
                let at = self.i;
                match self.word() {
                    b"true" | b"false" | b"null" => Some(Obj::Null),
                    _ => {
                        self.i = at;
                        None
                    }
                }
            }
            _ => None,
        }
    }
    fn name(&mut self) -> String {
        let raw = self.word();
        let mut out = Vec::with_capacity(raw.len());
        let mut k = 0;
        while k < raw.len() {
            let hex = (raw[k] == b'#')
                .then(|| raw.get(k + 1..k + 3))
                .flatten()
                .and_then(|h| std::str::from_utf8(h).ok())
                .and_then(|h| u8::from_str_radix(h, 16).ok());
            match hex {
                Some(v) => {
                    out.push(v);
                    k += 3;
                }
                None => {
                    out.push(raw[k]);
                    k += 1;
                }
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }
    fn number(&mut self) -> Option<Obj> {
        let at = self.i;
        let w = self.word();
        let text = std::str::from_utf8(w).ok()?;
        if let Ok(n) = text.parse::<i64>() {
            // `N G R` is a reference.
            if n >= 0 {
                let save = self.i;
                self.skip_ws();
                let g = self.word();
                if !g.is_empty() && g.iter().all(u8::is_ascii_digit) {
                    self.skip_ws();
                    if self.b.get(self.i) == Some(&b'R')
                        && !self.b.get(self.i + 1).is_some_and(|&c| is_regular(c))
                    {
                        self.i += 1;
                        return u32::try_from(n).ok().map(Obj::Ref);
                    }
                }
                self.i = save;
            }
            return Some(Obj::Int(n));
        }
        match text.parse::<f64>() {
            Ok(r) if r.is_finite() => Some(Obj::Real(r)),
            _ => {
                self.i = at;
                None
            }
        }
    }
    fn literal(&mut self) -> Vec<u8> {
        self.i += 1;
        let (mut out, mut depth) = (Vec::new(), 1);
        while let Some(&c) = self.b.get(self.i) {
            self.i += 1;
            match c {
                b'(' => {
                    depth += 1;
                    out.push(c);
                }
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                    out.push(c);
                }
                b'\\' => {
                    let Some(&e) = self.b.get(self.i) else { break };
                    self.i += 1;
                    match e {
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'0'..=b'7' => {
                            let mut v = (e - b'0') as u32;
                            for _ in 0..2 {
                                match self.b.get(self.i) {
                                    Some(&d @ b'0'..=b'7') => {
                                        v = v * 8 + (d - b'0') as u32;
                                        self.i += 1;
                                    }
                                    _ => break,
                                }
                            }
                            out.push(v as u8);
                        }
                        b'\r' => {
                            if self.b.get(self.i) == Some(&b'\n') {
                                self.i += 1;
                            }
                        }
                        b'\n' => {}
                        other => out.push(other),
                    }
                }
                _ => out.push(c),
            }
        }
        out
    }
    fn hex(&mut self) -> Vec<u8> {
        self.i += 1;
        let (mut out, mut half) = (Vec::new(), None);
        while let Some(&c) = self.b.get(self.i) {
            self.i += 1;
            if c == b'>' {
                break;
            }
            if let Some(d) = (c as char).to_digit(16) {
                match half.take() {
                    Some(h) => out.push((h << 4 | d) as u8),
                    None => half = Some(d),
                }
            }
        }
        if let Some(h) = half {
            out.push((h << 4) as u8);
        }
        out
    }
}

struct Stored {
    obj: Obj,
    /// Raw stream bytes in the file, for a stream object.
    stream: Option<(usize, usize)>,
}
struct Doc<'a> {
    bytes: &'a [u8],
    objs: BTreeMap<u32, Stored>,
    roots: Vec<u32>,
    encrypted: bool,
}

fn unreadable(why: &str) -> Stop {
    Stop::Unreadable(format!(
        "the PDF is damaged or truncated ({why}); upload the file again or re-export it"
    ))
}

impl<'a> Doc<'a> {
    fn load(bytes: &'a [u8], budget: &mut Budget) -> Res<Self> {
        let mut doc = Doc {
            bytes,
            objs: BTreeMap::new(),
            roots: Vec::new(),
            encrypted: false,
        };
        let mut at = 0;
        while let Some(rel) = find(&bytes[at..], b"obj") {
            let k = at + rel;
            at = k + 3;
            // `N G obj`, with `obj` not the tail of `endobj`.
            if k == 0 || !is_ws(bytes[k - 1]) || bytes.get(k + 3).is_some_and(|&c| is_regular(c)) {
                continue;
            }
            let Some(num) = header_number(bytes, k) else {
                continue;
            };
            if doc.objs.len() >= MAX_OBJECTS {
                return Err(Stop::Limit(format!(
                    "the PDF has more than {MAX_OBJECTS} objects"
                )));
            }
            let mut lex = Lex::new(bytes, k + 3);
            let Some(obj) = lex.object(0) else { continue };
            lex.skip_ws();
            let mut stream = None;
            if bytes[lex.i..].starts_with(b"stream") {
                let mut from = lex.i + 6;
                if bytes.get(from) == Some(&b'\r') {
                    from += 1;
                }
                if bytes.get(from) == Some(&b'\n') {
                    from += 1;
                }
                let len = match &obj {
                    Obj::Dict(d) => match d.get("Length") {
                        Some(Obj::Int(n)) => usize::try_from(*n).ok(),
                        _ => None,
                    },
                    _ => None,
                };
                let to = len
                    .filter(|&n| {
                        from.checked_add(n).is_some_and(|e| {
                            let mut t = Lex::new(bytes, e);
                            t.skip_ws();
                            bytes[t.i.min(bytes.len())..].starts_with(b"endstream")
                        })
                    })
                    .map(|n| from + n)
                    .or_else(|| {
                        find(&bytes[from.min(bytes.len())..], b"endstream").map(|r| {
                            let mut e = from + r;
                            if e > from && bytes[e - 1] == b'\n' {
                                e -= 1;
                            }
                            if e > from && bytes[e - 1] == b'\r' {
                                e -= 1;
                            }
                            e
                        })
                    })
                    .ok_or_else(|| unreadable("a stream has no end"))?;
                stream = Some((from, to));
                at = to;
            } else {
                at = lex.i.max(at);
            }
            doc.objs.insert(num, Stored { obj, stream });
        }
        // Trailer dictionaries and cross-reference stream dictionaries.
        let mut at = 0;
        while let Some(rel) = find(&bytes[at..], b"trailer") {
            at += rel + 7;
            if let Some(Obj::Dict(d)) = Lex::new(bytes, at).object(0) {
                doc.note_trailer(&d);
            }
        }
        let xref: Vec<Dict> = doc
            .objs
            .values()
            .filter_map(|s| match &s.obj {
                Obj::Dict(d) if d.get("Type").and_then(Obj::name) == Some("XRef") => {
                    Some(d.clone())
                }
                _ => None,
            })
            .collect();
        for d in &xref {
            doc.note_trailer(d);
        }
        if doc.encrypted {
            return Err(Stop::Unreadable(
                "the PDF is encrypted or password-protected; remove the protection \
                 and upload it again"
                    .into(),
            ));
        }
        doc.expand_object_streams(budget)?;
        Ok(doc)
    }
    fn note_trailer(&mut self, d: &Dict) {
        self.encrypted |= d.contains_key("Encrypt");
        if let Some(Obj::Ref(r)) = d.get("Root") {
            self.roots.push(*r);
        }
    }
    /// Objects packed into `/Type /ObjStm` streams (PDF 1.5).
    fn expand_object_streams(&mut self, budget: &mut Budget) -> Res<()> {
        let packs: Vec<u32> = self
            .objs
            .iter()
            .filter(|(_, s)| {
                s.stream.is_some()
                    && matches!(&s.obj, Obj::Dict(d)
                        if d.get("Type").and_then(Obj::name) == Some("ObjStm"))
            })
            .map(|(n, _)| *n)
            .collect();
        for n in packs {
            let data = self.stream(n, budget)?;
            let Some(Obj::Dict(d)) = self.objs.get(&n).map(|s| &s.obj) else {
                continue;
            };
            let (count, first) = match (d.get("N"), d.get("First")) {
                (Some(Obj::Int(c)), Some(Obj::Int(f))) if *c >= 0 && *f >= 0 => {
                    (*c as usize, *f as usize)
                }
                _ => return Err(unreadable("an object stream header is invalid")),
            };
            let mut head = Lex::new(&data, 0);
            let mut found = Vec::new();
            for _ in 0..count.min(MAX_OBJECTS) {
                let (Some(Obj::Int(num)), Some(Obj::Int(off))) = (head.object(0), head.object(0))
                else {
                    return Err(unreadable("an object stream index is invalid"));
                };
                found.push((num, off));
            }
            for (num, off) in found {
                let (Ok(num), Some(at)) = (u32::try_from(num), first.checked_add(off as usize))
                else {
                    continue;
                };
                if self.objs.len() >= MAX_OBJECTS {
                    return Err(Stop::Limit(format!(
                        "the PDF has more than {MAX_OBJECTS} objects"
                    )));
                }
                if let Some(obj) = Lex::new(&data, at).object(0) {
                    self.objs.entry(num).or_insert(Stored { obj, stream: None });
                }
            }
        }
        Ok(())
    }
    fn resolve<'b>(&'b self, mut o: &'b Obj) -> &'b Obj {
        for _ in 0..8 {
            match o {
                Obj::Ref(n) => match self.objs.get(n) {
                    Some(s) => o = &s.obj,
                    None => return &Obj::Null,
                },
                _ => return o,
            }
        }
        &Obj::Null
    }
    /// The dictionary object `n`.
    fn dict_of(&self, n: u32) -> Option<&Dict> {
        match &self.objs.get(&n)?.obj {
            Obj::Dict(d) => Some(d),
            _ => None,
        }
    }
    fn dict<'b>(&'b self, o: &'b Obj) -> Option<&'b Dict> {
        match self.resolve(o) {
            Obj::Dict(d) => Some(d),
            _ => None,
        }
    }
    /// Decoded content of stream object `n`.
    fn stream(&self, n: u32, budget: &mut Budget) -> Res<Vec<u8>> {
        let s = self.objs.get(&n);
        let Some(Stored {
            obj,
            stream: Some((a, b)),
        }) = s
        else {
            return Err(unreadable("a referenced stream is missing"));
        };
        let raw = &self.bytes[*a..*b];
        let Obj::Dict(d) = obj else { unreachable!() };
        let filters: Vec<String> = match d.get("Filter").map(|f| self.resolve(f)) {
            None | Some(Obj::Null) => vec![],
            Some(Obj::Name(n)) => vec![n.clone()],
            Some(Obj::Array(a)) => a
                .iter()
                .map(|f| self.resolve(f).name().unwrap_or("?").to_owned())
                .collect(),
            _ => return Err(unreadable("a stream filter is invalid")),
        };
        let mut data = raw.to_vec();
        for f in &filters {
            match f.as_str() {
                "FlateDecode" | "Fl" => {
                    let predictor = d
                        .get("DecodeParms")
                        .and_then(|p| self.dict(p))
                        .and_then(|p| p.get("Predictor"))
                        .and_then(Obj::num);
                    if predictor.is_some_and(|p| p > 1.0) {
                        return Err(Stop::Failed(
                            "a PDF stream uses a Flate predictor, which is not supported".into(),
                        ));
                    }
                    data = budget.inflate(&data, true)?;
                }
                other => {
                    return Err(Stop::Failed(format!(
                        "a PDF stream uses the {other} filter, which is not supported"
                    )));
                }
            }
        }
        Ok(data)
    }
    /// Leaf pages in document order, each with its inherited resources.
    fn pages(&self) -> Vec<(u32, Option<&Obj>)> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for root in self.roots.iter().rev() {
            let Some(catalog) = self.dict_of(*root) else {
                continue;
            };
            if let Some(Obj::Ref(p)) = catalog.get("Pages") {
                self.walk(*p, None, 0, &mut seen, &mut out);
            }
            if !out.is_empty() {
                return out;
            }
        }
        // No usable page tree: every page object, in object order.
        for (n, s) in &self.objs {
            if matches!(&s.obj, Obj::Dict(d) if d.get("Type").and_then(Obj::name) == Some("Page")) {
                out.push((*n, None));
            }
        }
        out
    }
    fn walk<'b>(
        &'b self,
        n: u32,
        inherited: Option<&'b Obj>,
        depth: usize,
        seen: &mut HashSet<u32>,
        out: &mut Vec<(u32, Option<&'b Obj>)>,
    ) {
        if depth > MAX_DEPTH || out.len() >= MAX_PAGES || !seen.insert(n) {
            return;
        }
        let Some(d) = self.dict_of(n) else {
            return;
        };
        let resources = d.get("Resources").or(inherited);
        match self.resolve(d.get("Kids").unwrap_or(&Obj::Null)) {
            Obj::Array(kids) => {
                for k in kids {
                    if let Obj::Ref(k) = k {
                        self.walk(*k, resources, depth + 1, seen, out);
                    }
                }
            }
            _ => out.push((n, resources)),
        }
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}
/// The object number of the `N G obj` header whose `obj` starts at `k`.
fn header_number(b: &[u8], k: usize) -> Option<u32> {
    let mut i = k;
    let mut back = |digits: &mut Vec<u8>| {
        while i > 0 && is_ws(b[i - 1]) {
            i -= 1;
        }
        let end = i;
        while i > 0 && b[i - 1].is_ascii_digit() {
            i -= 1;
        }
        digits.extend_from_slice(&b[i..end]);
        i < end && (i == 0 || !is_regular(b[i - 1]))
    };
    let (mut generation, mut number) = (Vec::new(), Vec::new());
    if !back(&mut generation) {
        return None;
    }
    if !back(&mut number) {
        return None;
    }
    std::str::from_utf8(&number).ok()?.parse().ok()
}

/// Unicode mapping of one font.
enum Font {
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

fn parse_cmap(data: &[u8], two_byte: bool) -> Res<Font> {
    let (mut lex, mut width, mut map) = (Lex::new(data, 0), 0usize, BTreeMap::new());
    let mut ops: Vec<Obj> = Vec::new();
    let too_big = || Stop::Limit("a PDF font mapping is over the size limit".into());
    let utf16 = |b: &[u8]| {
        char::decode_utf16(b.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])))
            .map(|r| r.unwrap_or('\u{FFFD}'))
            .collect::<String>()
    };
    let code = |b: &[u8]| b.iter().fold(0u32, |a, &x| a << 8 | x as u32);
    loop {
        match lex.object(0) {
            Some(o) => {
                ops.push(o);
                if ops.len() > 1_000_000 {
                    return Err(too_big());
                }
            }
            None => {
                lex.skip_ws();
                if lex.i >= data.len() {
                    break;
                }
                let at = lex.i;
                let word = lex.word();
                if word.is_empty() {
                    lex.i = at + 1;
                    continue;
                }
                match word {
                    b"endcodespacerange" => {
                        for p in ops.chunks_exact(2) {
                            if let Obj::Str(lo) = &p[0] {
                                width = width.max(lo.len());
                            }
                        }
                    }
                    b"endbfchar" => {
                        for p in ops.chunks_exact(2) {
                            if let (Obj::Str(src), Obj::Str(dst)) = (&p[0], &p[1]) {
                                map.insert(code(src), utf16(dst));
                            }
                        }
                    }
                    b"endbfrange" => {
                        for t in ops.chunks_exact(3) {
                            let (Obj::Str(lo), Obj::Str(hi)) = (&t[0], &t[1]) else {
                                continue;
                            };
                            let (lo, hi) = (code(lo), code(hi));
                            if hi < lo || hi - lo > 0xffff {
                                continue;
                            }
                            match &t[2] {
                                Obj::Str(dst) => {
                                    let base = utf16(dst);
                                    let mut chars: Vec<char> = base.chars().collect();
                                    let Some(last) = chars.pop() else { continue };
                                    for k in 0..=(hi - lo) {
                                        let mut s: String = chars.iter().collect();
                                        s.push(
                                            char::from_u32(last as u32 + k).unwrap_or('\u{FFFD}'),
                                        );
                                        map.insert(lo + k, s);
                                    }
                                }
                                Obj::Array(list) => {
                                    for (k, d) in list.iter().enumerate() {
                                        if let Obj::Str(d) = d {
                                            map.insert(lo + k as u32, utf16(d));
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
                if map.len() > MAX_CMAP_ENTRIES {
                    return Err(too_big());
                }
                ops.clear();
            }
        }
    }
    let width = if width > 0 {
        width.min(4)
    } else if two_byte {
        2
    } else {
        1
    };
    Ok(Font::Mapped { width, map })
}

/// Fonts of one resource dictionary, by resource name.
fn fonts(
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
        let subtype = d.get("Subtype").and_then(Obj::name).unwrap_or("");
        let two_byte = subtype == "Type0";
        let font = match d.get("ToUnicode") {
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
        };
        let font = Rc::new(font);
        if let Some(k) = key {
            cache.insert(k, font.clone());
        }
        out.insert(name.clone(), font);
    }
    Ok(out)
}

struct Page<'a> {
    out: String,
    font: Option<Rc<Font>>,
    fonts: &'a BTreeMap<String, Rc<Font>>,
    last_y: Option<f64>,
    mapped: usize,
    unmapped: usize,
    limit: usize,
}
impl Page<'_> {
    fn newline(&mut self) {
        if !self.out.is_empty() && !self.out.ends_with('\n') {
            self.out.push('\n');
        }
    }
    fn space(&mut self) {
        if !self.out.is_empty() && !self.out.ends_with(char::is_whitespace) {
            self.out.push(' ');
        }
    }
    fn show(&mut self, s: &[u8]) -> Res<()> {
        let font = self.font.clone().unwrap_or_else(|| Rc::new(Font::Latin1));
        match &*font {
            Font::Latin1 => {
                for &b in s {
                    self.glyph(Some(b as char).filter(|c| !c.is_control() || *c == '\t'));
                }
            }
            Font::Mapped { width, map } => {
                for chunk in s.chunks(*width) {
                    let code = chunk.iter().fold(0u32, |a, &x| a << 8 | x as u32);
                    match map.get(&code) {
                        Some(t) => {
                            for c in t.chars() {
                                self.glyph(Some(c));
                            }
                        }
                        None => self.glyph(None),
                    }
                }
            }
            Font::Unmapped { width } => {
                for chunk in s.chunks(*width) {
                    if *width == 1 && chunk == b" " {
                        self.space();
                    } else {
                        self.glyph(None);
                    }
                }
            }
        }
        if self.out.len() > self.limit {
            return Err(Stop::Limit(
                "the PDF text is far over the extraction limit".into(),
            ));
        }
        Ok(())
    }
    fn glyph(&mut self, c: Option<char>) {
        match c {
            Some(c) => {
                if !c.is_whitespace() {
                    self.mapped += 1;
                }
                self.out.push(c);
            }
            None => {
                self.unmapped += 1;
                self.out.push('\u{FFFD}');
            }
        }
    }
}

/// Runs the text operators of one content stream.
fn interpret(data: &[u8], page: &mut Page<'_>) -> Res<()> {
    let mut lex = Lex::new(data, 0);
    let mut ops: Vec<Obj> = Vec::new();
    loop {
        match lex.object(0) {
            Some(o) => {
                ops.push(o);
                if ops.len() > 4096 {
                    ops.clear();
                }
                continue;
            }
            None => {
                lex.skip_ws();
                if lex.i >= data.len() {
                    return Ok(());
                }
            }
        }
        let at = lex.i;
        let op = lex.word();
        if op.is_empty() {
            // A stray delimiter such as `]` or `>`.
            lex.i = at + 1;
            ops.clear();
            continue;
        }
        let last_str = || {
            ops.iter().rev().find_map(|o| match o {
                Obj::Str(s) => Some(s.clone()),
                _ => None,
            })
        };
        let num = |k: usize| ops.get(k).and_then(Obj::num);
        match op {
            b"Tf" => {
                if let Some(Obj::Name(n)) = ops.first() {
                    page.font = page.fonts.get(n).cloned();
                }
            }
            b"Tj" => {
                if let Some(s) = last_str() {
                    page.show(&s)?;
                }
            }
            b"'" | b"\"" => {
                page.newline();
                if let Some(s) = last_str() {
                    page.show(&s)?;
                }
            }
            b"TJ" => {
                if let Some(Obj::Array(items)) = ops.last() {
                    for item in items {
                        match item {
                            Obj::Str(s) => page.show(s)?,
                            n => {
                                if n.num().is_some_and(|n| n < -200.0) {
                                    page.space();
                                }
                            }
                        }
                    }
                }
            }
            b"Td" | b"TD" => match num(1) {
                Some(ty) if ty != 0.0 => page.newline(),
                _ => page.space(),
            },
            b"T*" => page.newline(),
            b"Tm" => {
                let y = num(5);
                match (page.last_y, y) {
                    (Some(a), Some(b)) if (a - b).abs() > 0.5 => page.newline(),
                    (Some(_), _) => page.space(),
                    _ => {}
                }
                if y.is_some() {
                    page.last_y = y;
                }
            }
            b"ET" => page.newline(),
            b"BI" => {
                // Inline image: skip its binary data up to `EI`.
                let mut k = lex.i;
                loop {
                    let Some(rel) = find(&data[k.min(data.len())..], b"EI") else {
                        return Ok(());
                    };
                    let e = k + rel;
                    let before = e == 0 || is_ws(data[e - 1]);
                    let after = data.get(e + 2).is_none_or(|&c| is_ws(c));
                    k = e + 2;
                    if before && after {
                        break;
                    }
                }
                lex.i = k;
            }
            _ => {}
        }
        ops.clear();
    }
}

pub(super) fn extract(bytes: &[u8], limits: &Limits, budget: &mut Budget) -> Res<(String, String)> {
    if !bytes.starts_with(b"%PDF-") {
        return Err(unreadable("the %PDF- header is missing"));
    }
    if !bytes[bytes.len().saturating_sub(2048)..]
        .windows(5)
        .any(|w| w == b"%%EOF")
    {
        return Err(unreadable("the %%EOF marker is missing"));
    }
    let doc = Doc::load(bytes, budget)?;
    let pages = doc.pages();
    if pages.is_empty() {
        return Err(unreadable("it has no pages"));
    }
    let (mut cache, mut texts) = (BTreeMap::new(), Vec::new());
    let (mut mapped, mut unmapped) = (0, 0);
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
        let contents = match page.get("Contents") {
            Some(c) => match doc.resolve(c) {
                Obj::Array(list) => list.clone(),
                Obj::Null => vec![],
                _ => vec![c.clone()],
            },
            None => vec![],
        };
        let mut state = Page {
            out: String::new(),
            font: None,
            fonts: &fonts,
            last_y: None,
            mapped: 0,
            unmapped: 0,
            limit: limits.max_text_bytes.saturating_mul(2),
        };
        for c in &contents {
            let Obj::Ref(r) = c else { continue };
            let data = doc.stream(*r, budget)?;
            interpret(&data, &mut state)?;
            state.newline();
        }
        mapped += state.mapped;
        unmapped += state.unmapped;
        texts.push(state.out);
        if texts.iter().map(String::len).sum::<usize>() > limits.max_text_bytes.saturating_mul(2) {
            return Err(Stop::Limit(
                "the PDF text is far over the extraction limit".into(),
            ));
        }
    }
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
