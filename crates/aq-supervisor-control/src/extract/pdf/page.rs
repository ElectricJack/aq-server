use super::font::Font;
use super::object::{Lex, Obj, is_ws};
use super::{Res, Stop, find};
use std::{collections::BTreeMap, rc::Rc};

pub(super) struct Page<'a> {
    pub(super) out: String,
    pub(super) font: Option<Rc<Font>>,
    pub(super) fonts: &'a BTreeMap<String, Rc<Font>>,
    pub(super) last_y: Option<f64>,
    pub(super) mapped: usize,
    pub(super) unmapped: usize,
    pub(super) limit: usize,
}
impl Page<'_> {
    pub(super) fn newline(&mut self) {
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
            Font::Latin1 => self.show_latin1(s),
            Font::Mapped { width, map } => self.show_mapped(s, *width, map),
            Font::Unmapped { width } => self.show_unmapped(s, *width),
        }
        if self.out.len() > self.limit {
            return Err(Stop::Limit(
                "the PDF text is far over the extraction limit".into(),
            ));
        }
        Ok(())
    }
    fn show_latin1(&mut self, data: &[u8]) {
        for &b in data {
            self.glyph(Some(b as char).filter(|c| !c.is_control() || *c == '\t'));
        }
    }

    fn show_mapped(&mut self, data: &[u8], width: usize, map: &BTreeMap<u32, String>) {
        for chunk in data.chunks(width) {
            let code = chunk.iter().fold(0u32, |a, &x| a << 8 | x as u32);
            self.mapped_glyph(map.get(&code));
        }
    }

    fn show_unmapped(&mut self, data: &[u8], width: usize) {
        for chunk in data.chunks(width) {
            if width == 1 && chunk == b" " {
                self.space()
            } else {
                self.glyph(None)
            }
        }
    }

    fn mapped_glyph(&mut self, text: Option<&String>) {
        match text {
            Some(text) => text.chars().for_each(|c| self.glyph(Some(c))),
            None => self.glyph(None),
        }
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

fn inline_image_end(data: &[u8], mut start: usize) -> Option<usize> {
    loop {
        let end = start + find(&data[start..], b"EI")?;
        let before = end == 0 || is_ws(data[end - 1]);
        let after = data.get(end + 2).is_none_or(|&c| is_ws(c));
        start = end + 2;
        if before && after {
            return Some(start);
        }
    }
}

impl Page<'_> {
    fn show_last(&mut self, ops: &[Obj]) -> Res<()> {
        if let Some(s) = ops.iter().rev().find_map(|o| match o {
            Obj::Str(s) => Some(s),
            _ => None,
        }) {
            self.show(s)?;
        }
        Ok(())
    }

    fn show_array(&mut self, ops: &[Obj]) -> Res<()> {
        let Some(Obj::Array(items)) = ops.last() else {
            return Ok(());
        };
        for item in items {
            match item {
                Obj::Str(s) => self.show(s)?,
                n if n.num().is_some_and(|n| n < -200.0) => self.space(),
                _ => {}
            }
        }
        Ok(())
    }

    fn text_matrix(&mut self, y: Option<f64>) {
        match (self.last_y, y) {
            (Some(a), Some(b)) if (a - b).abs() > 0.5 => self.newline(),
            (Some(_), _) => self.space(),
            _ => {}
        }
        if y.is_some() {
            self.last_y = y;
        }
    }

    fn select_font(&mut self, ops: &[Obj]) {
        if let Some(Obj::Name(name)) = ops.first() {
            self.font = self.fonts.get(name).cloned();
        }
    }

    fn operator(&mut self, op: &[u8], ops: &[Obj]) -> Res<()> {
        match op {
            b"Tf" => self.select_font(ops),
            b"Tj" => self.show_last(ops)?,
            b"'" | b"\"" => {
                self.newline();
                self.show_last(ops)?;
            }
            b"TJ" => self.show_array(ops)?,
            b"Td" | b"TD" => match ops.get(1).and_then(Obj::num) {
                Some(ty) if ty != 0.0 => self.newline(),
                _ => self.space(),
            },
            b"T*" | b"ET" => self.newline(),
            b"Tm" => self.text_matrix(ops.get(5).and_then(Obj::num)),
            _ => {}
        }
        Ok(())
    }
}

/// Runs the text operators of one content stream.
pub(super) fn interpret(data: &[u8], page: &mut Page<'_>) -> Res<()> {
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
        if op == b"BI" {
            let Some(end) = inline_image_end(data, lex.i) else {
                return Ok(());
            };
            lex.i = end;
        } else {
            page.operator(op, &ops)?;
        }
        ops.clear();
    }
}
