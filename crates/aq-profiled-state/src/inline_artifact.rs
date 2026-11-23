//! Canonical, database-backed receipts for immutable inline artifact bodies.
use aq_admission::{ArtifactKind, ArtifactRef, Digest, RetainedArtifact, Text};
use serde::{Deserialize, Serialize};
use sqlx::{Row, postgres::PgRow};
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
    fn accepts(self, kind: ArtifactKind) -> bool {
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

impl Retain<'_> {
    pub fn valid(&self) -> bool {
        let scope = !self.artifact_id.is_nil() && !self.project_id.is_nil();
        if !scope || !self.owner.accepts(self.kind) {
            return false;
        }
        (1..=2_097_152).contains(&self.bytes.len())
            && Digest::of(self.bytes) == self.artifact.digest
    }

    pub fn matches(&self, old: &Retained) -> bool {
        let owner = old.project_id == self.project_id && old.owner == self.owner;
        let identity = old.descriptor.artifact == self.artifact && old.descriptor.kind == self.kind;
        let retained =
            old.descriptor.retain_until_ms == self.retain_until_ms && old.bytes == self.bytes;
        owner && identity && retained
    }

    pub fn receipt(&self, time: u64) -> Receipt {
        Receipt {
            schema_version: 1,
            artifact_id: self.artifact_id,
            project_id: self.project_id,
            owner_kind: self.owner,
            artifact: self.artifact.clone(),
            kind: self.kind,
            bytes: self.bytes.len() as u64,
            retained_at_ms: time,
            retain_until_ms: self.retain_until_ms,
        }
    }
}
impl Receipt {
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        let mut value = serde_json::to_value(self)?;
        value.sort_all_objects();
        serde_json::to_vec(&value)
    }
    pub fn descriptor(&self, encoded: &[u8]) -> RetainedArtifact {
        RetainedArtifact {
            artifact: self.artifact.clone(),
            kind: self.kind,
            bytes: self.bytes,
            retention_receipt: receipt_ref(self.artifact_id, encoded),
            retained_at_ms: self.retained_at_ms,
            retain_until_ms: self.retain_until_ms,
        }
    }
    pub fn matches_descriptor(&self, descriptor: &RetainedArtifact) -> bool {
        let identity = self.artifact == descriptor.artifact && self.kind == descriptor.kind;
        let window = self.retained_at_ms..self.retain_until_ms;
        identity
            && self.bytes == descriptor.bytes
            && window == (descriptor.retained_at_ms..descriptor.retain_until_ms)
    }
    pub fn matches_record(&self, row: &PgRow, body_len: usize) -> bool {
        let scope = self.schema_version == 1
            && !self.project_id.is_nil()
            && self.owner_kind.accepts(self.kind);
        let owner = (
            row.get::<String, _>("owner_key"),
            row.get::<String, _>("owner_kind"),
        );
        let expected = (
            self.project_id.to_string(),
            self.owner_kind.name().to_owned(),
        );
        let window = self.retained_at_ms..self.retain_until_ms;
        let persisted =
            row.get::<i64, _>("created_at_ms") as u64..row.get::<i64, _>("retain_until_ms") as u64;
        let content = row.get::<i64, _>("byte_length") as u64 == body_len as u64
            && row.get::<String, _>("content_digest") == self.artifact.digest.as_str();
        let recorded =
            scope && owner == expected && self.artifact_id == row.get::<Uuid, _>("artifact_id");
        recorded && window == persisted && content
    }
}
pub fn receipt_ref(id: Uuid, bytes: &[u8]) -> ArtifactRef {
    ArtifactRef {
        id: Text::new(format!("aq.inline-retention/{id}")).expect("bounded UUID identity"),
        version: Text::new("1").unwrap(),
        digest: Digest::of(bytes),
    }
}

/// Offer assignment and retention interval for inline artifact linkage.
pub struct LinkScope {
    pub attempt: Uuid,
    project: Uuid,
    pub required: std::ops::Range<u64>,
}
impl LinkScope {
    pub fn new(attempt: Uuid, project: Uuid, required: std::ops::Range<u64>) -> Self {
        Self {
            attempt,
            project,
            required,
        }
    }
    pub fn ordered_ids(&self, artifacts: &[Uuid]) -> Option<Vec<Uuid>> {
        let assigned = !self.attempt.is_nil() && !self.project.is_nil();
        let bounded = artifacts.len() <= 128 && !self.required.is_empty();
        if !assigned || !bounded {
            return None;
        }
        let mut ids = artifacts.to_vec();
        ids.sort();
        ids.dedup();
        (ids.len() == artifacts.len()).then_some(ids)
    }
    pub fn covers(&self, item: &Retained) -> bool {
        item.project_id == self.project && item.descriptor.retain_until_ms >= self.required.end
    }
}
