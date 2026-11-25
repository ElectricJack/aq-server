//! Prepare bytes under the storage guard and publish a single generation.

use super::{Error, files, media};
use media::PreparedMedia;
use uuid::Uuid;

pub(super) struct Generation {
    pub id: Uuid,
    pub kind: media::MediaKind,
    pub input: media::Digest,
    pub size: usize,
    pub limits: media::Limits,
}

impl Generation {
    pub(super) fn finalize(
        &self,
        root: &files::Root,
        poster: Option<(Uuid, media::Variant)>,
    ) -> Result<(PreparedMedia, u64), JobError> {
        let published = root.generation(self.id, true).is_ok();
        let bytes = root.input(self.id, published, self.kind, self.size, &self.input)?;
        let prepared = match self.kind {
            media::MediaKind::Image => media::prepare_image(&bytes).map_err(JobError::Reject)?,
            media::MediaKind::Video => self.prepare_clip(root, bytes, poster)?,
        };
        prepared
            .enforce_limits(&self.limits)
            .map_err(JobError::Reject)?;
        self.persist_variants(root, &prepared, published)?;
        let charge = root.charge(self.id, self.kind)?;
        if charge > 10_485_760 {
            return Err(JobError::Reject(media::Rejection::StorageFailure));
        }
        root.point("before_receipt_commit")?;
        Ok((prepared, charge))
    }

    fn prepare_clip(
        &self,
        root: &files::Root,
        bytes: Vec<u8>,
        poster: Option<(Uuid, media::Variant)>,
    ) -> Result<PreparedMedia, JobError> {
        let header = media::prepare_clip(&bytes, &self.limits).map_err(JobError::Reject)?;
        let (poster_id, preview) =
            poster.ok_or(JobError::Reject(media::Rejection::PosterUnavailable))?;
        let preview_bytes = root
            .variant(poster_id, "preview.png", &preview)
            .map_err(|_| JobError::Reject(media::Rejection::PosterUnavailable))?;
        let render = media::Variant {
            digest: self.input.clone(),
            bytes: self.size as u64,
            mime: media::Mime::Mp4,
            width: header.width,
            height: header.height,
        };
        Ok(PreparedMedia::Clip {
            header,
            render,
            preview,
            bytes,
            preview_bytes,
        })
    }

    fn persist_variants(
        &self,
        root: &files::Root,
        prepared: &PreparedMedia,
        published: bool,
    ) -> Result<(), Error> {
        if published {
            let (render, preview) = prepared.descriptors();
            root.variant(self.id, files::render_name(render.mime), render)?;
            root.variant(self.id, "preview.png", preview)?;
            return root.confirm_published(self.id);
        }
        root.remove_variants(self.id)?;
        match prepared {
            PreparedMedia::Image(image) => root.variants(self.id, image)?,
            PreparedMedia::Clip {
                bytes,
                preview_bytes,
                ..
            } => {
                root.clip_variants(self.id, bytes, preview_bytes)?;
            }
        }
        root.publish(self.id)
    }
}

pub(super) enum JobError {
    Storage(Error),
    Reject(media::Rejection),
}

impl From<Error> for JobError {
    fn from(value: Error) -> Self {
        Self::Storage(value)
    }
}
