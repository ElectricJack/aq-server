//! Eligibility of inline bodies in one profiled offer's bounded artifact set.
use aq_admission::{ArtifactKind, ArtifactRef, Digest, RetainedArtifact};
use uuid::Uuid;

pub struct ArtifactBody<'a> {
    pub kind: ArtifactKind,
    pub artifact: ArtifactRef,
    pub bytes: &'a [u8],
}
pub enum Admission<'a> {
    New,
    Replay(&'a RetainedArtifact),
    Refused,
}
impl<'a> ArtifactBody<'a> {
    pub fn new(kind: ArtifactKind, artifact: ArtifactRef, bytes: &'a [u8]) -> Self {
        Self {
            kind,
            artifact,
            bytes,
        }
    }
    pub fn admission<'s>(&self, entries: &'s [(Uuid, RetainedArtifact)]) -> Admission<'s> {
        if self.artifact.digest != Digest::of(self.bytes) {
            return Admission::Refused;
        }
        let prior = entries.iter().map(|(_, r)| r).find(|r| {
            r.artifact.id == self.artifact.id && r.artifact.version == self.artifact.version
        });
        if let Some(prior) = prior {
            return self.replay(prior);
        }
        if self.fits(entries) {
            Admission::New
        } else {
            Admission::Refused
        }
    }
    fn replay<'s>(&self, prior: &'s RetainedArtifact) -> Admission<'s> {
        let identity = prior.artifact == self.artifact && prior.kind == self.kind;
        if identity && prior.bytes == self.bytes.len() as u64 {
            Admission::Replay(prior)
        } else {
            Admission::Refused
        }
    }
    fn fits(&self, entries: &[(Uuid, RetainedArtifact)]) -> bool {
        // Leave two Prepare slots for the separately hydrated Git manifest and bundle.
        if entries.len() >= 126 {
            return false;
        }
        let duplicate = entries
            .iter()
            .any(|(_, r)| r.artifact.digest == self.artifact.digest);
        let total = entries
            .iter()
            .try_fold(self.bytes.len() as u64, |sum, (_, r)| {
                sum.checked_add(r.bytes)
            });
        !duplicate && total.is_some_and(|n| n <= 16_777_216)
    }
}
