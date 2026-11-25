use super::MAX_DEPTH;
use std::collections::BTreeMap;

pub(super) type Dict = BTreeMap<String, Obj>;
#[derive(Clone, Debug)]
pub(super) enum Obj {
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
    pub(super) fn num(&self) -> Option<f64> {
        match self {
            Self::Int(i) => Some(*i as f64),
            Self::Real(r) => Some(*r),
            _ => None,
        }
    }
    pub(super) fn name(&self) -> Option<&str> {
        match self {
            Self::Name(n) => Some(n),
            _ => None,
        }
    }
}

pub(super) fn is_ws(b: u8) -> bool {
    matches!(b, 0 | 9 | 10 | 12 | 13 | 32)
}
fn is_delim(b: u8) -> bool {
    b"()<>[]{}/%".contains(&b)
}
pub(super) fn is_regular(b: u8) -> bool {
    !is_ws(b) && !is_delim(b)
}

pub(super) struct Lex<'a> {
    b: &'a [u8],
    pub(super) i: usize,
}
impl<'a> Lex<'a> {
    pub(super) fn new(b: &'a [u8], i: usize) -> Self {
        Self { b, i }
    }
    pub(super) fn skip_ws(&mut self) {
        while let Some(&c) = self.b.get(self.i) {
            if is_ws(c) {
                self.i += 1;
            } else if c == b'%' {
                self.skip_comment();
            } else {
                break;
            }
        }
    }
    fn skip_comment(&mut self) {
        while self
            .b
            .get(self.i)
            .is_some_and(|&c| c != b'\n' && c != b'\r')
        {
            self.i += 1;
        }
    }

    /// A run of regular characters (a number, keyword or operator).
    pub(super) fn word(&mut self) -> &'a [u8] {
        let from = self.i;
        while self.b.get(self.i).is_some_and(|&c| is_regular(c)) {
            self.i += 1;
        }
        &self.b[from..self.i]
    }
    /// One object, or `None` at an operator, a stray delimiter or the end.
    /// The position is left on a `None` operator so the caller can read it.
    pub(super) fn object(&mut self, depth: usize) -> Option<Obj> {
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
            b'<' if self.b.get(self.i + 1) == Some(&b'<') => self.dictionary(depth),
            b'<' => Some(Obj::Str(self.hex())),
            b'[' => self.array(depth),
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
    fn dictionary(&mut self, depth: usize) -> Option<Obj> {
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

    fn array(&mut self, depth: usize) -> Option<Obj> {
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
            return self.integer(n);
        }
        match text.parse::<f64>() {
            Ok(r) if r.is_finite() => Some(Obj::Real(r)),
            _ => {
                self.i = at;
                None
            }
        }
    }
    fn integer(&mut self, n: i64) -> Option<Obj> {
        if n >= 0 && self.read_reference_suffix() {
            u32::try_from(n).ok().map(Obj::Ref)
        } else {
            Some(Obj::Int(n))
        }
    }

    fn read_reference_suffix(&mut self) -> bool {
        let save = self.i;
        self.skip_ws();
        let generation = self.word();
        if !generation.is_empty() && generation.iter().all(u8::is_ascii_digit) {
            self.skip_ws();
            if self.b.get(self.i) == Some(&b'R')
                && !self.b.get(self.i + 1).is_some_and(|&c| is_regular(c))
            {
                self.i += 1;
                return true;
            }
        }
        self.i = save;
        false
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
                    self.escape(e, &mut out);
                }
                _ => out.push(c),
            }
        }
        out
    }
    fn octal(&mut self, e: u8) -> u8 {
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
        v as u8
    }

    fn escape(&mut self, e: u8, out: &mut Vec<u8>) {
        match e {
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'b' => out.push(8),
            b'f' => out.push(12),
            b'0'..=b'7' => {
                out.push(self.octal(e));
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

    fn hex(&mut self) -> Vec<u8> {
        self.i += 1;
        let (mut out, mut half) = (Vec::new(), None);
        while let Some(&c) = self.b.get(self.i) {
            self.i += 1;
            if c == b'>' {
                break;
            }
            let Some(d) = (c as char).to_digit(16) else {
                continue;
            };
            match half.take() {
                Some(h) => out.push((h << 4 | d) as u8),
                None => half = Some(d),
            }
        }
        if let Some(h) = half {
            out.push((h << 4) as u8);
        }
        out
    }
}
