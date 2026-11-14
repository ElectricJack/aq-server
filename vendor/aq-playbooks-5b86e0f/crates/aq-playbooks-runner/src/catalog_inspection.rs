//! Bounded metadata for storage-owned current catalog inspection. Pure
//! projection helpers do not themselves prove current ownership or freshness.
use crate::{State, StorageError};
use aq_playbooks_model::{DependencySelection, hash_bytes};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum CatalogError {
    #[error("bounded current catalog inspection unavailable")]
    Unsupported,
    #[error("current catalog namespace ownership unavailable or lost")]
    Ownership,
    #[error("current catalog context changed")]
    Conflict,
    #[error("catalog backend: {0}")]
    Backend(String),
}
impl From<StorageError> for CatalogError {
    fn from(error: StorageError) -> Self {
        match error {
            StorageError::Ownership => Self::Ownership,
            StorageError::Conflict => Self::Conflict,
            StorageError::Backend(reason) => Self::Backend(reason),
        }
    }
}

pub const CATALOG_VERSION: &str = "playbooks-catalog-1";
pub const MAX_CATALOG_PAGE: usize = 25;
pub const MAX_CATALOG_BYTES: usize = 262_144;
pub const MAX_CATALOG_ROW_BYTES: usize = 131_072;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogKind {
    Definitions,
    Activations,
}
impl CatalogKind {
    pub fn key(self) -> &'static str {
        match self {
            Self::Definitions => "definitions",
            Self::Activations => "activations",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogStamp {
    pub namespace: String,
    pub kind: CatalogKind,
    #[serde(with = "decimal")]
    pub incarnation: u64,
    #[serde(with = "decimal")]
    pub metadata_revision: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogQuery {
    pub kind: CatalogKind,
    /// An opaque item address from a previous page, never a mutation permit.
    pub exact: Option<String>,
    pub after: Option<String>,
    pub limit: usize,
    pub expected: Option<CatalogStamp>,
}
impl CatalogQuery {
    pub fn first(kind: CatalogKind, limit: usize) -> Self {
        Self {
            kind,
            exact: None,
            after: None,
            limit,
            expected: None,
        }
    }
    pub fn validate(&self) -> Result<(), CatalogError> {
        if !(1..=MAX_CATALOG_PAGE).contains(&self.limit)
            || (self.exact.is_some() && self.after.is_some())
            || self
                .exact
                .iter()
                .chain(self.after.iter())
                .any(|key| !valid_digest(key))
            || (self.after.is_some() && self.expected.is_none())
            || self.expected.as_ref().is_some_and(|stamp| {
                !valid_context(&stamp.namespace)
                    || stamp.incarnation == 0
                    || stamp.metadata_revision == 0
                    || stamp.kind != self.kind
            })
        {
            return Err(CatalogError::Backend(
                "invalid catalog query bounds or context".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogSelection {
    Exact,
    Catalog,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogDependency {
    pub binding: String,
    pub reference_artifact: String,
    pub selection: CatalogSelection,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DefinitionMetadata {
    pub artifact: String,
    pub executable_fingerprint: String,
    pub source_digest: String,
    pub name: String,
    pub scope: String,
    pub compiler: String,
    pub purpose: String,
    pub purpose_truncated: bool,
    pub entries: Vec<String>,
    pub dependencies: BTreeMap<String, CatalogDependency>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogControls {
    pub calls_enabled: bool,
    pub event_starts_enabled: bool,
    pub execution_enabled: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivationMetadata {
    pub binding: String,
    pub artifact: String,
    #[serde(with = "decimal")]
    pub revision: u64,
    pub controls: CatalogControls,
    pub artifact_execution_denied: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "metadata",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum CatalogRecord {
    Definition(DefinitionMetadata),
    Activation(ActivationMetadata),
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogItem {
    pub key: String,
    pub record: CatalogRecord,
}
impl CatalogItem {
    pub fn kind(&self) -> CatalogKind {
        match self.record {
            CatalogRecord::Definition(_) => CatalogKind::Definitions,
            CatalogRecord::Activation(_) => CatalogKind::Activations,
        }
    }
    pub fn validate(&self) -> Result<(), CatalogError> {
        let valid = match &self.record {
            CatalogRecord::Definition(d) => {
                self.key == d.artifact
                    && valid_digest(&d.artifact)
                    && valid_digest(&d.executable_fingerprint)
                    && valid_digest(&d.source_digest)
                    && valid_name(&d.name)
                    && valid_name(&d.scope)
                    && !d.compiler.is_empty()
                    && d.compiler.len() <= 256
                    && d.purpose.len() <= 4096
                    && !d.entries.is_empty()
                    && d.entries.len() <= 128
                    && d.entries.iter().all(|entry| valid_name(entry))
                    && d.entries.windows(2).all(|pair| pair[0] < pair[1])
                    && d.dependencies.len() <= 128
                    && d.dependencies.iter().all(|(name, dependency)| {
                        valid_name(name)
                            && valid_name(&dependency.binding)
                            && valid_digest(&dependency.reference_artifact)
                    })
            }
            CatalogRecord::Activation(a) => {
                self.key == activation_key(&a.binding)
                    && !a.binding.is_empty()
                    && a.binding.len() <= 512
                    && valid_digest(&a.artifact)
                    && a.revision > 0
            }
        };
        if !valid
            || serde_json::to_vec(self)
                .map_err(|_| CatalogError::Unsupported)?
                .len()
                > MAX_CATALOG_ROW_BYTES
        {
            return Err(CatalogError::Unsupported);
        }
        Ok(())
    }
}

/// Activation addresses are opaque stable identifiers. They are not secrets,
/// authentication tokens or permission to inspect a different namespace.
pub fn activation_key(binding: &str) -> String {
    hash_bytes(
        &[
            b"aq-playbooks.catalog.binding.v1\0".as_slice(),
            binding.as_bytes(),
        ]
        .concat(),
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogPage {
    pub api_version: String,
    #[serde(with = "decimal")]
    pub state_revision: u64,
    pub stamp: CatalogStamp,
    pub items: Vec<CatalogItem>,
    pub next_cursor: Option<String>,
}

/// Derived writer state only. A current read must go through the Storage method
/// that checks its actual lease and projected state revision.
pub type CatalogProjection = BTreeMap<(CatalogKind, String), CatalogItem>;
pub fn project(state: &State) -> Result<CatalogProjection, CatalogError> {
    let mut rows = BTreeMap::new();
    for (key, artifact) in &state.artifacts {
        let content = &artifact.content;
        let mut end = content.playbook.purpose.len().min(4096);
        while !content.playbook.purpose.is_char_boundary(end) {
            end -= 1;
        }
        let item = CatalogItem {
            key: key.clone(),
            record: CatalogRecord::Definition(DefinitionMetadata {
                artifact: artifact.digest.clone(),
                executable_fingerprint: artifact.executable_fingerprint.clone(),
                source_digest: content.source.digest.clone(),
                name: content.playbook.id.clone(),
                scope: content.playbook.scope.clone(),
                compiler: content.compiler.clone(),
                purpose: content.playbook.purpose[..end].into(),
                purpose_truncated: end < content.playbook.purpose.len(),
                entries: content.playbook.entries.keys().cloned().collect(),
                dependencies: content
                    .dependencies
                    .iter()
                    .map(|(name, dep)| {
                        (
                            name.clone(),
                            CatalogDependency {
                                binding: dep.binding.clone(),
                                reference_artifact: dep.artifact.clone(),
                                selection: match dep.selection {
                                    DependencySelection::Exact => CatalogSelection::Exact,
                                    DependencySelection::Catalog { .. } => {
                                        CatalogSelection::Catalog
                                    }
                                },
                            },
                        )
                    })
                    .collect(),
            }),
        };
        item.validate()?;
        rows.insert((item.kind(), item.key.clone()), item);
    }
    for (binding, activation) in &state.bindings {
        let item = CatalogItem {
            key: activation_key(binding),
            record: CatalogRecord::Activation(ActivationMetadata {
                binding: binding.clone(),
                artifact: activation.artifact.clone(),
                revision: activation.revision,
                controls: CatalogControls {
                    calls_enabled: activation.controls.calls_enabled,
                    event_starts_enabled: activation.controls.event_starts_enabled,
                    execution_enabled: activation.controls.execution_enabled,
                },
                artifact_execution_denied: state.execution_denied.contains(&activation.artifact),
            }),
        };
        item.validate()?;
        if rows.insert((item.kind(), item.key.clone()), item).is_some() {
            return Err(CatalogError::Unsupported);
        }
    }
    Ok(rows)
}

/// Pack already bounded stored rows into one response. No snapshot lookup occurs.
pub fn make_page(
    query: &CatalogQuery,
    stamp: CatalogStamp,
    state_revision: u64,
    rows: impl IntoIterator<Item = CatalogItem>,
    more: bool,
) -> Result<CatalogPage, CatalogError> {
    query.validate()?;
    if !valid_context(&stamp.namespace)
        || stamp.kind != query.kind
        || stamp.incarnation == 0
        || stamp.metadata_revision == 0
        || stamp.metadata_revision > state_revision
    {
        return Err(CatalogError::Backend(
            "invalid catalog projection stamp".into(),
        ));
    }
    if query
        .expected
        .as_ref()
        .is_some_and(|expected| expected != &stamp)
    {
        return Err(CatalogError::Conflict);
    }
    let mut page = CatalogPage {
        api_version: CATALOG_VERSION.into(),
        state_revision,
        stamp,
        items: vec![],
        next_cursor: None,
    };
    // Reserve more than the final cursor framing without assuming JSON character
    // width. Each single row fits the remaining response by construction.
    let mut bytes = serde_json::to_vec(&page)
        .map_err(|_| CatalogError::Unsupported)?
        .len()
        + 256;
    let mut previous = query.after.clone();
    for item in rows {
        item.validate()?;
        if item.kind() != query.kind
            || previous.as_ref().is_some_and(|key| &item.key <= key)
            || query.exact.as_ref().is_some_and(|key| key != &item.key)
        {
            return Err(CatalogError::Backend("catalog row context mismatch".into()));
        }
        let size = serde_json::to_vec(&item)
            .map_err(|_| CatalogError::Unsupported)?
            .len()
            + 1;
        if bytes + size > MAX_CATALOG_BYTES || page.items.len() == query.limit {
            page.next_cursor = page.items.last().map(|last| last.key.clone());
            break;
        }
        bytes += size;
        previous = Some(item.key.clone());
        page.items.push(item);
    }
    if more && page.next_cursor.is_none() {
        page.next_cursor = page.items.last().map(|last| last.key.clone());
    }
    if serde_json::to_vec(&page)
        .map_err(|_| CatalogError::Unsupported)?
        .len()
        > MAX_CATALOG_BYTES
    {
        return Err(CatalogError::Unsupported);
    }
    Ok(page)
}

pub fn valid_context(value: &str) -> bool {
    !value.is_empty() && value.len() <= 512 && !value.contains('\0')
}
fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}
fn valid_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

mod decimal {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};
    pub fn serialize<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        let value = String::deserialize(deserializer)?;
        if value.is_empty()
            || value.len() > 20
            || (value.len() > 1 && value.starts_with('0'))
            || !value.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(D::Error::custom("invalid unsigned decimal revision"));
        }
        value
            .parse()
            .map_err(|_| D::Error::custom("unsigned decimal revision overflow"))
    }
}
