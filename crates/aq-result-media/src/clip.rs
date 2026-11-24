//! Structural ISO-BMFF (MP4) clip preflight. No codec is linked: a clip is
//! retained byte-for-byte, so the owner only proves a bounded, non-fragmented
//! container with one visual track and a duration header it can cite.
use crate::{Invalid, Result, limits::*};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipHeader {
    pub width: u32,
    pub height: u32,
    pub duration_ms: u64,
}

const BRANDS: [&[u8; 4]; 10] = [
    b"isom", b"iso2", b"iso3", b"iso4", b"iso5", b"iso6", b"mp41", b"mp42", b"avc1", b"M4V ",
];

struct Mp4Box<'a> {
    kind: [u8; 4],
    body: &'a [u8],
}

impl<'a> Mp4Box<'a> {
    fn read(bytes: &'a [u8], top: bool) -> Result<(Self, &'a [u8])> {
        let size = be32(bytes, 0)?;
        let kind = bytes
            .get(4..8)
            .ok_or(Invalid::MalformedVideo)?
            .try_into()
            .expect("four bytes");
        let (header, total) = match size {
            // Only a top-level box may extend to end of file.
            0 if top => (8, bytes.len() as u64),
            1 => (16, be64(bytes, 8)?),
            0..=7 => return Err(Invalid::MalformedVideo),
            n => (8, u64::from(n)),
        };
        let total = usize::try_from(total).map_err(|_| Invalid::MalformedVideo)?;
        let body = bytes.get(header..total).ok_or(Invalid::MalformedVideo)?;
        Ok((Self { kind, body }, &bytes[total..]))
    }
}

#[derive(Default)]
struct BoxReader {
    seen: usize,
}
impl BoxReader {
    /// Split one container level while enforcing the shared budget across nested boxes.
    fn boxes<'a>(&mut self, mut bytes: &'a [u8], top: bool) -> Result<Vec<Mp4Box<'a>>> {
        let mut out = Vec::new();
        while !bytes.is_empty() {
            self.seen += 1;
            if self.seen > MAX_MP4_BOXES {
                return Err(Invalid::Limit("mp4 boxes"));
            }
            let (element, rest) = Mp4Box::read(bytes, top)?;
            out.push(element);
            bytes = rest;
        }
        Ok(out)
    }
}
fn be32(b: &[u8], at: usize) -> Result<u32> {
    b.get(at..at + 4)
        .map(|v| u32::from_be_bytes(v.try_into().expect("4")))
        .ok_or(Invalid::MalformedVideo)
}
fn be64(b: &[u8], at: usize) -> Result<u64> {
    b.get(at..at + 8)
        .map(|v| u64::from_be_bytes(v.try_into().expect("8")))
        .ok_or(Invalid::MalformedVideo)
}
impl Mp4Box<'_> {
    fn version(&self) -> Result<u8> {
        let version = *self.body.first().ok_or(Invalid::MalformedVideo)?;
        if version > 1 {
            return Err(Invalid::UnsupportedVideo);
        }
        Ok(version)
    }
    fn duration(&self) -> Result<u64> {
        let mvhd = self.body;
        let (scale, value) = if self.version()? == 0 {
            (be32(mvhd, 12)?, u64::from(be32(mvhd, 16)?))
        } else {
            (be32(mvhd, 20)?, be64(mvhd, 24)?)
        };
        if scale == 0 || [0, u64::from(u32::MAX), u64::MAX].contains(&value) {
            return Err(Invalid::MalformedVideo);
        }
        // Round up so cited ranges never exceed the declared presentation.
        let ms = (u128::from(value) * 1000).div_ceil(u128::from(scale));
        let ms = u64::try_from(ms).map_err(|_| Invalid::Limit("clip duration"))?;
        if ms > MAX_CLIP_DURATION_MS {
            return Err(Invalid::Limit("clip duration"));
        }
        Ok(ms)
    }
    fn dimensions(&self) -> Result<(u32, u32)> {
        let tkhd = self.body;
        // Fixed fields after version/flags: v0 20 bytes, v1 32 bytes; then
        // reserved(8) layer(2) group(2) volume(2) reserved(2) matrix(36).
        let at = if self.version()? == 0 { 4 + 20 } else { 4 + 32 } + 52;
        let (w, h) = (be32(tkhd, at)?, be32(tkhd, at + 4)?);
        if w & 0xffff != 0 || h & 0xffff != 0 {
            return Err(Invalid::UnsupportedVideo);
        }
        let (w, h) = (w >> 16, h >> 16);
        rgba_len(w, h)?;
        Ok((w, h))
    }
}
impl BoxReader {
    fn read_handler(&mut self, mdia: &Mp4Box<'_>, handler: &mut Option<[u8; 4]>) -> Result<()> {
        for element in self.boxes(mdia.body, false)? {
            if &element.kind != b"hdlr" {
                continue;
            }
            if handler.is_some() {
                return Err(Invalid::MalformedVideo);
            }
            element.version()?;
            *handler = Some(
                element
                    .body
                    .get(8..12)
                    .ok_or(Invalid::MalformedVideo)?
                    .try_into()
                    .expect("four bytes"),
            );
        }
        Ok(())
    }

    fn track(&mut self, trak: &Mp4Box<'_>) -> Result<Option<(u32, u32)>> {
        let mut tkhd = None;
        let mut handler = None;
        for element in self.boxes(trak.body, false)? {
            match &element.kind {
                b"tkhd" if tkhd.is_none() => tkhd = Some(element),
                b"tkhd" => return Err(Invalid::MalformedVideo),
                b"mdia" => self.read_handler(&element, &mut handler)?,
                _ => {}
            }
        }
        let (Some(tkhd), Some(handler)) = (tkhd, handler) else {
            return Err(Invalid::MalformedVideo);
        };
        if &handler == b"vide" {
            tkhd.dimensions().map(Some)
        } else {
            Ok(None)
        }
    }

    fn movie_header(&mut self, moov: &Mp4Box<'_>) -> Result<ClipHeader> {
        let mut duration_ms = None;
        let mut visual = None;
        for element in self.boxes(moov.body, false)? {
            match &element.kind {
                b"mvhd" if duration_ms.is_none() => duration_ms = Some(element.duration()?),
                b"mvhd" => return Err(Invalid::MalformedVideo),
                b"mvex" => return Err(Invalid::UnsupportedVideo),
                b"trak" => add_visual_track(&mut visual, self.track(&element)?)?,
                _ => {}
            }
        }
        let (Some(duration_ms), Some((width, height))) = (duration_ms, visual) else {
            return Err(Invalid::MalformedVideo);
        };
        Ok(ClipHeader {
            width,
            height,
            duration_ms,
        })
    }
}
fn add_visual_track(visual: &mut Option<(u32, u32)>, track: Option<(u32, u32)>) -> Result<()> {
    if track.is_some_and(|size| visual.replace(size).is_some()) {
        return Err(Invalid::UnsupportedVideo);
    }
    Ok(())
}

impl Mp4Box<'_> {
    fn check_file_type(&self) -> Result<()> {
        let body = self.body;
        if body.len() < 8 || !body.len().is_multiple_of(4) {
            return Err(Invalid::MalformedVideo);
        }
        let mut brands = std::iter::once(&body[..4]).chain(body[8..].chunks(4));
        if !brands.any(|b| BRANDS.iter().any(|k| &k[..] == b)) {
            return Err(Invalid::UnsupportedVideo);
        }
        Ok(())
    }
}
fn movie_box<'a, 'b>(top: &'b [Mp4Box<'a>]) -> Result<&'b Mp4Box<'a>> {
    let Some((first, rest)) = top.split_first() else {
        return Err(Invalid::MalformedVideo);
    };
    if &first.kind != b"ftyp" {
        return Err(Invalid::MalformedVideo);
    }
    first.check_file_type()?;
    let mut moov = None;
    let mut mdat = false;
    for element in rest {
        match &element.kind {
            b"moov" if moov.is_none() => moov = Some(element),
            b"ftyp" | b"moov" => return Err(Invalid::MalformedVideo),
            b"mdat" => mdat = true,
            b"free" | b"skip" => {}
            _ => return Err(Invalid::UnsupportedVideo),
        }
    }
    moov.filter(|_| mdat).ok_or(Invalid::MalformedVideo)
}

/// Accepts only top-level `ftyp` (first), one `moov`, `mdat` and `free`/`skip`
/// boxes; fragmented (`moof`/`mvex`) and other layouts are unsupported.
pub fn preflight_mp4(input: &[u8]) -> Result<ClipHeader> {
    if !(1..=MAX_CLIP_BYTES).contains(&input.len()) {
        return Err(Invalid::Limit("clip bytes"));
    }
    let mut reader = BoxReader::default();
    let top = reader.boxes(input, true)?;
    reader.movie_header(movie_box(&top)?)
}

/// Minimal well-formed clip for tests: ftyp, moov(mvhd, one `vide` trak), mdat.
#[doc(hidden)]
pub fn synthetic_mp4(width: u32, height: u32, duration_ms: u32) -> Vec<u8> {
    fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }
    let mut mvhd = vec![0; 100];
    mvhd[12..16].copy_from_slice(&1000u32.to_be_bytes());
    mvhd[16..20].copy_from_slice(&duration_ms.to_be_bytes());
    let mut tkhd = vec![0; 84];
    tkhd[76..80].copy_from_slice(&(width << 16).to_be_bytes());
    tkhd[80..84].copy_from_slice(&(height << 16).to_be_bytes());
    let mut hdlr = vec![0; 24];
    hdlr[8..12].copy_from_slice(b"vide");
    let mdia = boxed(b"mdia", &boxed(b"hdlr", &hdlr));
    let trak = boxed(b"trak", &[boxed(b"tkhd", &tkhd), mdia].concat());
    let moov = boxed(b"moov", &[boxed(b"mvhd", &mvhd), trak].concat());
    [
        boxed(b"ftyp", b"isom\0\0\x02\0isommp41"),
        moov,
        boxed(b"mdat", &[0u8; 32]),
    ]
    .concat()
}
