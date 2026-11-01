//! Immutable metadata from actual startup configuration, not a current engine
//! catalog. No engine/database snapshot is loaded to serve this projection.
use super::bounded;
use crate::{
    authz::RequireOwner,
    playbooks::{domain::*, driver::Driver},
};
use aq_playbook_host::model::Artifact;
use axum::{Extension, Json, extract::Query};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Arc};
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
pub(crate) struct LoadedDefinitions {
    captured_at_ms: i64,
    items: Vec<LoadedDefinition>,
}
impl LoadedDefinitions {
    pub(crate) fn capture<'a>(
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

impl Driver {
    /// Only runtime bootstrap calls this, with its actual verified configuration.
    /// Later installation/activation never restamps this historical snapshot.
    pub(crate) fn record_loaded_definitions<'a>(
        &self,
        artifacts: impl IntoIterator<Item = &'a Artifact>,
    ) -> Result<()> {
        let snapshot = LoadedDefinitions::capture(artifacts, self.service.current_time())?;
        self.loaded_definitions
            .set(snapshot)
            .map_err(|_| Error::Conflict)
    }

    pub fn loaded_definitions_available(&self) -> bool {
        self.loaded_definitions.get().is_some()
    }

    pub async fn inspect_loaded_definitions(
        &self,
        actor: Uuid,
        query: DefinitionsQuery,
    ) -> Result<DefinitionsPage> {
        if query
            .project_id
            .is_some_and(|id| id != self.service.namespace.project_id)
        {
            return Err(Error::Forbidden);
        }
        self.service.authorize_inspection(actor).await?;
        let snapshot = self.loaded_definitions.get().ok_or(Error::Unknown)?;
        let page = DefinitionsPage {
            api_version: VERSION,
            namespace: LoadedNamespace {
                id: self.service.namespace.namespace_id,
                generation: self.service.namespace.generation.to_string(),
                name: self.service.namespace.name.clone(),
                project_id: self.service.namespace.project_id,
            },
            server_time_ms: self.service.current_time(),
            captured_at_ms: snapshot.captured_at_ms,
            source: "runtime_startup_configuration",
            current_catalog: false,
            items: snapshot.items.clone(),
        };
        self.service.authorize_inspection(actor).await?;
        Ok(page)
    }
}

pub(super) async fn loaded_definitions(
    RequireOwner(actor): RequireOwner,
    Extension(driver): Extension<Arc<Driver>>,
    Query(query): Query<DefinitionsQuery>,
) -> Result<Json<DefinitionsPage>> {
    bounded(driver.inspect_loaded_definitions(actor.id, query).await?)
}

/// Schema for this separately negotiated metadata profile.
pub fn schema_bytes() -> Vec<u8> {
    serde_json::to_vec_pretty(&schemars::schema_for!(DefinitionsPage))
        .expect("loaded definition schema serialization")
}
