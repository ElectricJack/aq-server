//! Typed atomic creation batches; Graph remains the sole task/edge authority.
pub(crate) mod supervisor;
use super::graph_actions::{Current, GraphPayload};
use super::*;
use crate::dashboard_api::errors::ApiError;
use schemars::JsonSchema;
use std::collections::BTreeSet;

pub(super) fn hints_valid(priority: Option<u16>, hints: [&Option<String>; 3]) -> bool {
    priority.is_none_or(|n| (1..=300).contains(&n))
        && hints.iter().all(|h| {
            h.as_ref().is_none_or(|s| {
                !s.trim().is_empty() && s.len() <= 128 && !s.chars().any(char::is_control)
            })
        })
}
pub(super) fn metadata(
    value: &mut Value,
    priority: Option<u16>,
    profile: &Option<String>,
    intelligence: &Option<String>,
    task_type: &Option<String>,
) {
    if let Some(n) = priority {
        value["aq:priority"] = json!(n);
    }
    for (key, hint) in [
        ("aq:profile_hint", profile),
        ("aq:intelligence_hint", intelligence),
        ("aq:task_type", task_type),
    ] {
        if let Some(hint) = hint {
            value[key] = json!(hint);
        }
    }
}

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Task {
    pub id: Uuid,
    #[schemars(length(min = 1, max = 512))]
    pub title: String,
    #[serde(default)]
    #[schemars(length(max = 32768))]
    pub body: String,
    #[serde(default)]
    #[schemars(length(max = 8192))]
    pub acceptance_criteria: String,
    #[serde(default)]
    #[schemars(length(max = 16))]
    pub labels: Vec<String>,
    #[serde(default)]
    pub task_state: InitialState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 300))]
    pub priority: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 128))]
    pub profile_hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 128))]
    pub intelligence_hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 128))]
    pub task_type: Option<String>,
}
#[derive(Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum InitialState {
    Draft,
    #[default]
    Active,
}
impl Task {
    fn valid(&self) -> bool {
        !self.id.is_nil()
            && !self.title.trim().is_empty()
            && self.title.chars().count() <= 512
            && !self.title.chars().any(char::is_control)
            && self.body.len() <= 32768
            && !self.body.contains('\0')
            && self.acceptance_criteria.len() <= 8192
            && !self.acceptance_criteria.contains('\0')
            && self.labels.len() <= 16
            && self
                .labels
                .iter()
                .all(|s| !s.trim().is_empty() && s.len() <= 128 && !s.chars().any(char::is_control))
            && self.labels.iter().collect::<BTreeSet<_>>().len() == self.labels.len()
            && self.parent_id.is_none_or(|p| !p.is_nil() && p != self.id)
            && hints_valid(
                self.priority,
                [&self.profile_hint, &self.intelligence_hint, &self.task_type],
            )
    }
    fn operation(&self, c: &Current<'_>) -> Value {
        let mut m = json!({"aq:project_id":c.project});
        if let Some(actor) = c.supervisor_actor {
            m["aq:supervisor_actor_id"] = json!(actor);
            m["aq:supervisor_session_id"] = json!(c.actor);
        } else {
            m["aq:human_actor_id"] = json!(c.actor);
        }
        metadata(
            &mut m,
            self.priority,
            &self.profile_hint,
            &self.intelligence_hint,
            &self.task_type,
        );
        json!({"op":"create_node","id":self.id,"kind":"task","title":self.title,"body":self.body,"acceptance_criteria":self.acceptance_criteria,"labels":self.labels,"task_state":self.task_state,"metadata":m})
    }
}
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Dependency {
    pub id: Uuid,
    pub source: Uuid,
    pub target: Uuid,
}
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct GraphCreate {
    pub operation_id: Uuid,
    pub project_id: Uuid,
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    pub expected_version: String,
    #[schemars(length(min = 1, max = 100))]
    pub tasks: Vec<Task>,
    #[serde(default)]
    #[schemars(length(max = 99))]
    pub edges: Vec<Dependency>,
}
fn parent_edge(operation: Uuid, task: Uuid) -> Uuid {
    use sha2::Digest;
    Uuid::from_bytes(
        sha2::Sha256::digest(
            [
                operation.as_bytes().as_slice(),
                task.as_bytes().as_slice(),
                b"parent",
            ]
            .concat(),
        )[..16]
            .try_into()
            .expect("digest length"),
    )
}
impl GraphPayload for GraphCreate {
    const KIND: &'static str = "task.graph.create";
    const ENTITY: &'static str = "project";
    fn operation_id(&self) -> Uuid {
        self.operation_id
    }
    fn project_id(&self) -> Uuid {
        self.project_id
    }
    fn expected(&self) -> &str {
        &self.expected_version
    }
    fn valid(&self) -> bool {
        let nodes: BTreeSet<_> = self.tasks.iter().map(|t| t.id).collect();
        let mut ids = nodes.clone();
        let mut pairs = BTreeSet::new();
        !self.operation_id.is_nil()
            && !self.project_id.is_nil()
            && !self.tasks.is_empty()
            && self.tasks.len()
                + self.edges.len()
                + self.tasks.iter().filter(|t| t.parent_id.is_some()).count()
                <= 100
            && nodes.len() == self.tasks.len()
            && self.tasks.iter().all(Task::valid)
            && self.edges.iter().all(|e| {
                !e.id.is_nil()
                    && !e.source.is_nil()
                    && !e.target.is_nil()
                    && e.source != e.target
                    && (nodes.contains(&e.source) || nodes.contains(&e.target))
                    && ids.insert(e.id)
                    && pairs.insert((e.source, e.target))
            })
            && self
                .tasks
                .iter()
                .filter(|t| t.parent_id.is_some())
                .all(|t| ids.insert(parent_edge(self.operation_id, t.id)))
    }
    fn tasks(&self) -> Vec<Uuid> {
        let new: BTreeSet<_> = self.tasks.iter().map(|t| t.id).collect();
        self.edges
            .iter()
            .flat_map(|e| [e.source, e.target])
            .chain(self.tasks.iter().filter_map(|t| t.parent_id))
            .filter(|id| !new.contains(id))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }
    fn operations(&self, c: &Current<'_>) -> std::result::Result<Vec<Value>, ApiError> {
        let mut ops: Vec<_> = self.tasks.iter().map(|t| t.operation(c)).collect();
        for task in &self.tasks {
            if let Some(parent) = task.parent_id {
                ops.push(json!({"op":"add_edge","id":parent_edge(self.operation_id,task.id),"source":parent,"target":task.id,"edge_type":"parent-child","description":"Enqueued task parent","source_revision":null,"target_revision":null}));
            }
        }
        for e in &self.edges {
            ops.push(json!({"op":"add_edge","id":e.id,"source":e.source,"target":e.target,"edge_type":"blocks","description":"","source_revision":null,"target_revision":null}));
        }
        Ok(ops)
    }
}
