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

/// Splits one container level into (type, body) boxes.
fn boxes<'a>(bytes: &'a [u8], top: bool, seen: &mut usize) -> Result<Vec<([u8; 4], &'a [u8])>> {
    let mut out = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        *seen += 1;
        if *seen > MAX_MP4_BOXES {
            return Err(Invalid::Limit("mp4 boxes"));
        }
        let rest = &bytes[offset..];
        if rest.len() < 8 {
            return Err(Invalid::MalformedVideo);
        }
        let size = u64::from(u32::from_be_bytes(rest[..4].try_into().expect("4 bytes")));
        let kind: [u8; 4] = rest[4..8].try_into().expect("4 bytes");
        let (header, total) = match size {
            // Only a top-level box may extend to end of file.
            0 if top => (8, rest.len() as u64),
            1 => {
                if rest.len() < 16 {
                    return Err(Invalid::MalformedVideo);
                }
                (16, u64::from_be_bytes(rest[8..16].try_into().expect("8")))
            }
            0..=7 => return Err(Invalid::MalformedVideo),
            n => (8, n),
        };
        if total < header || total > rest.len() as u64 {
            return Err(Invalid::MalformedVideo);
        }
        out.push((kind, &rest[header as usize..total as usize]));
        offset += total as usize;
    }
    Ok(out)
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
fn full_box(b: &[u8]) -> Result<u8> {
    let version = *b.first().ok_or(Invalid::MalformedVideo)?;
    if version > 1 {
        return Err(Invalid::UnsupportedVideo);
    }
    Ok(version)
}
fn duration(mvhd: &[u8]) -> Result<u64> {
    let (scale, value) = if full_box(mvhd)? == 0 {
        (be32(mvhd, 12)?, u64::from(be32(mvhd, 16)?))
    } else {
        (be32(mvhd, 20)?, be64(mvhd, 24)?)
    };
    if scale == 0 || value == 0 || value == u64::from(u32::MAX) || value == u64::MAX {
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
fn dimensions(tkhd: &[u8]) -> Result<(u32, u32)> {
    // Fixed fields after version/flags: v0 20 bytes, v1 32 bytes; then
    // reserved(8) layer(2) group(2) volume(2) reserved(2) matrix(36).
    let at = if full_box(tkhd)? == 0 { 4 + 20 } else { 4 + 32 } + 52;
    let (w, h) = (be32(tkhd, at)?, be32(tkhd, at + 4)?);
    if w & 0xffff != 0 || h & 0xffff != 0 {
        return Err(Invalid::UnsupportedVideo);
    }
    let (w, h) = (w >> 16, h >> 16);
    rgba_len(w, h)?;
    Ok((w, h))
}
fn track(trak: &[u8], seen: &mut usize) -> Result<Option<(u32, u32)>> {
    let mut tkhd = None;
    let mut handler = None;
    for (kind, body) in boxes(trak, false, seen)? {
        match &kind {
            b"tkhd" if tkhd.is_none() => tkhd = Some(body),
            b"tkhd" => return Err(Invalid::MalformedVideo),
            b"mdia" => {
                for (kind, body) in boxes(body, false, seen)? {
                    if &kind == b"hdlr" {
                        if handler.is_some() {
                            return Err(Invalid::MalformedVideo);
                        }
                        full_box(body)?;
                        handler = Some(
                            <[u8; 4]>::try_from(body.get(8..12).ok_or(Invalid::MalformedVideo)?)
                                .expect("4"),
                        );
                    }
                }
            }
            _ => {}
        }
    }
    let (Some(tkhd), Some(handler)) = (tkhd, handler) else {
        return Err(Invalid::MalformedVideo);
    };
    if &handler == b"vide" {
        dimensions(tkhd).map(Some)
    } else {
        Ok(None)
    }
}
/// Accepts only top-level `ftyp` (first), one `moov`, `mdat` and `free`/`skip`
/// boxes; fragmented (`moof`/`mvex`) and other layouts are unsupported.
pub fn preflight_mp4(input: &[u8]) -> Result<ClipHeader> {
    if input.is_empty() || input.len() > MAX_CLIP_BYTES {
        return Err(Invalid::Limit("clip bytes"));
    }
    let mut seen = 0;
    let top = boxes(input, true, &mut seen)?;
    let mut moov = None;
    let mut mdat = false;
    for (index, (kind, body)) in top.iter().enumerate() {
        match (index, kind) {
            (0, b"ftyp") => {
                if body.len() < 8 || body.len() % 4 != 0 {
                    return Err(Invalid::MalformedVideo);
                }
                let brands = std::iter::once(&body[..4]).chain(body[8..].chunks(4));
                if !brands
                    .into_iter()
                    .any(|b| BRANDS.iter().any(|k| &k[..] == b))
                {
                    return Err(Invalid::UnsupportedVideo);
                }
            }
            (0, _) => return Err(Invalid::MalformedVideo),
            (_, b"ftyp") => return Err(Invalid::MalformedVideo),
            (_, b"moov") if moov.is_none() => moov = Some(*body),
            (_, b"moov") => return Err(Invalid::MalformedVideo),
            (_, b"mdat") => mdat = true,
            (_, b"free" | b"skip") => {}
            _ => return Err(Invalid::UnsupportedVideo),
        }
    }
    let (Some(moov), true) = (moov, mdat) else {
        return Err(Invalid::MalformedVideo);
    };
    let children = boxes(moov, false, &mut seen)?;
    let mut duration_ms = None;
    let mut visual = None;
    for (kind, body) in children {
        match &kind {
            b"mvhd" if duration_ms.is_none() => duration_ms = Some(duration(body)?),
            b"mvhd" => return Err(Invalid::MalformedVideo),
            b"mvex" => return Err(Invalid::UnsupportedVideo),
            b"trak" => {
                if let Some(size) = track(body, &mut seen)? {
                    if visual.is_some() {
                        return Err(Invalid::UnsupportedVideo);
                    }
                    visual = Some(size);
                }
            }
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
