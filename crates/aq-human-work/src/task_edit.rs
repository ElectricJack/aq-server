//! Version-bound task field changes. The owner of the Graph mutation supplies the current node.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;
use uuid::Uuid;

fn text(v: &str, min: usize, max: usize) -> bool {
    v.len() >= min && v.len() <= max && !v.contains('\0')
}
fn labels(v: &Option<Vec<String>>) -> bool {
    v.as_ref().is_none_or(|l| {
        l.len() <= 16
            && l.iter().all(|s| text(s, 1, 128) && !s.trim().is_empty())
            && l.iter().collect::<BTreeSet<_>>().len() == l.len()
    })
}
fn put(patch: &mut serde_json::Map<String, Value>, key: &str, value: Option<impl Serialize>) {
    if let Some(v) = value {
        patch.insert(key.into(), json!(v));
    }
}

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskEdit {
    pub operation_id: Uuid,
    pub project_id: Uuid,
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    pub expected_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 512))]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 32768))]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 8192))]
    pub acceptance_criteria: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 16))]
    pub labels: Option<Vec<String>>,
    /// Absent leaves the estimate intact. Null removes it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_points"
    )]
    #[schemars(range(min = 1, max = 1000))]
    pub story_points: Option<Option<u16>>,
}

fn present_points<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<Option<u16>>, D::Error> {
    Option::deserialize(deserializer).map(Some)
}

impl TaskEdit {
    pub fn valid(&self) -> bool {
        (self.title.is_some()
            || self.body.is_some()
            || self.acceptance_criteria.is_some()
            || self.labels.is_some()
            || self.story_points.is_some())
            && self
                .story_points
                .flatten()
                .is_none_or(|n| (1..=super::MAX_STORY_POINTS).contains(&n))
            && self
                .title
                .as_deref()
                .is_none_or(|t| text(t, 1, 512) && !t.trim().is_empty())
            && self.body.as_deref().is_none_or(|t| text(t, 0, 32768))
            && self
                .acceptance_criteria
                .as_deref()
                .is_none_or(|t| text(t, 0, 8192))
            && labels(&self.labels)
    }
    pub fn patch(&self, node: &Value) -> Option<Map<String, Value>> {
        let mut patch = serde_json::Map::new();
        put(&mut patch, "title", self.title.as_ref());
        put(&mut patch, "body", self.body.as_ref());
        put(
            &mut patch,
            "acceptance_criteria",
            self.acceptance_criteria.as_ref(),
        );
        put(&mut patch, "labels", self.labels.as_ref());
        if let Some(points) = self.story_points {
            // Graph replaces the metadata object on update.
            let mut metadata = node["metadata"].as_object().cloned()?;
            if let Some(points) = points {
                metadata.insert(super::STORY_POINTS_KEY.into(), json!(points));
            } else {
                metadata.remove(super::STORY_POINTS_KEY);
            }
            patch.insert("metadata".into(), Value::Object(metadata));
        }
        Some(patch)
    }
}

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskReprioritize {
    pub operation_id: Uuid,
    pub project_id: Uuid,
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    pub expected_version: String,
    /// Stored as Graph metadata `aq:priority`; the scheduler sorts descending.
    #[schemars(range(min = -1_000_000, max = 1_000_000))]
    pub priority: i64,
}
impl TaskReprioritize {
    pub fn valid(&self) -> bool {
        (-1_000_000..=1_000_000).contains(&self.priority)
    }
    pub fn patch(&self, node: &Value) -> Option<Map<String, Value>> {
        // A Graph metadata patch replaces the object; keep every other key.
        let mut metadata = node["metadata"].as_object()?.clone();
        metadata.insert("aq:priority".into(), json!(self.priority));
        let mut patch = serde_json::Map::new();
        patch.insert("metadata".into(), Value::Object(metadata));
        Some(patch)
    }
}

#[cfg(test)]
mod story_points_tests {
    use super::*;

    #[test]
    fn story_points_matches_the_dashboard_action_contract() {
        let generated = serde_json::to_value(schemars::schema_for!(TaskEdit)).unwrap();
        let published: Value = serde_json::from_str(include_str!(
            "../../../protocol/dashboard-actions/v1/schema.json"
        ))
        .unwrap();
        let payload = published["$defs"]["ActionRequest"]["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["properties"]["kind"]["const"] == "task.edit")
            .unwrap()["properties"]["payload"]
            .clone();
        assert_eq!(payload["properties"], generated["properties"]);
        assert_eq!(payload["required"], generated["required"]);
    }

    #[test]
    fn story_points_edit_preserves_metadata_and_distinguishes_missing_from_null() {
        let id = Uuid::new_v4();
        let input =
            json!({"operation_id":id,"project_id":id,"expected_version":"5","title":"Task"});
        let node = json!({"metadata":{"aq:priority":7,"aq:story_points":3}});
        for (points, expected) in [
            (Some(json!(8)), json!({"aq:priority":7,"aq:story_points":8})),
            (Some(Value::Null), json!({"aq:priority":7})),
            (None, Value::Null),
        ] {
            let mut request = input.clone();
            if let Some(points) = points {
                request["story_points"] = points;
            }
            let edit: TaskEdit = serde_json::from_value(request).unwrap();
            assert!(edit.valid());
            assert_eq!(
                edit.patch(&node)
                    .unwrap()
                    .get("metadata")
                    .cloned()
                    .unwrap_or_default(),
                expected
            );
        }
        let mut invalid = input;
        invalid["story_points"] = json!(1001);
        assert!(!serde_json::from_value::<TaskEdit>(invalid).unwrap().valid());
    }
}
