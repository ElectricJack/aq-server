use crate::{Digest, Invalid, Mime, Result, Variant, limits::*};
use std::io::{Cursor, Write};

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
fn crc(bytes: &[u8]) -> u32 {
    let mut c = !0_u32;
    for byte in bytes {
        c = CRC[((c ^ u32::from(*byte)) & 255) as usize] ^ (c >> 8);
    }
    !c
}
fn be(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes.try_into().expect("checked four-byte slice"))
}

/// Structural policy check before any decoder allocation or pixel work.
pub fn preflight_png(input: &[u8]) -> Result<Header> {
    if input.len() > MAX_INPUT_BYTES {
        return Err(Invalid::Limit("input bytes"));
    }
    if input.get(..8) != Some(SIGNATURE.as_slice()) {
        return Err(Invalid::MalformedPng);
    }
    let (mut offset, mut chunks, mut data_bytes) = (8_usize, 0_usize, 0_usize);
    let (mut header, mut srgb, mut image_data) = (None, false, false);
    while offset < input.len() {
        chunks += 1;
        if chunks > MAX_PNG_CHUNKS {
            return Err(Invalid::Limit("PNG chunks"));
        }
        let prefix = input
            .get(offset..offset.checked_add(8).ok_or(Invalid::MalformedPng)?)
            .ok_or(Invalid::MalformedPng)?;
        let length = usize::try_from(be(&prefix[..4])).map_err(|_| Invalid::MalformedPng)?;
        let data_start = offset.checked_add(8).ok_or(Invalid::MalformedPng)?;
        let data_end = data_start
            .checked_add(length)
            .ok_or(Invalid::MalformedPng)?;
        let end = data_end.checked_add(4).ok_or(Invalid::MalformedPng)?;
        let data = input
            .get(data_start..data_end)
            .ok_or(Invalid::MalformedPng)?;
        let expected_crc = input.get(data_end..end).ok_or(Invalid::MalformedPng)?;
        if crc(input
            .get(offset + 4..data_end)
            .ok_or(Invalid::MalformedPng)?)
            != be(expected_crc)
        {
            return Err(Invalid::MalformedPng);
        }
        match &prefix[4..8] {
            b"IHDR" if header.is_none() && chunks == 1 && length == 13 => {
                let (width, height) = (be(&data[..4]), be(&data[4..8]));
                rgba_len(width, height)?;
                if data[8] != 8 || ![2, 6].contains(&data[9]) || data[12] != 0 {
                    return Err(Invalid::UnsupportedPng);
                }
                if data[10] != 0 || data[11] != 0 {
                    return Err(Invalid::MalformedPng);
                }
                header = Some(Header {
                    width,
                    height,
                    channels: if data[9] == 2 { 3 } else { 4 },
                });
            }
            b"sRGB" if header.is_some() && !image_data && !srgb && length == 1 && data[0] <= 3 => {
                srgb = true;
            }
            b"IDAT" if header.is_some() => {
                image_data = true;
                data_bytes = data_bytes
                    .checked_add(length)
                    .ok_or(Invalid::MalformedPng)?;
            }
            b"IEND"
                if header.is_some()
                    && image_data
                    && data_bytes > 0
                    && length == 0
                    && end == input.len() =>
            {
                return header.ok_or(Invalid::MalformedPng);
            }
            b"acTL" | b"fcTL" | b"fdAT" | b"PLTE" | b"tRNS" | b"iCCP" | b"gAMA" | b"cHRM"
            | b"tEXt" | b"zTXt" | b"iTXt" => return Err(Invalid::UnsupportedPng),
            b"IHDR" | b"sRGB" | b"IEND" => return Err(Invalid::MalformedPng),
            _ => return Err(Invalid::UnsupportedPng),
        }
        offset = end;
    }
    Err(Invalid::MalformedPng)
}

#[derive(Debug)]
pub struct PreparedVariant {
    descriptor: Variant,
    bytes: Vec<u8>,
}
impl PreparedVariant {
    pub fn descriptor(&self) -> &Variant {
        &self.descriptor
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn into_parts(self) -> (Variant, Vec<u8>) {
        (self.descriptor, self.bytes)
    }
}
#[derive(Debug)]
pub struct PreparedImage {
    input_digest: Digest,
    input_bytes: usize,
    render: PreparedVariant,
    preview: PreparedVariant,
}
impl PreparedImage {
    pub fn input_digest(&self) -> &Digest {
        &self.input_digest
    }
    pub fn input_bytes(&self) -> usize {
        self.input_bytes
    }
    pub fn render(&self) -> &PreparedVariant {
        &self.render
    }
    pub fn preview(&self) -> &PreparedVariant {
        &self.preview
    }
    pub fn into_variants(self) -> (PreparedVariant, PreparedVariant) {
        (self.render, self.preview)
    }
}
fn allocation(length: usize, maximum: usize) -> Result<Vec<u8>> {
    if length > maximum {
        return Err(Invalid::Limit("caller buffer"));
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| Invalid::Limit("caller buffer"))?;
    bytes.resize(length, 0);
    Ok(bytes)
}
struct BoundedOutput {
    bytes: Vec<u8>,
    maximum: usize,
    exceeded: bool,
}
impl Write for BoundedOutput {
    fn write(&mut self, value: &[u8]) -> std::io::Result<usize> {
        let length = self.bytes.len().checked_add(value.len());
        if length.is_none_or(|n| n > self.maximum)
            || self.bytes.try_reserve_exact(value.len()).is_err()
        {
            self.exceeded = true;
            return Err(std::io::Error::other("encoded media limit"));
        }
        self.bytes.extend_from_slice(value);
        Ok(value.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn encode(width: u32, height: u32, rgba: &[u8], maximum: usize) -> Result<PreparedVariant> {
    if rgba.len() != rgba_len(width, height)? {
        return Err(Invalid::Field("RGBA length"));
    }
    let mut output = BoundedOutput {
        bytes: Vec::new(),
        maximum,
        exceeded: false,
    };
    let encoded = (|| -> Result<()> {
        let mut encoder = png::Encoder::new(&mut output, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_source_srgb(png::SrgbRenderingIntent::Perceptual);
        encoder.set_compression(png::Compression::Fast);
        encoder.set_filter(png::Filter::Sub);
        encoder.validate_sequence(true);
        let mut writer = encoder
            .write_header()
            .map_err(|_| Invalid::ProcessingFailure)?;
        writer
            .write_image_data(rgba)
            .map_err(|_| Invalid::ProcessingFailure)?;
        writer.finish().map_err(|_| Invalid::ProcessingFailure)
    })();
    if output.exceeded {
        return Err(Invalid::Limit("encoded output"));
    }
    encoded?;
    Ok(PreparedVariant {
        descriptor: Variant {
            digest: Digest::of(&output.bytes),
            bytes: output.bytes.len() as u64,
            mime: Mime::Png,
            width,
            height,
        },
        bytes: output.bytes,
    })
}
fn decoder_error(error: png::DecodingError) -> Invalid {
    if matches!(error, png::DecodingError::LimitsExceeded) {
        Invalid::Limit("decoder scratch")
    } else {
        Invalid::MalformedPng
    }
}
// png::Reader deliberately discards data after its expected rows. Validate the
// complete zlib stream first so surplus raster/concatenated streams are refused
// rather than silently rendered as a prefix. The scratch never holds a raster.
fn validate_zlib(input: &[u8], header: Header) -> Result<()> {
    let expected = u64::from(header.width)
        .checked_mul(u64::from(header.channels))
        .and_then(|n| n.checked_add(1))
        .and_then(|n| n.checked_mul(u64::from(header.height)))
        .ok_or(Invalid::Limit("inflated scanlines"))?;
    let mut compressed = Vec::new();
    let mut offset = 8;
    while offset < input.len() {
        // Structural preflight has verified every range and complete chunk.
        let length = be(&input[offset..offset + 4]) as usize;
        if &input[offset + 4..offset + 8] == b"IDAT" {
            if compressed
                .len()
                .checked_add(length)
                .is_none_or(|n| n > MAX_COMPRESSED_BYTES)
            {
                return Err(Invalid::Limit("compressed data"));
            }
            compressed
                .try_reserve_exact(length)
                .map_err(|_| Invalid::Limit("compressed data"))?;
            compressed.extend_from_slice(&input[offset + 8..offset + 8 + length]);
        }
        offset += length + 12;
    }
    let mut decoder = flate2::Decompress::new(true);
    let mut scratch = [0_u8; ZLIB_SCRATCH_BYTES];
    let mut consumed = 0_usize;
    loop {
        let (before_in, before_out) = (decoder.total_in(), decoder.total_out());
        let remaining = expected
            .checked_add(1)
            .and_then(|n| n.checked_sub(before_out))
            .ok_or(Invalid::MalformedPng)?;
        let capacity = remaining.min(ZLIB_SCRATCH_BYTES as u64) as usize;
        if capacity == 0 {
            return Err(Invalid::MalformedPng);
        }
        let status = decoder
            .decompress(
                &compressed[consumed..],
                &mut scratch[..capacity],
                // Streaming scratch is deliberately smaller than the raster.
                // Finish promises enough output space for the whole remaining
                // stream, so it must not be used with this bounded scratch.
                flate2::FlushDecompress::None,
            )
            .map_err(|_| Invalid::MalformedPng)?;
        let used =
            usize::try_from(decoder.total_in() - before_in).map_err(|_| Invalid::MalformedPng)?;
        consumed = consumed
            .checked_add(used)
            .filter(|n| *n <= compressed.len())
            .ok_or(Invalid::MalformedPng)?;
        if decoder.total_out() > expected {
            return Err(Invalid::MalformedPng);
        }
        if status == flate2::Status::StreamEnd {
            return if consumed == compressed.len() && decoder.total_out() == expected {
                Ok(())
            } else {
                Err(Invalid::MalformedPng)
            };
        }
        if decoder.total_in() == before_in && decoder.total_out() == before_out {
            return Err(Invalid::MalformedPng);
        }
    }
}
fn prepare(input: &[u8], header: Header) -> Result<PreparedImage> {
    let mut options = png::DecodeOptions::default();
    options.set_ignore_crc(false);
    options.set_ignore_adler32(false);
    options.set_skip_ancillary_crc_failures(false);
    options.set_ignore_text_chunk(true);
    options.set_ignore_iccp_chunk(true);
    let mut decoder = png::Decoder::new_with_options(Cursor::new(input), options);
    decoder.set_limits(png::Limits {
        bytes: DECODER_SCRATCH_HINT,
    });
    decoder.set_transformations(png::Transformations::IDENTITY);
    let mut reader = decoder.read_info().map_err(decoder_error)?;
    if reader.info().width != header.width
        || reader.info().height != header.height
        || reader.info().animation_control.is_some()
        || reader.info().frame_control.is_some()
        || reader.info().interlaced
    {
        return Err(Invalid::MalformedPng);
    }
    let pixels = rgba_len(header.width, header.height)? / 4;
    let required = pixels
        .checked_mul(usize::from(header.channels))
        .ok_or(Invalid::Limit("decoded output"))?;
    if reader.output_buffer_size() != Some(required) {
        return Err(Invalid::MalformedPng);
    }
    let mut rgba = allocation(pixels * 4, MAX_RGBA_BYTES)?;
    let info = reader
        .next_frame(&mut rgba[..required])
        .map_err(decoder_error)?;
    if info.width != header.width
        || info.height != header.height
        || info.bit_depth != png::BitDepth::Eight
        || info.buffer_size() != required
        || info.color_type
            != if header.channels == 3 {
                png::ColorType::Rgb
            } else {
                png::ColorType::Rgba
            }
    {
        return Err(Invalid::MalformedPng);
    }
    reader.finish().map_err(decoder_error)?;
    drop(reader);
    if header.channels == 3 {
        for pixel in (0..pixels).rev() {
            let rgb = [
                rgba[pixel * 3],
                rgba[pixel * 3 + 1],
                rgba[pixel * 3 + 2],
                255,
            ];
            rgba[pixel * 4..pixel * 4 + 4].copy_from_slice(&rgb);
        }
    }
    let render = encode(header.width, header.height, &rgba, MAX_RENDER_BYTES)?;
    let (width, height) = preview_dimensions(header.width, header.height)?;
    let mut thumbnail = allocation(rgba_len(width, height)?, MAX_PREVIEW_RGBA_BYTES)?;
    for y in 0..height {
        for x in 0..width {
            let source_x = u64::from(x) * u64::from(header.width) / u64::from(width);
            let source_y = u64::from(y) * u64::from(header.height) / u64::from(height);
            let from = ((source_y * u64::from(header.width) + source_x) * 4) as usize;
            let to = ((u64::from(y) * u64::from(width) + u64::from(x)) * 4) as usize;
            thumbnail[to..to + 4].copy_from_slice(&rgba[from..from + 4]);
        }
    }
    let preview = encode(width, height, &thumbnail, MAX_PREVIEW_BYTES)?;
    Ok(PreparedImage {
        input_digest: Digest::of(input),
        input_bytes: input.len(),
        render,
        preview,
    })
}
/// No IO or authority. The daemon must separately own the raw input, retain all
/// bytes, reserve capacity and recheck authorization before publishing a receipt.
pub fn prepare_png(input: &[u8]) -> Result<PreparedImage> {
    let header = preflight_png(input)?;
    std::panic::catch_unwind(|| {
        validate_zlib(input, header)?;
        prepare(input, header)
    })
    .map_err(|_| Invalid::ProcessingFailure)?
}

#[cfg(test)]
mod output_tests {
    use super::*;
    #[test]
    fn known_crc_vector_and_encoding_bound() {
        assert_eq!(crc(b"123456789"), 0xcbf43926);
        assert!(matches!(
            encode(1, 1, &[0, 0, 0, 255], 12),
            Err(Invalid::Limit("encoded output"))
        ));
        assert!(matches!(
            allocation(MAX_RGBA_BYTES + 1, MAX_RGBA_BYTES),
            Err(Invalid::Limit("caller buffer"))
        ));
    }
}
