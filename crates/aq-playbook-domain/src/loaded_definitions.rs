//! Immutable metadata from actual startup configuration, not a current engine
//! catalog. No engine/database snapshot is loaded to serve this projection.
use crate::{Error, Namespace, Result};
use aq_playbook_host::model::Artifact;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

pub const VERSION: &str = "playbooks-loaded-definitions-1";
const MAX_PURPOSE: usize = 4096;

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LoadedDefinition {
    #[schemars(regex(pattern = "^sha256:[0-9a-f]{64}$"))]
    pub artifact: String,
    #[schemars(regex(pattern = "^sha256:[0-9a-f]{64}$"))]
    pub executable_fingerprint: String,
    #[schemars(length(min = 1, max = 128))]
    pub name: String,
    /// At most 4096 UTF-8 bytes; truncation is explicit and scalar-aligned.
    #[schemars(length(max = 4096))]
    pub purpose: String,
    pub purpose_truncated: bool,
    #[schemars(length(min = 1, max = 256))]
    pub compiler: String,
    #[schemars(regex(pattern = "^sha256:[0-9a-f]{64}$"))]
    pub source_digest: String,
    #[schemars(length(min = 1, max = 128))]
    pub entries: Vec<String>,
    pub dependency_pins: BTreeMap<String, String>,
}

#[derive(Clone, Debug)]
pub struct LoadedDefinitions {
    captured_at_ms: i64,
    items: Vec<LoadedDefinition>,
}
impl LoadedDefinitions {
    pub fn capture<'a>(
        artifacts: impl IntoIterator<Item = &'a Artifact>,
        captured_at_ms: i64,
    ) -> Result<Self> {
        let mut items = BTreeMap::new();
        let mut count = 0;
        for artifact in artifacts {
            count += 1;
            if count > 4 || captured_at_ms < 0 {
                return Err(Error::Invalid);
            }
            // Config loading already verified these immutable artifacts. Do not
            // retain registry schemas, captured values, expressions or paths.
            let source = &artifact.content;
            let mut end = source.playbook.purpose.len().min(MAX_PURPOSE);
            while !source.playbook.purpose.is_char_boundary(end) {
                end -= 1;
            }
            let definition = LoadedDefinition {
                artifact: artifact.digest.clone(),
                executable_fingerprint: artifact.executable_fingerprint.clone(),
                name: source.playbook.id.clone(),
                purpose: source.playbook.purpose[..end].to_owned(),
                purpose_truncated: end < source.playbook.purpose.len(),
                compiler: source.compiler.clone(),
                source_digest: source.source.digest.clone(),
                entries: source.playbook.entries.keys().cloned().collect(),
                dependency_pins: source
                    .dependencies
                    .iter()
                    .map(|(name, dependency)| (name.clone(), dependency.artifact.clone()))
                    .collect(),
            };
            items.insert(artifact.digest.clone(), definition);
        }
        if count == 0 {
            return Err(Error::Invalid);
        }
        Ok(Self {
            captured_at_ms,
            items: items.into_values().collect(),
        })
    }
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DefinitionsQuery {
    pub project_id: Option<Uuid>,
}

impl DefinitionsQuery {
    pub fn validate(&self, namespace: &Namespace) -> Result<()> {
        if self.project_id.is_some_and(|id| id != namespace.project_id) {
            return Err(Error::Forbidden);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LoadedNamespace {
    pub id: Uuid,
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    pub generation: String,
    #[schemars(length(min = 1, max = 128))]
    pub name: String,
    pub project_id: Uuid,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DefinitionsPage {
    #[schemars(schema_with = "version_schema")]
    pub api_version: &'static str,
    pub namespace: LoadedNamespace,
    pub server_time_ms: i64,
    pub captured_at_ms: i64,
    #[schemars(schema_with = "source_schema")]
    pub source: &'static str,
    #[schemars(schema_with = "historical_schema")]
    pub current_catalog: bool,
    #[schemars(length(min = 1, max = 4))]
    pub items: Vec<LoadedDefinition>,
}

fn version_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({"type":"string","const":VERSION})
}
fn source_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({"type":"string","const":"runtime_startup_configuration"})
}
fn historical_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({"type":"boolean","const":false})
}

impl LoadedDefinitions {
    pub fn page(&self, namespace: &Namespace, now: i64) -> DefinitionsPage {
        DefinitionsPage {
            api_version: VERSION,
            namespace: LoadedNamespace {
                id: namespace.namespace_id,
                generation: namespace.generation.to_string(),
                name: namespace.name.clone(),
                project_id: namespace.project_id,
            },
            server_time_ms: now,
            captured_at_ms: self.captured_at_ms,
            source: "runtime_startup_configuration",
            current_catalog: false,
            items: self.items.clone(),
        }
    }
}

/// Schema for this separately negotiated metadata profile.
pub fn schema_bytes() -> Vec<u8> {
    serde_json::to_vec_pretty(&schemars::schema_for!(DefinitionsPage))
        .expect("loaded definition schema serialization")
}
