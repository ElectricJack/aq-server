//! Single-task intent and construction of its idempotent Graph mutation.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Digest as _;
use uuid::Uuid;
#[derive(Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Create {
    pub operation_id: Uuid,
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    pub expected_project_revision: String,
    #[schemars(length(min = 1, max = 512))]
    pub title: String,
    #[schemars(length(max = 32768))]
    pub body: String,
    #[schemars(length(max = 8192))]
    pub acceptance_criteria: String,
    #[schemars(length(max = 16))]
    pub labels: Vec<String>,
    pub task_state: TaskState,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 17))]
    pub workspace: Vec<aq_project_policy::workspace::Requirement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 300))]
    pub priority: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 1000))]
    pub story_points: Option<u16>,
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
    pub needs: Option<super::Needs>,
}
#[derive(Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Draft,
    Active,
}
impl Create {
    pub fn valid(&self) -> bool {
        self.valid_identity()
            && self.valid_routing()
            && self.valid_content()
            && aq_project_policy::workspace::valid_list(&self.workspace)
    }
    fn valid_identity(&self) -> bool {
        !self.operation_id.is_nil()
            && self
                .parent_id
                .is_none_or(|parent| !parent.is_nil() && parent != self.operation_id)
            && self.revision().is_some()
    }
    fn valid_routing(&self) -> bool {
        super::hints_valid(
            self.priority,
            [&self.role, &self.intelligence_hint, &self.task_type],
        ) && self
            .story_points
            .is_none_or(|n| (1..=crate::MAX_STORY_POINTS).contains(&n))
    }
    fn valid_content(&self) -> bool {
        super::bounded_label(&self.title, 512)
            && super::bounded_text(&self.body, 32768)
            && super::bounded_text(&self.acceptance_criteria, 8192)
            && super::valid_labels(&self.labels)
    }
    pub fn revision(&self) -> Option<i64> {
        let s = &self.expected_project_revision;
        if s.starts_with('0') || !s.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        s.parse::<i64>().ok().filter(|n| *n > 0)
    }
}
impl Create {
    pub fn mutation(&self, project: Uuid, actor: Uuid, target: Option<Uuid>) -> Value {
        let mut metadata = json!({
            "aq:human_actor_id": actor,
            "aq:human_operation_id": self.operation_id,
            "aq:project_id": project,
        });
        if let Some(node) = target {
            metadata["aq:requested_node_id"] = json!(node);
        }
        super::metadata(
            &mut metadata,
            self.priority,
            [&self.role, &self.intelligence_hint, &self.task_type],
        );
        #[cfg(feature = "project_environments")]
        super::needs(&mut metadata, &self.needs);
        if let Some(points) = self.story_points {
            metadata[crate::STORY_POINTS_KEY] = json!(points);
        }
        let mut operations = vec![json!({
            "op": "create_node", "id": self.operation_id, "kind": "task",
            "title": self.title, "body": self.body, "acceptance_criteria": self.acceptance_criteria,
            "labels": self.labels, "task_state": self.task_state, "metadata": metadata,
        })];
        if let Some(parent) = self.parent_id {
            let digest = sha2::Sha256::digest(
                [self.operation_id.as_bytes().as_slice(), b"feedback-parent"].concat(),
            );
            let mut id = [0; 16];
            id.copy_from_slice(&digest[..16]);
            let edge = Uuid::from_bytes(id);
            operations.push(json!({
                "op":"add_edge", "id":edge, "source":parent, "target":self.operation_id,
                "edge_type":"parent-child", "description":"Feedback draft parent",
                "source_revision":null, "target_revision":null,
            }));
        }
        json!({
            "idempotency_key":format!("aq-human-task-create:{}",self.operation_id),
            "reason":format!("AQ human task create {}",self.operation_id),
            "operations":operations,
        })
    }
}
