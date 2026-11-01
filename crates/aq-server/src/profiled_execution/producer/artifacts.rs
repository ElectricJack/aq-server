use super::refused;
use crate::{
    AppState,
    error::AppError,
    profiled_execution::artifacts::{self, OwnerKind},
};
use aq_admission::{ArtifactKind, ArtifactRef, Digest, RetainedArtifact, Text};
use serde::Serialize;
use uuid::Uuid;
pub fn canonical(value: &impl Serialize) -> Result<Vec<u8>, AppError> {
    let mut value = serde_json::to_value(value).map_err(|_| AppError::Internal)?;
    value.sort_all_objects();
    serde_json::to_vec(&value).map_err(|_| AppError::Internal)
}
pub fn reference(prefix: &str, bytes: &[u8]) -> Result<ArtifactRef, AppError> {
    let digest = Digest::of(bytes);
    Ok(ArtifactRef {
        id: Text::new(format!("{prefix}/{}", digest.as_str())).map_err(|_| refused())?,
        version: Text::new("1").map_err(|_| refused())?,
        digest,
    })
}
pub struct RetainedSet {
    pub entries: Vec<(Uuid, RetainedArtifact)>,
    pub project: Uuid,
    pub until: u64,
}
impl RetainedSet {
    pub fn new(project: Uuid, until: u64) -> Self {
        Self {
            entries: vec![],
            project,
            until,
        }
    }
    pub async fn retain(
        &mut self,
        state: &AppState,
        owner: OwnerKind,
        kind: ArtifactKind,
        artifact: ArtifactRef,
        bytes: &[u8],
    ) -> Result<RetainedArtifact, AppError> {
        if artifact.digest != Digest::of(bytes) {
            return Err(refused());
        }
        if let Some((_, old)) = self
            .entries
            .iter()
            .find(|(_, r)| r.artifact.id == artifact.id && r.artifact.version == artifact.version)
        {
            if old.artifact != artifact || old.kind != kind || old.bytes != bytes.len() as u64 {
                return Err(refused());
            }
            return Ok(old.clone());
        }
        if self
            .entries
            .iter()
            .any(|(_, old)| old.artifact.digest == artifact.digest)
            || self
                .entries
                .iter()
                .try_fold(bytes.len() as u64, |sum, (_, r)| sum.checked_add(r.bytes))
                .is_none_or(|n| n > 16_777_216)
        {
            return Err(refused());
        }
        // Two Prepare slots remain for the separately hydrated Git manifest/bundle.
        if self.entries.len() >= 126 {
            return Err(refused());
        }
        let id = Uuid::new_v5(
            &Uuid::from_bytes(*b"aq-offer-retain!"),
            &canonical(&(self.project, &artifact, kind, self.until))?,
        );
        let retained = artifacts::retain(
            state,
            artifacts::Retain {
                artifact_id: id,
                project_id: self.project,
                owner,
                artifact,
                kind,
                bytes,
                retain_until_ms: self.until,
            },
        )
        .await?;
        self.entries.push((id, retained.clone()));
        Ok(retained)
    }
    pub fn descriptors(&self) -> Vec<RetainedArtifact> {
        self.entries.iter().map(|(_, r)| r.clone()).collect()
    }
}
