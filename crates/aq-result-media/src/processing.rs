//! Prepare deterministic variants and serialize the receipt for finalized media.

use crate::{
    Clip, ClipHeader, Id, Input, Invalid, Limits, MediaKind, PreparedImage, Receipt, Rejection,
    ReportBinding, Result, UploadCreate, Validate, Variant, Variants, canonical_bytes,
    preflight_mp4, prepare_png,
};
use uuid::Uuid;

pub enum PreparedMedia {
    Image(PreparedImage),
    Clip {
        header: ClipHeader,
        render: Variant,
        preview: Variant,
        bytes: Vec<u8>,
        preview_bytes: Vec<u8>,
    },
}

pub struct ReceiptSource<'a> {
    pub artifact: Uuid,
    pub uploader: Uuid,
    pub binding: &'a ReportBinding,
    pub request: &'a UploadCreate,
    pub kind: MediaKind,
    pub retained_at_ms: u64,
    pub retain_until_ms: u64,
}

pub fn prepare_image(bytes: &[u8]) -> std::result::Result<PreparedMedia, Rejection> {
    prepare_png(bytes)
        .map(PreparedMedia::Image)
        .map_err(|invalid| match invalid {
            Invalid::UnsupportedPng => Rejection::UnsupportedPng,
            Invalid::Limit(_) => Rejection::ProcessingLimit,
            _ => Rejection::InvalidPng,
        })
}

pub fn prepare_clip(bytes: &[u8], limits: &Limits) -> std::result::Result<ClipHeader, Rejection> {
    let header = preflight_mp4(bytes).map_err(|invalid| match invalid {
        Invalid::UnsupportedVideo => Rejection::UnsupportedVideo,
        Invalid::Limit(_) => Rejection::ProcessingLimit,
        _ => Rejection::InvalidVideo,
    })?;
    if bytes.len() as u64 > limits.max_clip_bytes
        || header.duration_ms > limits.max_clip_duration_ms
    {
        return Err(Rejection::ProcessingLimit);
    }
    Ok(header)
}

impl PreparedMedia {
    pub fn descriptors(&self) -> (&Variant, &Variant) {
        match self {
            Self::Image(image) => (image.render().descriptor(), image.preview().descriptor()),
            Self::Clip {
                render, preview, ..
            } => (render, preview),
        }
    }

    pub fn enforce_limits(&self, limits: &Limits) -> std::result::Result<(), Rejection> {
        let (render, preview) = self.descriptors();
        let variants_fit = render.bytes <= limits.max_render_bytes.max(limits.max_clip_bytes)
            && preview.bytes <= limits.max_preview_bytes;
        let dimensions_fit = render.width.max(render.height) <= limits.max_dimension;
        let geometry_fits = dimensions_fit
            && u64::from(render.width) * u64::from(render.height) <= limits.max_pixels;
        if !variants_fit || !geometry_fits {
            return Err(Rejection::ProcessingLimit);
        }
        Ok(())
    }

    pub fn receipt_bytes(&self, source: ReceiptSource<'_>) -> Result<Vec<u8>> {
        let (render, preview) = self.descriptors();
        let clip = match self {
            Self::Image(_) => None,
            Self::Clip { header, .. } => Some(Clip {
                duration_ms: header.duration_ms,
                poster_artifact_id: source
                    .request
                    .poster_artifact_id
                    .ok_or(Invalid::Field("clip poster"))?,
            }),
        };
        let receipt = Receipt {
            schema_version: 1,
            artifact_id: Id::new(source.artifact)?,
            binding: source.binding.clone(),
            uploader_id: Id::new(source.uploader)?,
            input: Input {
                digest: source.request.input_digest.clone(),
                bytes: source.request.input_bytes,
                filename: source.request.filename.clone(),
            },
            capture: source.request.capture.clone(),
            rendering_policy: source.kind.policy(),
            variants: Variants {
                render: render.clone(),
                preview: preview.clone(),
            },
            clip,
            retained_at_ms: source.retained_at_ms,
            retain_until_ms: source.retain_until_ms,
        };
        receipt.validate()?;
        canonical_bytes(&receipt)
    }
}
