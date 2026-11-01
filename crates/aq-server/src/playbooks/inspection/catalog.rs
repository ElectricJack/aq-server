//! Current component-owned catalog metadata. No startup-config projection,
//! snapshot loading, mutation authority or direct component SQL.
use super::bounded;
use crate::{
    authz::RequireOwner,
    playbooks::{
        domain::{Error, Result},
        driver::Driver,
        service::Namespace,
    },
};
use aq_playbook_host::{
    CatalogCallError,
    runner::{self, catalog_inspection as component},
};
use axum::{Extension, Json, extract::Query};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use uuid::Uuid;

pub const VERSION: &str = "playbooks-current-catalog-1";
#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Definitions,
    Activations,
}
impl From<Kind> for runner::CatalogKind {
    fn from(kind: Kind) -> Self {
        match kind {
            Kind::Definitions => Self::Definitions,
            Kind::Activations => Self::Activations,
        }
    }
}
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CatalogQuery {
    pub project_id: Option<Uuid>,
    #[schemars(range(min = 1, max = 25))]
    pub limit: Option<usize>,
    pub exact: Option<String>,
    pub after: Option<String>,
    pub incarnation: Option<String>,
    pub metadata_revision: Option<String>,
}
fn decimal(value: &str) -> Result<u64> {
    if value.is_empty()
        || value.len() > 19
        || value.starts_with('0')
        || !value.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(Error::Invalid);
    }
    value
        .parse::<i64>()
        .ok()
        .filter(|number| *number > 0)
        .map(|number| number as u64)
        .ok_or(Error::Invalid)
}
impl CatalogQuery {
    pub fn component(&self, namespace: &Namespace, kind: Kind) -> Result<runner::CatalogQuery> {
        if self
            .project_id
            .is_some_and(|project| project != namespace.project_id)
        {
            return Err(Error::Forbidden);
        }
        let expected = match (&self.incarnation, &self.metadata_revision) {
            (None, None) => None,
            (Some(incarnation), Some(revision)) => Some(runner::CatalogStamp {
                namespace: namespace.engine_namespace(),
                kind: kind.into(),
                incarnation: decimal(incarnation)?,
                metadata_revision: decimal(revision)?,
            }),
            _ => return Err(Error::Invalid),
        };
        let query = runner::CatalogQuery {
            kind: kind.into(),
            exact: self.exact.clone(),
            after: self.after.clone(),
            limit: self.limit.unwrap_or(25),
            expected,
        };
        query.validate().map_err(|_| Error::Invalid)?;
        Ok(query)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Dependency {
    pub binding: String,
    pub reference_artifact: String,
    pub selection: Selection,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Selection {
    Exact,
    Catalog,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Definition {
    pub artifact: String,
    pub executable_fingerprint: String,
    pub source_digest: String,
    pub name: String,
    pub scope: String,
    pub compiler: String,
    pub purpose: String,
    pub purpose_truncated: bool,
    pub entries: Vec<String>,
    pub dependencies: BTreeMap<String, Dependency>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Controls {
    pub calls_enabled: bool,
    pub event_starts_enabled: bool,
    pub execution_enabled: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Activation {
    pub binding: String,
    pub artifact: String,
    pub revision: String,
    pub controls: Controls,
    pub artifact_execution_denied: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    content = "metadata",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Record {
    Definition(Definition),
    Activation(Activation),
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Item {
    pub key: String,
    pub record: Record,
}
impl Item {
    fn from_component(item: component::CatalogItem, kind: Kind) -> Result<Self> {
        item.validate().map_err(|_| Error::Unknown)?;
        if item.kind() != kind.into() {
            return Err(Error::Unknown);
        }
        let record = match item.record {
            component::CatalogRecord::Definition(d) => Record::Definition(Definition {
                artifact: d.artifact,
                executable_fingerprint: d.executable_fingerprint,
                source_digest: d.source_digest,
                name: d.name,
                scope: d.scope,
                compiler: d.compiler,
                purpose: d.purpose,
                purpose_truncated: d.purpose_truncated,
                entries: d.entries,
                dependencies: d
                    .dependencies
                    .into_iter()
                    .map(|(name, dep)| {
                        (
                            name,
                            Dependency {
                                binding: dep.binding,
                                reference_artifact: dep.reference_artifact,
                                selection: match dep.selection {
                                    component::CatalogSelection::Exact => Selection::Exact,
                                    component::CatalogSelection::Catalog => Selection::Catalog,
                                },
                            },
                        )
                    })
                    .collect(),
            }),
            component::CatalogRecord::Activation(a) => Record::Activation(Activation {
                binding: a.binding,
                artifact: a.artifact,
                revision: a.revision.to_string(),
                controls: Controls {
                    calls_enabled: a.controls.calls_enabled,
                    event_starts_enabled: a.controls.event_starts_enabled,
                    execution_enabled: a.controls.execution_enabled,
                },
                artifact_execution_denied: a.artifact_execution_denied,
            }),
        };
        Ok(Self {
            key: item.key,
            record,
        })
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CatalogNamespace {
    pub id: Uuid,
    pub generation: String,
    pub name: String,
    pub project_id: Uuid,
}
impl From<&Namespace> for CatalogNamespace {
    fn from(n: &Namespace) -> Self {
        Self {
            id: n.namespace_id,
            generation: n.generation.to_string(),
            name: n.name.clone(),
            project_id: n.project_id,
        }
    }
}
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CatalogPage {
    #[schemars(schema_with = "version_schema")]
    pub api_version: &'static str,
    #[schemars(schema_with = "component_version_schema")]
    pub component_api_version: &'static str,
    #[schemars(schema_with = "source_schema")]
    pub source: &'static str,
    #[schemars(schema_with = "current_schema")]
    pub current_catalog: bool,
    pub namespace: CatalogNamespace,
    pub server_time_ms: i64,
    pub kind: Kind,
    pub state_revision: String,
    pub incarnation: String,
    pub metadata_revision: String,
    #[schemars(length(max = 25))]
    pub items: Vec<Item>,
    pub next_cursor: Option<String>,
}
fn version_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({"type":"string","const":"playbooks-current-catalog-1"})
}
fn component_version_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({"type":"string","const":"playbooks-catalog-1"})
}
fn source_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({"type":"string","const":"current_namespace_state"})
}
fn current_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({"type":"boolean","const":true})
}
fn public_page(
    namespace: &Namespace,
    kind: Kind,
    query: &runner::CatalogQuery,
    raw: runner::CatalogPage,
    time: i64,
) -> Result<CatalogPage> {
    if raw.api_version != component::CATALOG_VERSION
        || raw.stamp.namespace != namespace.engine_namespace()
        || raw.stamp.kind != kind.into()
        || raw.items.len() > query.limit
        || raw.stamp.incarnation == 0
        || raw.stamp.incarnation > i64::MAX as u64
        || raw.stamp.metadata_revision == 0
        || raw.stamp.metadata_revision > raw.state_revision
        || raw.state_revision > i64::MAX as u64
        || query
            .expected
            .as_ref()
            .is_some_and(|expected| expected != &raw.stamp)
    {
        return Err(Error::Conflict);
    }
    // Re-apply the component's closed bounds/order/exact-key/stamp validation at
    // the owner-thread boundary; this never loads or derives catalog State.
    let verified = component::make_page(query, raw.stamp, raw.state_revision, raw.items, false)
        .map_err(|_| Error::Unknown)?;
    if verified.next_cursor.is_some() {
        return Err(Error::Unknown);
    }
    if raw
        .next_cursor
        .as_ref()
        .is_some_and(|cursor| verified.items.last().is_none_or(|last| &last.key != cursor))
    {
        return Err(Error::Unknown);
    }
    let mut page = CatalogPage {
        api_version: VERSION,
        component_api_version: component::CATALOG_VERSION,
        source: "current_namespace_state",
        current_catalog: true,
        namespace: CatalogNamespace::from(namespace),
        server_time_ms: time,
        kind,
        state_revision: verified.state_revision.to_string(),
        incarnation: verified.stamp.incarnation.to_string(),
        metadata_revision: verified.stamp.metadata_revision.to_string(),
        items: verified
            .items
            .into_iter()
            .map(|item| Item::from_component(item, kind))
            .collect::<Result<_>>()?,
        next_cursor: raw.next_cursor,
    };
    // AQ adds namespace/source framing. Shorten only this bounded in-memory
    // metadata page; every dropped item remains reachable through the cursor.
    while serde_json::to_vec(&page).map_err(|_| Error::Unknown)?.len() > super::MAX_BYTES {
        if page.items.len() <= 1 {
            return Err(Error::Overloaded);
        }
        page.items.pop();
        page.next_cursor = page.items.last().map(|item| item.key.clone());
    }
    Ok(page)
}
fn owner_error(error: CatalogCallError) -> Error {
    match error {
        CatalogCallError::Catalog(runner::CatalogError::Conflict) => Error::Conflict,
        CatalogCallError::Host(aq_playbook_host::Error::Overloaded) => Error::Overloaded,
        CatalogCallError::Host(aq_playbook_host::Error::Invalid) => Error::Invalid,
        _ => Error::Unknown,
    }
}
impl Driver {
    pub fn catalog_supported(&self) -> bool {
        self.owner.catalog_supported()
    }
    pub async fn inspect_current_catalog(
        &self,
        actor: Uuid,
        kind: Kind,
        query: CatalogQuery,
    ) -> Result<CatalogPage> {
        let query = query.component(&self.service.namespace, kind)?;
        self.service.authorize_inspection(actor).await?;
        let raw = self
            .owner
            .inspect_catalog(query.clone(), Duration::from_secs(2))
            .await;
        self.service.authorize_inspection(actor).await?;
        public_page(
            &self.service.namespace,
            kind,
            &query,
            raw.map_err(owner_error)?,
            self.service.current_time(),
        )
    }
}
pub(super) async fn definitions(
    RequireOwner(actor): RequireOwner,
    Extension(driver): Extension<Arc<Driver>>,
    Query(query): Query<CatalogQuery>,
) -> Result<Json<CatalogPage>> {
    bounded(
        driver
            .inspect_current_catalog(actor.id, Kind::Definitions, query)
            .await?,
    )
}
pub(super) async fn activations(
    RequireOwner(actor): RequireOwner,
    Extension(driver): Extension<Arc<Driver>>,
    Query(query): Query<CatalogQuery>,
) -> Result<Json<CatalogPage>> {
    bounded(
        driver
            .inspect_current_catalog(actor.id, Kind::Activations, query)
            .await?,
    )
}
pub fn schema_bytes() -> Vec<u8> {
    serde_json::to_vec_pretty(&schemars::schema_for!(CatalogPage))
        .expect("current catalog schema serialization")
}

#[cfg(test)]
mod tests {
    use super::*;
    fn namespace() -> Namespace {
        Namespace {
            namespace_id: Uuid::new_v4(),
            generation: 1,
            name: "catalog-fixture".into(),
            workspace_id: Uuid::new_v4(),
            project_id: Uuid::new_v4(),
            owner_user_id: Uuid::new_v4(),
        }
    }
    fn fixture_item(index: usize) -> component::CatalogItem {
        let digest = aq_playbook_host::model::hash_bytes(format!("catalog-{index}").as_bytes());
        component::CatalogItem {
            key: digest.clone(),
            record: component::CatalogRecord::Definition(component::DefinitionMetadata {
                artifact: digest.clone(),
                executable_fingerprint: digest.clone(),
                source_digest: digest,
                name: format!("definition-{index}"),
                scope: "project".into(),
                compiler: "contract-fixture".into(),
                purpose: "bounded metadata".into(),
                purpose_truncated: false,
                entries: vec!["main".into()],
                dependencies: BTreeMap::new(),
            }),
        }
    }
    fn raw(n: &Namespace, items: Vec<component::CatalogItem>) -> runner::CatalogPage {
        runner::CatalogPage {
            api_version: component::CATALOG_VERSION.into(),
            state_revision: 9,
            stamp: runner::CatalogStamp {
                namespace: n.engine_namespace(),
                kind: runner::CatalogKind::Definitions,
                incarnation: 2,
                metadata_revision: 3,
            },
            items,
            next_cursor: None,
        }
    }
    #[test]
    fn closed_query_bounds_and_context_are_lossless() {
        let n = namespace();
        for query in [
            CatalogQuery {
                limit: Some(0),
                ..Default::default()
            },
            CatalogQuery {
                limit: Some(26),
                ..Default::default()
            },
            CatalogQuery {
                after: Some(fixture_item(0).key),
                ..Default::default()
            },
            CatalogQuery {
                incarnation: Some("01".into()),
                metadata_revision: Some("3".into()),
                ..Default::default()
            },
            CatalogQuery {
                incarnation: Some("2".into()),
                ..Default::default()
            },
        ] {
            assert!(query.component(&n, Kind::Definitions).is_err());
        }
        assert!(
            serde_json::from_value::<CatalogQuery>(serde_json::json!({"namespace":"other"}))
                .is_err()
        );
        assert_eq!(
            CatalogQuery {
                project_id: Some(Uuid::new_v4()),
                ..Default::default()
            }
            .component(&n, Kind::Definitions)
            .unwrap_err(),
            Error::Forbidden
        );
        let query = CatalogQuery {
            incarnation: Some("9007199254740993".into()),
            metadata_revision: Some("3".into()),
            ..Default::default()
        }
        .component(&n, Kind::Definitions)
        .unwrap();
        assert_eq!(query.expected.unwrap().incarnation, 9007199254740993);
    }
    #[test]
    fn exact_namespace_kind_stamp_and_opaque_key_are_joined_after_owner_read() {
        let n = namespace();
        let item = fixture_item(1);
        let q = CatalogQuery {
            exact: Some(item.key.clone()),
            incarnation: Some("2".into()),
            metadata_revision: Some("3".into()),
            ..Default::default()
        }
        .component(&n, Kind::Definitions)
        .unwrap();
        let page = public_page(&n, Kind::Definitions, &q, raw(&n, vec![item.clone()]), 1).unwrap();
        let value = serde_json::to_value(page).unwrap();
        assert_eq!(value["current_catalog"], true);
        assert_eq!(value["source"], "current_namespace_state");
        assert_eq!(value["metadata_revision"], "3");
        for wrong in ["namespace", "kind", "incarnation", "metadata"] {
            let mut bad = raw(&n, vec![item.clone()]);
            match wrong {
                "namespace" => bad.stamp.namespace = "other".into(),
                "kind" => bad.stamp.kind = runner::CatalogKind::Activations,
                "incarnation" => bad.stamp.incarnation = 4,
                _ => bad.stamp.metadata_revision = 4,
            };
            assert_eq!(
                public_page(&n, Kind::Definitions, &q, bad, 1).unwrap_err(),
                Error::Conflict
            );
        }
        let mut bad = raw(&n, vec![item.clone()]);
        bad.items[0].key = fixture_item(2).key;
        assert!(public_page(&n, Kind::Definitions, &q, bad, 1).is_err());
        let mut metadata =
            serde_json::to_value(Item::from_component(item, Kind::Definitions).unwrap()).unwrap();
        metadata["record"]["metadata"]["authority"] = serde_json::json!({"actor":"private"});
        assert!(serde_json::from_value::<Item>(metadata).is_err());
    }
    #[test]
    fn wrapper_budget_never_exceeds_component_limit_or_loses_last_key_cursor() {
        let mut n = namespace();
        n.name = "\n".repeat(128);
        let query = runner::CatalogQuery::first(runner::CatalogKind::Definitions, 25);
        let mut rows = (0..25).map(fixture_item).collect::<Vec<_>>();
        rows.sort_by(|a, b| a.key.cmp(&b.key));
        for row in &mut rows {
            let component::CatalogRecord::Definition(d) = &mut row.record else {
                unreachable!()
            };
            d.purpose = "\0".repeat(4096);
        }
        let stamp = raw(&n, vec![]).stamp;
        let raw = component::make_page(&query, stamp, 9, rows, false).unwrap();
        let keys = raw
            .items
            .iter()
            .map(|row| row.key.clone())
            .collect::<Vec<_>>();
        let page = public_page(&n, Kind::Definitions, &query, raw, 1).unwrap();
        assert!(serde_json::to_vec(&page).unwrap().len() <= super::super::MAX_BYTES);
        assert!(!page.items.is_empty());
        assert_eq!(
            page.next_cursor.as_ref(),
            page.items.last().map(|last| &last.key)
        );
        assert_eq!(
            page.items
                .iter()
                .map(|row| row.key.clone())
                .collect::<Vec<_>>(),
            keys[..page.items.len()]
        );
    }
}
