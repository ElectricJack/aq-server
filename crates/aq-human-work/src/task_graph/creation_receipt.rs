//! Verify the Graph result against the exact task creation and optional parent edge that were sent.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub workspace_id: Uuid,
    pub change_set_id: Uuid,
    pub task_id: Uuid,
    pub version: String,
    pub content_revision: String,
}

pub struct Creation<'a> {
    pub task: Uuid,
    pub workspace: Uuid,
    pub actor: &'a Value,
}

impl Creation<'_> {
    pub fn receipt(
        &self,
        request: &Value,
        result: &Value,
        conforms: impl Fn(&str, &Value) -> bool,
    ) -> Option<Receipt> {
        let operations = request["operations"].as_array()?;
        let results = result["results"].as_array()?;
        let (node, expected) = match (results.as_slice(), operations.as_slice()) {
            ([node], [create]) => (node, create),
            ([node, edge], [create, parent])
                if conforms("Edge", edge) && self.parent_edge(edge, parent) =>
            {
                (node, create)
            }
            _ => return None,
        };
        if !conforms("Node", node) || !self.created_task(node, expected) {
            return None;
        }
        let change_set_id = result["change_set_id"].as_str()?.parse::<Uuid>().ok()?;
        if change_set_id.is_nil() {
            return None;
        }
        Some(Receipt {
            workspace_id: self.workspace,
            change_set_id,
            task_id: self.task,
            version: positive_revision(&node["version"])?,
            content_revision: positive_revision(&node["content_revision"])?,
        })
    }

    fn parent_edge(&self, edge: &Value, expected: &Value) -> bool {
        let fresh =
            edge["deleted"] == false && edge["version"] == 1 && edge["created_at"].is_string();
        let fields = [
            "id",
            "source",
            "target",
            "edge_type",
            "description",
            "source_revision",
            "target_revision",
        ];
        fresh
            && edge["workspace_id"] == json!(self.workspace)
            && fields_match(edge, expected, &fields)
    }

    fn created_task(&self, node: &Value, expected: &Value) -> bool {
        let identity =
            node["id"] == json!(self.task) && node["workspace_id"] == json!(self.workspace);
        let ownership = node["kind"] == "task" && node["created_by"] == *self.actor;
        let fields = [
            "title",
            "body",
            "acceptance_criteria",
            "labels",
            "task_state",
            "metadata",
        ];
        identity && ownership && fields_match(node, expected, &fields) && initial_node_state(node)
    }
}

fn fields_match(actual: &Value, expected: &Value, fields: &[&str]) -> bool {
    fields
        .iter()
        .all(|field| actual[*field] == expected[*field])
}

fn positive_revision(value: &Value) -> Option<String> {
    value
        .as_i64()
        .filter(|revision| *revision > 0)
        .map(|revision| revision.to_string())
}

fn initial_node_state(node: &Value) -> bool {
    let note = node["content_type"] == "note" && node["verification"] == "unverified";
    let live = node["deleted"] == false && node["retired"] == false;
    let timestamps = ["created_at", "updated_at"]
        .iter()
        .all(|field| node[*field].as_str().is_some_and(|value| value.len() <= 64));
    note && live && timestamps && empty_history(node)
}

fn empty_history(node: &Value) -> bool {
    let optional = [
        "summary",
        "summary_of_revision",
        "last_verified_by",
        "last_verified_at",
        "retirement_reason",
        "valid_from",
        "valid_until",
        "review_at",
        "successor_id",
    ];
    node["provenance"] == json!([]) && optional.iter().all(|field| node[*field].is_null())
}
