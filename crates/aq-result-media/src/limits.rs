use crate::{Invalid, Result};
pub const MAX_INPUT_BYTES: usize = 1_048_576;
pub const MAX_COMPRESSED_BYTES: usize = MAX_INPUT_BYTES;
pub const ZLIB_SCRATCH_BYTES: usize = 8192;
pub const MAX_PIXELS: u64 = 2_000_000;
pub const MAX_DIMENSION: u32 = 4096;
pub const MAX_RGBA_BYTES: usize = 8_000_000;
pub const MAX_PREVIEW_RGBA_BYTES: usize = 589_824;
pub const MAX_RENDER_BYTES: usize = 8_388_608;
pub const MAX_PREVIEW_BYTES: usize = 786_432;
/// Additional decoder hint. The dependency documents this as best-effort.
pub const DECODER_SCRATCH_HINT: usize = 16 * 1024 * 1024;
pub const MAX_PNG_CHUNKS: usize = 128;
pub const PREVIEW_EDGE: u32 = 384;
pub const MAX_JSON_BYTES: usize = 16_384;
pub const MAX_PAGE_BYTES: usize = 262_144;
pub const MAX_REPORT_ARTIFACTS: usize = 8;
pub const MAX_UPLOAD_TTL_MS: u64 = 1_800_000;
pub const MAX_RETENTION_MS: u64 = 604_800_000;
pub const MAX_SAFE_MILLIS: u64 = 9_007_199_254_740_991;
/// Clips are retained as a verified passthrough copy (no transcode), so input,
/// render copy and poster preview must fit the unchanged 10 MiB reservation.
pub const MAX_CLIP_BYTES: usize = 4_194_304;
pub const MAX_CLIP_DURATION_MS: u64 = 60_000;
/// Bound on ISO-BMFF boxes walked during structural preflight.
pub const MAX_MP4_BOXES: usize = 4096;

pub fn rgba_len(width: u32, height: u32) -> Result<usize> {
    if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(Invalid::Limit("dimensions"));
    }
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or(Invalid::Limit("pixels"))?;
    if pixels > MAX_PIXELS {
        return Err(Invalid::Limit("pixels"));
    }
    usize::try_from(pixels.checked_mul(4).ok_or(Invalid::Limit("RGBA buffer"))?)
        .ok()
        .filter(|v| *v <= MAX_RGBA_BYTES)
        .ok_or(Invalid::Limit("RGBA buffer"))
}
pub fn preview_dimensions(width: u32, height: u32) -> Result<(u32, u32)> {
    rgba_len(width, height)?;
    let longest = width.max(height);
    if longest <= PREVIEW_EDGE {
        return Ok((width, height));
    }
    Ok((
        (u64::from(width) * u64::from(PREVIEW_EDGE) / u64::from(longest)).max(1) as u32,
        (u64::from(height) * u64::from(PREVIEW_EDGE) / u64::from(longest)).max(1) as u32,
    ))
}
