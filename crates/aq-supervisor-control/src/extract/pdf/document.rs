use super::object::{Dict, Lex, Obj, is_regular, is_ws};
use super::{Budget, MAX_DEPTH, MAX_OBJECTS, MAX_PAGES, Res, Stop, find, unreadable};
use std::collections::{BTreeMap, HashSet};

struct Stored {
    obj: Obj,
    /// Raw stream bytes in the file, for a stream object.
    stream: Option<(usize, usize)>,
}
pub(super) struct Doc<'a> {
    bytes: &'a [u8],
    objs: BTreeMap<u32, Stored>,
    roots: Vec<u32>,
    encrypted: bool,
}

impl<'a> Doc<'a> {
    pub(super) fn load(bytes: &'a [u8], budget: &mut Budget) -> Res<Self> {
        let mut doc = Doc {
            bytes,
            objs: BTreeMap::new(),
            roots: Vec::new(),
            encrypted: false,
        };
        doc.scan_objects()?;
        doc.read_trailers();
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
    fn scan_objects(&mut self) -> Res<()> {
        let mut at = 0;
        while let Some(rel) = find(&self.bytes[at..], b"obj") {
            let k = at + rel;
            at = k + 3;
            let Some(num) = object_header(self.bytes, k) else {
                continue;
            };
            self.check_capacity()?;
            let mut lex = Lex::new(self.bytes, at);
            let Some(obj) = lex.object(0) else { continue };
            lex.skip_ws();
            let stream = stream_span(self.bytes, lex.i, &obj)?;
            at = stream.map_or(lex.i.max(at), |(_, end)| end);
            self.objs.insert(num, Stored { obj, stream });
        }
        Ok(())
    }

    fn check_capacity(&self) -> Res<()> {
        if self.objs.len() >= MAX_OBJECTS {
            return Err(Stop::Limit(format!(
                "the PDF has more than {MAX_OBJECTS} objects"
            )));
        }
        Ok(())
    }

    fn read_trailers(&mut self) {
        let bytes = self.bytes;
        // Trailer dictionaries and cross-reference stream dictionaries.
        let mut at = 0;
        while let Some(rel) = find(&bytes[at..], b"trailer") {
            at += rel + 7;
            if let Some(Obj::Dict(d)) = Lex::new(bytes, at).object(0) {
                self.note_trailer(&d);
            }
        }
        self.read_xref_dictionaries();
    }

    fn read_xref_dictionaries(&mut self) {
        let xref: Vec<Dict> = self
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
            self.note_trailer(d);
        }
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
            let (count, first) = packed_header(d)?;
            for (num, off) in packed_offsets(&data, count)? {
                let (Ok(num), Some(at)) = (u32::try_from(num), first.checked_add(off as usize))
                else {
                    continue;
                };
                self.check_capacity()?;
                if let Some(obj) = Lex::new(&data, at).object(0) {
                    self.objs.entry(num).or_insert(Stored { obj, stream: None });
                }
            }
        }
        Ok(())
    }
    pub(super) fn resolve<'b>(&'b self, mut o: &'b Obj) -> &'b Obj {
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
    pub(super) fn dict_of(&self, n: u32) -> Option<&Dict> {
        match &self.objs.get(&n)?.obj {
            Obj::Dict(d) => Some(d),
            _ => None,
        }
    }
    pub(super) fn dict<'b>(&'b self, o: &'b Obj) -> Option<&'b Dict> {
        match self.resolve(o) {
            Obj::Dict(d) => Some(d),
            _ => None,
        }
    }
    /// Decoded content of stream object `n`.
    pub(super) fn stream(&self, n: u32, budget: &mut Budget) -> Res<Vec<u8>> {
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
    pub(super) fn pages(&self) -> Vec<(u32, Option<&Obj>)> {
        let mut tree = PageTree::default();
        for root in self.roots.iter().rev() {
            let Some(catalog) = self.dict_of(*root) else {
                continue;
            };
            if let Some(Obj::Ref(p)) = catalog.get("Pages") {
                tree.walk(self, *p, None);
            }
            if !tree.pages.is_empty() {
                return tree.pages;
            }
        }
        // No usable page tree: every page object, in object order.
        for (n, s) in &self.objs {
            if matches!(&s.obj, Obj::Dict(d) if d.get("Type").and_then(Obj::name) == Some("Page")) {
                tree.pages.push((*n, None));
            }
        }
        tree.pages
    }
}

#[derive(Default)]
struct PageTree<'a> {
    seen: HashSet<u32>,
    pages: Vec<(u32, Option<&'a Obj>)>,
}

impl<'a> PageTree<'a> {
    fn walk(&mut self, doc: &'a Doc<'_>, n: u32, inherited: Option<&'a Obj>) {
        self.walk_at(doc, (n, inherited), 0);
    }

    fn walk_at(&mut self, doc: &'a Doc<'_>, node: (u32, Option<&'a Obj>), depth: usize) {
        let (n, inherited) = node;
        if !self.visit(n, depth) {
            return;
        }
        let Some(d) = doc.dict_of(n) else { return };
        let resources = d.get("Resources").or(inherited);
        if let Obj::Array(kids) = doc.resolve(d.get("Kids").unwrap_or(&Obj::Null)) {
            for child in kids {
                if let Obj::Ref(k) = child {
                    self.walk_at(doc, (*k, resources), depth + 1);
                }
            }
        } else {
            self.pages.push((n, resources));
        }
    }

    fn visit(&mut self, n: u32, depth: usize) -> bool {
        depth <= MAX_DEPTH && self.pages.len() < MAX_PAGES && self.seen.insert(n)
    }
}

fn packed_header(d: &Dict) -> Res<(usize, usize)> {
    match (d.get("N"), d.get("First")) {
        (Some(Obj::Int(c)), Some(Obj::Int(f))) if *c >= 0 && *f >= 0 => {
            Ok((*c as usize, *f as usize))
        }
        _ => Err(unreadable("an object stream header is invalid")),
    }
}

fn packed_offsets(data: &[u8], count: usize) -> Res<Vec<(i64, i64)>> {
    let mut head = Lex::new(data, 0);
    let mut found = Vec::new();
    for _ in 0..count.min(MAX_OBJECTS) {
        let (Some(Obj::Int(num)), Some(Obj::Int(off))) = (head.object(0), head.object(0)) else {
            return Err(unreadable("an object stream index is invalid"));
        };
        found.push((num, off));
    }
    Ok(found)
}

fn stream_length(obj: &Obj) -> Option<usize> {
    let Obj::Dict(d) = obj else { return None };
    let Obj::Int(n) = d.get("Length")? else {
        return None;
    };
    usize::try_from(*n).ok()
}

fn stream_span(bytes: &[u8], pos: usize, obj: &Obj) -> Res<Option<(usize, usize)>> {
    if !bytes[pos..].starts_with(b"stream") {
        return Ok(None);
    }
    let mut from = pos + 6;
    if bytes.get(from) == Some(&b'\r') {
        from += 1;
    }
    if bytes.get(from) == Some(&b'\n') {
        from += 1;
    }
    let end = stream_length(obj)
        .and_then(|length| from.checked_add(length))
        .filter(|&end| has_endstream(bytes, end))
        .or_else(|| scan_stream_end(bytes, from))
        .ok_or_else(|| unreadable("a stream has no end"))?;
    Ok(Some((from, end)))
}

fn has_endstream(bytes: &[u8], end: usize) -> bool {
    let mut lex = Lex::new(bytes, end);
    lex.skip_ws();
    bytes[lex.i.min(bytes.len())..].starts_with(b"endstream")
}

fn scan_stream_end(bytes: &[u8], from: usize) -> Option<usize> {
    let raw = &bytes[from..];
    let body = &raw[..find(raw, b"endstream")?];
    let body = body.strip_suffix(b"\n").unwrap_or(body);
    let body = body.strip_suffix(b"\r").unwrap_or(body);
    Some(from + body.len())
}

/// Object header N G obj; ignore the suffix of endobj or a longer keyword.
fn object_header(bytes: &[u8], at: usize) -> Option<u32> {
    if !at.checked_sub(1).is_some_and(|prev| is_ws(bytes[prev])) {
        return None;
    }
    if bytes.get(at + 3).is_some_and(|&b| is_regular(b)) {
        return None;
    }
    let (before_generation, _) = previous_number(&bytes[..at])?;
    let (_, number) = previous_number(before_generation)?;
    std::str::from_utf8(number).ok()?.parse().ok()
}

fn previous_number(prefix: &[u8]) -> Option<(&[u8], &[u8])> {
    let end = prefix.iter().rposition(|&c| !is_ws(c))? + 1;
    let field = &prefix[..end];
    let start = field
        .iter()
        .rposition(|c| !c.is_ascii_digit())
        .map_or(0, |i| i + 1);
    if start == end || start.checked_sub(1).is_some_and(|i| is_regular(field[i])) {
        return None;
    }
    Some((&prefix[..start], &field[start..]))
}
