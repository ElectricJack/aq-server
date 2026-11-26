//! Request bodies for the retained-source path through local aq-git.
use crate::{ObjectId, SourceBinding, git_author::GitAuthor};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingGitOperation {
    pub operation_id: Uuid,
    pub request: Value,
    pub result: Option<Value>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub workspace_id: Uuid,
    pub generation: u64,
    pub path: PathBuf,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub workspace_id: Uuid,
    pub workspace_generation: u64,
    pub source: ObjectId,
    pub retention_handle: Uuid,
}
#[derive(Debug, Clone, Serialize)]
pub enum GitAction {
    Register,
    Create,
    Import {
        artifact_handle: Uuid,
    },
    Release {
        handle_id: Uuid,
        generation: u64,
        refs: Vec<RetainedRef>,
    },
    Checkpoint {
        workspace_id: Uuid,
        generation: u64,
        head: ObjectId,
    },
    Export {
        source: ObjectId,
        retention_handle: Uuid,
    },
    Retire {
        workspace_id: Uuid,
        generation: u64,
        head: ObjectId,
    },
    Evict {
        storage_generation: u64,
    },
}
impl GitAction {
    fn parts(&self, b: &SourceBinding) -> (&'static str, Value, u64) {
        match self {
            Self::Register => (
                "repo.register",
                json!({"binding":{"identity":b.logical_identity,"mode":"worker","binding_generation":b.binding_generation,"object_format":"sha1"}}),
                0,
            ),
            Self::Import { artifact_handle } => (
                "artifact.import",
                json!({"artifact_handle":artifact_handle}),
                b.binding_generation,
            ),
            Self::Release {
                handle_id,
                generation,
                refs,
            } => (
                "retention.release",
                json!({"handle_id":handle_id,"expected_generation":generation,"refs":refs,"reason":"central handoff retained and local execution released"}),
                b.binding_generation,
            ),
            Self::Create => (
                "workspace.create",
                json!({"base":b.base}),
                b.binding_generation,
            ),
            Self::Checkpoint {
                workspace_id,
                generation,
                head,
            } => (
                "workspace.checkpoint",
                json!({"workspace_id":workspace_id,"expected_generation":generation,"expected_head":head,"quiescent":true,"mode":"all","commit":{"name":"AQ Worker","email":"worker@invalid","message":"Retained execution checkpoint"}}),
                b.binding_generation,
            ),
            Self::Export {
                source,
                retention_handle,
            } => {
                // The receiver issued the base, so the bundle carries only the
                // commits after it. A source equal to its base has nothing after
                // it and is exported whole (git refuses an empty bundle).
                let prerequisites: &[ObjectId] = if *source == b.base {
                    &[]
                } else {
                    std::slice::from_ref(&b.base)
                };
                (
                    "artifact.export",
                    json!({"source":source,"base":b.base,"retention_handle":retention_handle,"confirmed_prerequisites":prerequisites}),
                    b.binding_generation,
                )
            }
            Self::Retire {
                workspace_id,
                generation,
                head,
            } => (
                "workspace.retire",
                json!({"workspace_id":workspace_id,"expected_generation":generation,"expected_head":head,"quiescent":true}),
                b.binding_generation,
            ),
            Self::Evict { storage_generation } => (
                "repo.cache-evict",
                json!({"expected_storage_generation":storage_generation,"reconstructible":true,"reason":"bounded worker cache reclamation"}),
                b.binding_generation,
            ),
        }
    }
    pub fn request(&self, b: &SourceBinding, id: Uuid) -> Value {
        self.request_as(b, id, None)
    }

    pub fn request_as(&self, b: &SourceBinding, id: Uuid, author: Option<&GitAuthor>) -> Value {
        let (op, mut params, generation) = self.parts(b);
        if let (Self::Checkpoint { .. }, Some(author)) = (self, author) {
            params["commit"]["author"] = json!(author);
        }
        json!({"protocol_version":1,"request_id":id.to_string(),"operation":op,"repository_id":b.repository_id,"operation_id":id,"expected_binding_generation":generation,"params":params})
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetainedRef {
    pub name: String,
    pub oid: String,
}

/// Public artifact-v1 field order is significant for aq-git's canonical digest.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitManifest {
    pub format_version: u32,
    pub repository_id: Uuid,
    pub repository_identity: String,
    pub binding_generation: u64,
    pub transfer_id: Uuid,
    pub object_format: String,
    pub base: ObjectId,
    pub source: ObjectId,
    pub prerequisites: Vec<ObjectId>,
    pub bundle_size: u64,
    pub bundle_sha256: Option<String>,
    pub producer_installation_id: Uuid,
    pub reference_only: bool,
    #[serde(default)]
    pub caller_correlation: Value,
}
impl GitManifest {
    pub fn canonical_digest(&self) -> Result<String> {
        use sha2::{Digest, Sha256};
        Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(self)?)))
    }
}
