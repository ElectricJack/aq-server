//! Structural PNG checks and bounded validation of the complete raster stream.
use crate::{Invalid, Result, limits::*};
const SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub width: u32,
    pub height: u32,
    pub channels: u8,
}

const fn crc_table() -> [u32; 256] {
    let mut table = [0; 256];
    let mut n = 0;
    while n < 256 {
        let mut c = n as u32;
        let mut bit = 0;
        while bit < 8 {
            c = if c & 1 != 0 {
                0xedb88320 ^ (c >> 1)
            } else {
                c >> 1
            };
            bit += 1;
        }
        table[n] = c;
        n += 1;
    }
    table
}
const CRC: [u32; 256] = crc_table();
pub(super) fn crc(bytes: &[u8]) -> u32 {
    let mut c = !0_u32;
    for byte in bytes {
        c = CRC[((c ^ u32::from(*byte)) & 255) as usize] ^ (c >> 8);
    }
    !c
}
fn be(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes.try_into().expect("checked four-byte slice"))
}

struct Chunk<'a> {
    kind: &'a [u8],
    data: &'a [u8],
    crc_input: &'a [u8],
    expected_crc: u32,
}

impl<'a> Chunk<'a> {
    fn read(bytes: &'a [u8]) -> Result<(Self, &'a [u8])> {
        let prefix = bytes.get(..8).ok_or(Invalid::MalformedPng)?;
        let length = usize::try_from(be(&prefix[..4])).map_err(|_| Invalid::MalformedPng)?;
        let data_end = 8usize.checked_add(length).ok_or(Invalid::MalformedPng)?;
        let end = data_end.checked_add(4).ok_or(Invalid::MalformedPng)?;
        let checksum = bytes.get(data_end..end).ok_or(Invalid::MalformedPng)?;
        Ok((
            Self {
                kind: &prefix[4..],
                data: &bytes[8..data_end],
                crc_input: &bytes[4..data_end],
                expected_crc: be(checksum),
            },
            &bytes[end..],
        ))
    }

    fn verify_crc(&self) -> Result<()> {
        if crc(self.crc_input) != self.expected_crc {
            return Err(Invalid::MalformedPng);
        }
        Ok(())
    }
}

impl Chunk<'_> {
    fn header(&self) -> Result<Header> {
        let data = self.data;
        if data.len() != 13 {
            return Err(Invalid::MalformedPng);
        }
        let (width, height) = (be(&data[..4]), be(&data[4..8]));
        rgba_len(width, height)?;
        let channels = match (data[8], data[9], data[12]) {
            (8, 2, 0) => 3,
            (8, 6, 0) => 4,
            _ => return Err(Invalid::UnsupportedPng),
        };
        if (data[10], data[11]) != (0, 0) {
            return Err(Invalid::MalformedPng);
        }
        Ok(Header {
            width,
            height,
            channels,
        })
    }
}

#[derive(Default)]
struct PngChunks {
    count: usize,
    header: Option<Header>,
    srgb: bool,
    // An empty IDAT still prevents a following sRGB.
    data_bytes: Option<usize>,
}

impl PngChunks {
    fn record_srgb(&mut self, chunk: &Chunk<'_>) -> Result<()> {
        let expected = self.header.is_some() && self.data_bytes.is_none() && !self.srgb;
        if !expected || !matches!(chunk.data, [0..=3]) {
            return Err(Invalid::MalformedPng);
        }
        self.srgb = true;
        Ok(())
    }

    fn accept(&mut self, chunk: &Chunk<'_>, last: bool) -> Result<Option<Header>> {
        match chunk.kind {
            b"IHDR" if self.count == 1 => self.header = Some(chunk.header()?),
            b"sRGB" => self.record_srgb(chunk)?,
            b"IDAT" if self.header.is_some() => {
                self.data_bytes = Some(
                    self.data_bytes
                        .unwrap_or(0)
                        .checked_add(chunk.data.len())
                        .ok_or(Invalid::MalformedPng)?,
                );
            }
            b"IEND" => {
                let complete = self.data_bytes.is_some_and(|n| n > 0);
                let at_end = last && chunk.data.is_empty();
                if !(complete && at_end) {
                    return Err(Invalid::MalformedPng);
                }
                return Ok(self.header);
            }
            b"IHDR" => return Err(Invalid::MalformedPng),
            _ => return Err(Invalid::UnsupportedPng),
        }
        Ok(None)
    }
}

/// Structural policy check before any decoder allocation or pixel work.
pub fn preflight_png(input: &[u8]) -> Result<Header> {
    if input.len() > MAX_INPUT_BYTES {
        return Err(Invalid::Limit("input bytes"));
    }
    let mut bytes = input.strip_prefix(SIGNATURE).ok_or(Invalid::MalformedPng)?;
    let mut chunks = PngChunks::default();
    while !bytes.is_empty() {
        chunks.count += 1;
        if chunks.count > MAX_PNG_CHUNKS {
            return Err(Invalid::Limit("PNG chunks"));
        }
        let (chunk, rest) = Chunk::read(bytes)?;
        chunk.verify_crc()?;
        if let Some(header) = chunks.accept(&chunk, rest.is_empty())? {
            return Ok(header);
        }
        bytes = rest;
    }
    Err(Invalid::MalformedPng)
}

// png::Reader deliberately discards data after its expected rows. Validate the
// complete zlib stream first so surplus raster/concatenated streams are refused
// rather than silently rendered as a prefix. The scratch never holds a raster.
pub(super) fn validate_zlib(input: &[u8], header: Header) -> Result<()> {
    let expected = u64::from(header.width)
        .checked_mul(u64::from(header.channels))
        .and_then(|n| n.checked_add(1))
        .and_then(|n| n.checked_mul(u64::from(header.height)))
        .ok_or(Invalid::Limit("inflated scanlines"))?;
    let compressed = compressed_image_data(input)?;
    inflate_scanlines(&compressed, expected)
}

fn compressed_image_data(input: &[u8]) -> Result<Vec<u8>> {
    let mut compressed = Vec::new();
    let mut bytes = &input[SIGNATURE.len()..];
    while !bytes.is_empty() {
        // Structural preflight already checked these chunks and their checksums.
        let (chunk, rest) = Chunk::read(bytes)?;
        if chunk.kind == b"IDAT" {
            chunk.append_compressed(&mut compressed)?;
        }
        bytes = rest;
    }
    Ok(compressed)
}

impl Chunk<'_> {
    fn append_compressed(&self, compressed: &mut Vec<u8>) -> Result<()> {
        let data = self.data;
        if compressed
            .len()
            .checked_add(data.len())
            .is_none_or(|n| n > MAX_COMPRESSED_BYTES)
        {
            return Err(Invalid::Limit("compressed data"));
        }
        compressed
            .try_reserve_exact(data.len())
            .map_err(|_| Invalid::Limit("compressed data"))?;
        compressed.extend_from_slice(data);
        Ok(())
    }
}

struct StreamExtent {
    compressed: u64,
    inflated: u64,
}
impl StreamExtent {
    fn verify_progress(&self, used: (u64, u64)) -> Result<()> {
        if used.0 > self.compressed || used.1 > self.inflated {
            return Err(Invalid::MalformedPng);
        }
        Ok(())
    }

    fn finish(&self, used: (u64, u64)) -> Result<()> {
        if used != (self.compressed, self.inflated) {
            return Err(Invalid::MalformedPng);
        }
        Ok(())
    }
}
fn inflate_scanlines(compressed: &[u8], expected: u64) -> Result<()> {
    let extent = StreamExtent {
        compressed: compressed.len() as u64,
        inflated: expected,
    };
    let mut decoder = flate2::Decompress::new(true);
    let mut scratch = [0_u8; ZLIB_SCRATCH_BYTES];
    loop {
        let before = (decoder.total_in(), decoder.total_out());
        let consumed = usize::try_from(before.0).map_err(|_| Invalid::MalformedPng)?;
        let remaining = compressed.get(consumed..).ok_or(Invalid::MalformedPng)?;
        let capacity = extent.scratch_capacity(before.1)?;
        // Finish assumes the entire remaining raster fits; our scratch is bounded.
        let status = decoder
            .decompress(
                remaining,
                &mut scratch[..capacity],
                flate2::FlushDecompress::None,
            )
            .map_err(|_| Invalid::MalformedPng)?;
        let after = (decoder.total_in(), decoder.total_out());
        extent.verify_progress(after)?;
        if status == flate2::Status::StreamEnd {
            return extent.finish(after);
        }
        if before == after {
            return Err(Invalid::MalformedPng);
        }
    }
}
impl StreamExtent {
    fn scratch_capacity(&self, inflated: u64) -> Result<usize> {
        let remaining = self
            .inflated
            .checked_add(1)
            .and_then(|n| n.checked_sub(inflated))
            .filter(|n| *n != 0)
            .ok_or(Invalid::MalformedPng)?;
        Ok(remaining.min(ZLIB_SCRATCH_BYTES as u64) as usize)
    }
}
