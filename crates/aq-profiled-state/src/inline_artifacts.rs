//! Inline artifact ownership and retention receipts.
use aq_admission::{ArtifactKind, ArtifactRef, RetainedArtifact};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerKind {
    Catalog,
    GraphSnapshot,
    Composition,
    Selection,
    Outcome,
    SourceRetention,
    Startup,
}
impl OwnerKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Catalog => "catalog",
            Self::GraphSnapshot => "graph_snapshot",
            Self::Composition => "composition",
            Self::Selection => "selection",
            Self::Outcome => "outcome",
            Self::SourceRetention => "source_retention",
            Self::Startup => "startup",
        }
    }
    pub fn accepts(self, kind: ArtifactKind) -> bool {
        matches!(
            (self, kind),
            (Self::Catalog, ArtifactKind::Definition)
                | (Self::GraphSnapshot, ArtifactKind::Context)
                | (Self::Composition, ArtifactKind::Composition)
                | (Self::Composition, ArtifactKind::Definition)
                | (Self::Selection, ArtifactKind::Selection)
                | (Self::Outcome, ArtifactKind::Outcome)
                | (Self::SourceRetention, ArtifactKind::Context)
                | (Self::SourceRetention, ArtifactKind::Source)
                | (Self::Startup, ArtifactKind::Context)
                | (Self::Startup, ArtifactKind::Definition)
        )
    }
}
/// Stable artifact_id is the retain operation identity. Changed reuse conflicts.
pub struct Retain<'a> {
    pub artifact_id: Uuid,
    pub project_id: Uuid,
    pub owner: OwnerKind,
    pub artifact: ArtifactRef,
    pub kind: ArtifactKind,
    pub bytes: &'a [u8],
    pub retain_until_ms: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub schema_version: u32,
    pub artifact_id: Uuid,
    pub project_id: Uuid,
    pub owner_kind: OwnerKind,
    pub artifact: ArtifactRef,
    pub kind: ArtifactKind,
    pub bytes: u64,
    pub retained_at_ms: u64,
    pub retain_until_ms: u64,
}
pub struct Retained {
    pub artifact_id: Uuid,
    pub project_id: Uuid,
    pub owner: OwnerKind,
    pub descriptor: RetainedArtifact,
    pub receipt: Vec<u8>,
    pub bytes: Vec<u8>,
}
