use crate::{Digest, Invalid, Mime, Result, Variant, limits::*};
use std::io::{Cursor, Write};

mod preflight;
use preflight::validate_zlib;
pub use preflight::{Header, preflight_png};

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
fn decode_rgba(input: &[u8], header: Header) -> Result<Vec<u8>> {
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
    header.check_info(reader.info())?;
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
    header.check_frame(&info, required)?;
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
    Ok(rgba)
}

impl Header {
    fn check_info(&self, info: &png::Info<'_>) -> Result<()> {
        let static_frame =
            info.animation_control.is_none() && info.frame_control.is_none() && !info.interlaced;
        if (info.width, info.height) != (self.width, self.height) || !static_frame {
            return Err(Invalid::MalformedPng);
        }
        Ok(())
    }

    fn check_frame(&self, info: &png::OutputInfo, required: usize) -> Result<()> {
        let expected_color = if self.channels == 3 {
            png::ColorType::Rgb
        } else {
            png::ColorType::Rgba
        };
        let geometry = (info.width, info.height, info.buffer_size());
        let format = (info.bit_depth, info.color_type);
        if geometry != (self.width, self.height, required)
            || format != (png::BitDepth::Eight, expected_color)
        {
            return Err(Invalid::MalformedPng);
        }
        Ok(())
    }
}

fn preview(header: Header, rgba: &[u8]) -> Result<PreparedVariant> {
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
    encode(width, height, &thumbnail, MAX_PREVIEW_BYTES)
}

fn prepare(input: &[u8], header: Header) -> Result<PreparedImage> {
    let rgba = decode_rgba(input, header)?;
    let render = encode(header.width, header.height, &rgba, MAX_RENDER_BYTES)?;
    let preview = preview(header, &rgba)?;
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
        assert_eq!(preflight::crc(b"123456789"), 0xcbf43926);
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
