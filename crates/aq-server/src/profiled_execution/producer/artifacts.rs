use super::refused;
use crate::{
    AppState,
    error::AppError,
    profiled_execution::artifacts::{self, OwnerKind},
};
use aq_admission::{ArtifactRef, Digest, RetainedArtifact, Text};
use aq_profiled_state::inline_artifact::offer::Admission;
pub(super) use aq_profiled_state::inline_artifact::offer::ArtifactBody;
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
    pub stage: Option<Uuid>,
}
impl RetainedSet {
    pub fn new(project: Uuid, until: u64) -> Self {
        Self {
            entries: vec![],
            project,
            until,
            stage: None,
        }
    }
    pub async fn retain(
        &mut self,
        state: &AppState,
        owner: OwnerKind,
        body: ArtifactBody<'_>,
    ) -> Result<RetainedArtifact, AppError> {
        match body.admission(&self.entries) {
            Admission::New => {}
            Admission::Replay(old) => return Ok(old.clone()),
            Admission::Refused => return Err(refused()),
        }
        let ArtifactBody {
            kind,
            artifact,
            bytes,
        } = body;
        let id = Uuid::new_v5(
            &Uuid::from_bytes(*b"aq-offer-retain!"),
            &canonical(&(self.project, &artifact, kind, self.until))?,
        );
        let retained = artifacts::retain_for(
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
            self.stage,
        )
        .await?;
        self.entries.push((id, retained.clone()));
        Ok(retained)
    }
    pub fn descriptors(&self) -> Vec<RetainedArtifact> {
        self.entries.iter().map(|(_, r)| r.clone()).collect()
    }
}
