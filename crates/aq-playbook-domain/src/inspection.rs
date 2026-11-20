//! Public predicates and projections for the playbook inspector.
use crate::{Error, Namespace, Result, playbook_view};
use aq_playbook_host::runner::InspectionPage;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PageQuery {
    pub after: Option<String>,
    #[schemars(range(min = 1, max = 25))]
    pub limit: Option<usize>,
    pub project_id: Option<Uuid>,
}
impl PageQuery {
    pub fn first(&self, limit: usize) -> Self {
        Self {
            limit: Some(limit),
            project_id: self.project_id,
            after: None,
        }
    }

    pub fn validate(&self, namespace: &Namespace) -> Result<usize> {
        let limit = self.limit.unwrap_or(25);
        if limit == 0
            || limit > 25
            || self
                .after
                .as_ref()
                .is_some_and(|v| v.is_empty() || v.len() > 256 || v.chars().any(char::is_control))
        {
            return Err(Error::Invalid);
        }
        if self.project_id.is_some_and(|id| id != namespace.project_id) {
            return Err(Error::Forbidden);
        }
        Ok(limit)
    }
}
/// X34: the runs list with an optional admission time range. `since_ms` is
/// inclusive and `until_ms` exclusive; either may be omitted.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunsQuery {
    pub after: Option<String>,
    #[schemars(range(min = 1, max = 25))]
    pub limit: Option<usize>,
    pub project_id: Option<Uuid>,
    #[schemars(range(min = 0))]
    pub since_ms: Option<i64>,
    #[schemars(range(min = 1))]
    pub until_ms: Option<i64>,
}
impl RunsQuery {
    pub fn validate(&self) -> Result<()> {
        if self.since_ms.is_some_and(|v| v < 0)
            || self.until_ms.is_some_and(|v| v <= 0)
            || self
                .since_ms
                .zip(self.until_ms)
                .is_some_and(|(since, until)| until <= since)
        {
            return Err(Error::Invalid);
        }
        Ok(())
    }
    pub fn ranged(&self) -> bool {
        self.since_ms.is_some() || self.until_ms.is_some()
    }
    pub fn contains(&self, at_ms: i64) -> bool {
        self.since_ms.is_none_or(|v| at_ms >= v) && self.until_ms.is_none_or(|v| at_ms < v)
    }
    pub fn page(&self, after: Option<String>, limit: Option<usize>) -> PageQuery {
        PageQuery {
            after,
            limit,
            project_id: self.project_id,
        }
    }
}
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct NamespaceView {
    pub id: Uuid,
    pub generation: String,
    pub name: String,
    pub project_id: Uuid,
}
impl From<&Namespace> for NamespaceView {
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
pub struct Page<T> {
    pub api_version: &'static str,
    pub namespace: NamespaceView,
    pub server_time_ms: i64,
    /// Each page is a fresh bounded observation; not a cross-owner snapshot.
    pub revision: Option<String>,
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
}
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct RunView {
    pub id: String,
    pub artifact: String,
    pub entry: String,
    pub step: String,
    pub status: String,
    pub outcome: Option<String>,
    pub reason: Option<String>,
    pub root: String,
    pub parent: Option<String>,
    pub children: Vec<String>,
    pub dependencies: BTreeMap<String, String>,
    pub pending_kind: Option<String>,
    pub admitted_at_ms: i64,
    /// X30/X31: null when the artifact has no AQ registry entry.
    pub definition: Option<playbook_view::DefinitionView>,
    /// X33: set while the pending effect is a human decision request.
    pub waiting: Option<playbook_view::WaitingView>,
    /// Binding the run was admitted on; null for runs not started through a
    /// retained admission (children, event starts) or admitted before it was kept.
    pub binding: Option<String>,
    /// Activation revision the run was admitted at; null when not known.
    pub activation_revision: Option<String>,
    /// Effect intents the run waits on (a pending effect or join members);
    /// private join keys for X33, never serialized.
    #[serde(skip)]
    #[schemars(skip)]
    pub pending_intents: Vec<String>,
}
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct EffectView {
    pub id: String,
    pub step: String,
    pub command: String,
    pub status: String,
    pub deadline_at_ms: Option<i64>,
    pub cancel_requested: bool,
}
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct ReceiptView {
    pub sequence: String,
    pub step: String,
    pub at_ms: i64,
    pub kind: String,
    pub operation: Option<String>,
    pub outcome: Option<String>,
    pub child: Option<String>,
}
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct RunDetail {
    pub api_version: &'static str,
    pub namespace: NamespaceView,
    pub server_time_ms: i64,
    pub run: RunView,
    /// X32: ordered step graph of the run's pinned definition from its entry;
    /// null when no public graph is registered for the artifact.
    pub steps: Option<playbook_view::StepTimeline>,
    pub effects: Page<EffectView>,
    pub receipts: Page<ReceiptView>,
    pub observation_is_atomic: bool,
}
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct AdmissionView {
    pub request_id: Uuid,
    pub status: String,
    pub created_at_ms: i64,
    pub run_id: Option<String>,
    pub activation_revision: Option<String>,
    pub catalog_revision: Option<String>,
    /// Why a held (`unknown`) admission is held; `None` otherwise.
    pub held_reason: Option<String>,
}
fn text(v: &Value, key: &str) -> Result<String> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 512 && !s.chars().any(char::is_control))
        .map(str::to_owned)
        .ok_or(Error::Unknown)
}
fn optional(v: &Value, key: &str) -> Result<Option<String>> {
    match v.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(_) => text(v, key).map(Some),
    }
}
pub fn run(v: Value) -> Result<RunView> {
    let children: Vec<String> =
        serde_json::from_value(v["children"].clone()).map_err(|_| Error::Unknown)?;
    let dependencies: BTreeMap<String, String> =
        serde_json::from_value(v["dependencies"].clone()).map_err(|_| Error::Unknown)?;
    if children.len() > 128
        || dependencies.len() > 128
        || children
            .iter()
            .chain(dependencies.keys())
            .chain(dependencies.values())
            .any(|s| s.len() > 512 || s.chars().any(char::is_control))
    {
        return Err(Error::Overloaded);
    }
    Ok(RunView {
        id: text(&v, "id")?,
        artifact: text(&v, "artifact")?,
        entry: text(&v, "entry")?,
        step: text(&v, "step")?,
        status: text(&v, "status")?,
        outcome: optional(&v, "outcome")?,
        reason: optional(&v, "reason")?,
        root: text(&v, "root")?,
        parent: optional(&v, "parent")?,
        children,
        dependencies,
        pending_kind: optional(&v["pending"], "kind")?,
        admitted_at_ms: v["admitted_at_ms"].as_i64().ok_or(Error::Unknown)?,
        definition: None,
        waiting: None,
        binding: None,
        activation_revision: None,
        pending_intents: pending_intents(&v["pending"])?,
    })
}
fn pending_intents(pending: &Value) -> Result<Vec<String>> {
    let intents: Vec<String> = match pending["kind"].as_str() {
        Some("effect") => optional(pending, "intent")?.into_iter().collect(),
        // Human decisions are awaited as owned-async join members.
        Some("join") => {
            serde_json::from_value(pending["members"].clone()).map_err(|_| Error::Unknown)?
        }
        _ => Vec::new(),
    };
    if intents.len() > 128
        || intents
            .iter()
            .any(|s| s.is_empty() || s.len() > 512 || s.chars().any(char::is_control))
    {
        return Err(Error::Overloaded);
    }
    Ok(intents)
}
pub fn effect(v: Value) -> Result<EffectView> {
    Ok(EffectView {
        id: text(&v, "id")?,
        step: text(&v, "step")?,
        command: text(&v, "command")?,
        status: text(&v, "status")?,
        deadline_at_ms: v["deadline_at_ms"].as_i64(),
        cancel_requested: v["cancel_requested"].as_bool().ok_or(Error::Unknown)?,
    })
}
pub fn receipt(v: Value) -> Result<ReceiptView> {
    Ok(ReceiptView {
        sequence: v["sequence"].as_u64().ok_or(Error::Unknown)?.to_string(),
        step: text(&v, "step")?,
        at_ms: v["at_ms"].as_i64().ok_or(Error::Unknown)?,
        kind: text(&v, "kind")?,
        operation: optional(&v["detail"], "operation")?,
        outcome: optional(&v["detail"], "outcome")?,
        child: optional(&v["detail"], "child")?,
    })
}
pub const VERSION: &str = "playbooks-inspection-1";

pub fn project_page<T>(
    namespace: &Namespace,
    raw: Value,
    limit: usize,
    now: i64,
    project: impl Fn(Value) -> Result<T>,
) -> Result<Page<T>> {
    let raw: InspectionPage = serde_json::from_value(raw).map_err(|_| Error::Unknown)?;
    if raw.items.len() > limit {
        return Err(Error::Unknown);
    }
    let items = raw
        .items
        .into_iter()
        .map(project)
        .collect::<Result<Vec<_>>>()?;
    Ok(Page {
        api_version: VERSION,
        namespace: NamespaceView::from(namespace),
        server_time_ms: now,
        revision: Some(raw.revision.to_string()),
        items,
        next_cursor: raw.next,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn page_bounds_and_project_scope_are_explicit() {
        let n = Namespace {
            namespace_id: Uuid::new_v4(),
            generation: 1,
            name: "test".into(),
            workspace_id: Uuid::new_v4(),
            project_id: Uuid::new_v4(),
            owner_user_id: Uuid::new_v4(),
        };
        assert!(
            PageQuery {
                limit: Some(26),
                ..Default::default()
            }
            .validate(&n)
            .is_err()
        );
        assert!(
            PageQuery {
                after: Some("".into()),
                ..Default::default()
            }
            .validate(&n)
            .is_err()
        );
        assert!(
            PageQuery {
                project_id: Some(Uuid::new_v4()),
                ..Default::default()
            }
            .validate(&n)
            .is_err()
        );
    }
    #[test]
    fn runs_time_range_is_half_open_and_validated() {
        let q = |since_ms, until_ms| RunsQuery {
            since_ms,
            until_ms,
            ..Default::default()
        };
        assert!(q(None, None).validate().is_ok() && !q(None, None).ranged());
        assert!(q(Some(-1), None).validate().is_err());
        assert!(q(None, Some(0)).validate().is_err());
        assert!(q(Some(10), Some(10)).validate().is_err());
        let range = q(Some(10), Some(20));
        assert!(range.validate().is_ok() && range.ranged());
        assert!(range.contains(10) && range.contains(19));
        assert!(!range.contains(9) && !range.contains(20));
        assert!(serde_json::from_value::<RunsQuery>(serde_json::json!({"from": 1})).is_err());
    }
    #[test]
    fn run_projection_keeps_pending_effect_intent_private() {
        let v = serde_json::json!({"id":"r1","artifact":"a","entry":"main","step":"review","status":"waiting","outcome":null,"reason":null,"root":"r1","parent":null,"children":[],"dependencies":{},"pending":{"kind":"effect","intent":"i-7"},"admitted_at_ms":5});
        let view = run(v.clone()).unwrap();
        assert_eq!(view.pending_kind.as_deref(), Some("effect"));
        assert_eq!(view.pending_intents, ["i-7"]);
        assert!(!serde_json::to_string(&view).unwrap().contains("i-7"));
        let mut join = v.clone();
        join["pending"] = serde_json::json!({"kind":"join","members":["i-8","i-9"],"mode":"all"});
        let view = run(join).unwrap();
        assert_eq!(view.pending_intents, ["i-8", "i-9"]);
        assert!(!serde_json::to_string(&view).unwrap().contains("i-8"));
        let mut event = v;
        event["pending"] = serde_json::json!({"kind":"event","intent":"x"});
        assert!(run(event).unwrap().pending_intents.is_empty());
    }
    #[test]
    fn public_receipt_projection_excludes_values_and_private_diagnostics() {
        let v = serde_json::json!({"sequence":9007199254740992_u64,"step":"send","at_ms":1,"kind":"command_result","detail":{"operation":"op","outcome":"queued","public":{"body":"private-canary"},"reason":"secret"}});
        let s = serde_json::to_string(&receipt(v).unwrap()).unwrap();
        assert!(s.contains("9007199254740992"));
        assert!(!s.contains("canary") && !s.contains("secret"));
    }
}
