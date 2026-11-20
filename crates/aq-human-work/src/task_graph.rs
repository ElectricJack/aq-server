//! Typed atomic task creation and Graph mutation planning.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

/// Graph CAS guards assert an existing revision without changing a node.
pub fn leading_guards(operations: &[Value]) -> usize {
    operations
        .iter()
        .take_while(|op| {
            op["op"] == "update_node"
                && op["patch"]
                    .as_object()
                    .is_some_and(|patch| patch.is_empty())
        })
        .count()
}

#[derive(Clone, Copy)]
pub enum Author {
    Agent { attempt: Uuid, task: Uuid },
    Supervisor { actor: Uuid, session: Uuid },
    Owner(Uuid),
}

pub fn hints_valid(priority: Option<u16>, hints: [&Option<String>; 3]) -> bool {
    priority.is_none_or(|n| (1..=300).contains(&n))
        && hints.iter().all(|h| {
            h.as_ref().is_none_or(|s| {
                !s.trim().is_empty() && s.len() <= 128 && !s.chars().any(char::is_control)
            })
        })
}
/// Spec 0017 §5.4: the capabilities a task needs from its environment,
/// stored as `aq:needs`: distinct bounded labels. Empty names none.
#[cfg(feature = "project_environments")]
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "Vec<String>")]
pub struct Needs(#[schemars(length(max = 32))] pub Vec<String>);
#[cfg(feature = "project_environments")]
impl TryFrom<Vec<String>> for Needs {
    type Error = String;
    fn try_from(needs: Vec<String>) -> std::result::Result<Self, String> {
        aq_project_policy::environments::check_needs(&needs).map(|()| Self(needs))
    }
}
#[cfg(feature = "project_environments")]
pub fn needs(value: &mut Value, needs: &Option<Needs>) {
    if let Some(Needs(needs)) = needs.as_ref().filter(|n| !n.0.is_empty()) {
        value[aq_project_policy::environments::NEEDS_KEY] = json!(needs);
    }
}
/// `role` is the role the task names (spec 0014 §5.5), stored as `aq:role`.
pub fn metadata(
    value: &mut Value,
    priority: Option<u16>,
    role: &Option<String>,
    intelligence: &Option<String>,
    task_type: &Option<String>,
) {
    if let Some(n) = priority {
        value["aq:priority"] = json!(n);
    }
    for (key, hint) in [
        (aq_role_settings::tasks::ROLE_KEY, role),
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
pub struct Task {
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
    /// The role the task expects; it must exist. Absent: the project's
    /// default role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 128))]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 128))]
    pub intelligence_hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 128))]
    pub task_type: Option<String>,
    /// What the task needs from its environment (spec 0017), as `aq:needs`.
    #[cfg(feature = "project_environments")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub needs: Option<Needs>,
}
#[derive(Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InitialState {
    Draft,
    #[default]
    Active,
}
impl Task {
    fn valid(&self) -> bool {
        !self.id.is_nil()
            && !self.title.trim().is_empty()
            && self.title.len() <= 512
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
                [&self.role, &self.intelligence_hint, &self.task_type],
            )
    }
    fn operation(&self, project: Uuid, author: Author) -> Value {
        let mut m = json!({"aq:project_id":project});
        match author {
            Author::Agent { attempt, task } => {
                m["aq:agent_attempt_id"] = json!(attempt);
                m["aq:agent_task_id"] = json!(task);
            }
            Author::Supervisor { actor, session } => {
                m["aq:supervisor_actor_id"] = json!(actor);
                m["aq:supervisor_session_id"] = json!(session);
            }
            Author::Owner(actor) => {
                m["aq:human_actor_id"] = json!(actor);
            }
        }
        metadata(
            &mut m,
            self.priority,
            &self.role,
            &self.intelligence_hint,
            &self.task_type,
        );
        #[cfg(feature = "project_environments")]
        needs(&mut m, &self.needs);
        json!({"op":"create_node","id":self.id,"kind":"task","title":self.title,"body":self.body,"acceptance_criteria":self.acceptance_criteria,"labels":self.labels,"task_state":self.task_state,"metadata":m})
    }
}
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Dependency {
    pub id: Uuid,
    pub source: Uuid,
    pub target: Uuid,
}
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GraphCreate {
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
impl GraphCreate {
    pub fn valid(&self) -> bool {
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
    pub fn roles(&self) -> Vec<&str> {
        self.tasks
            .iter()
            .filter_map(|t| t.role.as_deref())
            .collect()
    }
    pub fn external_tasks(&self) -> Vec<Uuid> {
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
    pub fn plan_operations(&self, project: Uuid, author: Author) -> Vec<Value> {
        let mut ops: Vec<_> = self
            .tasks
            .iter()
            .map(|t| t.operation(project, author))
            .collect();
        for task in &self.tasks {
            if let Some(parent) = task.parent_id {
                ops.push(json!({"op":"add_edge","id":parent_edge(self.operation_id,task.id),"source":parent,"target":task.id,"edge_type":"parent-child","description":"Enqueued task parent","source_revision":null,"target_revision":null}));
            }
        }
        for e in &self.edges {
            ops.push(json!({"op":"add_edge","id":e.id,"source":e.source,"target":e.target,"edge_type":"blocks","description":"","source_revision":null,"target_revision":null}));
        }
        ops
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DraftBatchError {
    Invalid,
    OutsideScope,
}

pub struct DraftTask {
    pub id: Uuid,
    pub parent: Uuid,
    /// The first parent that already exists and must be checked in Graph.
    pub ancestor: Uuid,
    pub new_levels: u32,
}

impl GraphCreate {
    /// Validate the draft-only constraints and find each existing ancestor.
    pub fn draft_tasks(&self) -> Result<Vec<DraftTask>, DraftBatchError> {
        use DraftBatchError::{Invalid, OutsideScope};
        if !self.valid() {
            return Err(Invalid);
        }
        let parents: BTreeMap<_, _> = self
            .tasks
            .iter()
            .map(|task| (task.id, task.parent_id))
            .collect();
        if self
            .edges
            .iter()
            .any(|edge| !parents.contains_key(&edge.source) || !parents.contains_key(&edge.target))
        {
            return Err(OutsideScope);
        }
        self.tasks
            .iter()
            .map(|task| {
                if !matches!(task.task_state, InitialState::Draft)
                    || task.intelligence_hint.is_some()
                {
                    return Err(OutsideScope);
                }
                #[cfg(feature = "project_environments")]
                if task.needs.is_some() {
                    return Err(OutsideScope);
                }
                let parent = task.parent_id.ok_or(OutsideScope)?;
                let mut ancestor = parent;
                let mut chain = BTreeSet::from([task.id]);
                while let Some(parent) = parents.get(&ancestor) {
                    if !chain.insert(ancestor) {
                        return Err(Invalid);
                    }
                    ancestor = (*parent).ok_or(OutsideScope)?;
                }
                Ok(DraftTask {
                    id: task.id,
                    parent,
                    ancestor,
                    new_levels: chain.len() as u32,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_draft_graph_plans_nested_tasks_under_live_ancestors() {
        let [project, operation, parent, child, grandchild] = [1, 2, 3, 4, 5].map(Uuid::from_u128);
        let mut graph: GraphCreate = serde_json::from_value(json!({
            "operation_id":operation,"project_id":project,"expected_version":"1",
            "tasks":[
                {"id":grandchild,"parent_id":child,"title":"Nested","task_state":"draft"},
                {"id":child,"parent_id":parent,"title":"Root","task_state":"draft","labels":["test"]}
            ]
        })).unwrap();
        let drafts = graph.draft_tasks().unwrap();
        assert_eq!((drafts[0].ancestor, drafts[0].new_levels), (parent, 2));
        assert_eq!((drafts[1].ancestor, drafts[1].new_levels), (parent, 1));
        let ops = graph.plan_operations(
            project,
            Author::Agent {
                attempt: operation,
                task: parent,
            },
        );
        assert_eq!(ops[1]["labels"], json!(["test"]));
        assert_eq!(ops[1]["metadata"]["aq:agent_attempt_id"], json!(operation));
        assert_eq!(ops[2]["source"], json!(child));
        graph.tasks[1].parent_id = Some(grandchild);
        assert!(matches!(graph.draft_tasks(), Err(DraftBatchError::Invalid)));
    }
}
