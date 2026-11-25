//! Bounded observations of Graph task state for the owner overview.
use crate::projection::Project;
use serde::Serialize;
use serde_json::{Value, json};
use uuid::Uuid;
#[derive(Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Coverage {
    Complete,
    Partial,
    Unavailable,
}
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum SourceStatus {
    Available,
    Unavailable,
}
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum Accuracy {
    CompleteObservation,
    LowerBound,
    Unknown,
}
#[derive(Default, Serialize)]
struct Counts {
    draft: Option<u16>,
    active: Option<u16>,
    completed: Option<u16>,
    canceled: Option<u16>,
}
#[derive(Serialize)]
struct Task {
    id: Uuid,
    title: String,
    version: String,
    content_revision: String,
    task_state: String,
    updated_at: String,
}
#[derive(Serialize)]
pub struct ProjectView {
    project: Project,
    observed_at: Option<String>,
    source_status: SourceStatus,
    coverage: Coverage,
    count_accuracy: Accuracy,
    task_counts: Counts,
    tasks: Vec<Task>,
    more_tasks: Option<bool>,
    reason: Option<&'static str>,
}
impl Counts {
    fn empty() -> Self {
        Self {
            draft: Some(0),
            active: Some(0),
            completed: Some(0),
            canceled: Some(0),
        }
    }
    fn record(&mut self, state: &str) -> Result<(), ()> {
        let count = match state {
            "draft" => &mut self.draft,
            "active" => &mut self.active,
            "completed" => &mut self.completed,
            "canceled" => &mut self.canceled,
            _ => return Err(()),
        };
        *count = Some(count.ok_or(())?.checked_add(1).ok_or(())?);
        Ok(())
    }
}
impl Task {
    fn read(node: &Value, workspace: Uuid) -> Result<Self, ()> {
        let scoped = node["workspace_id"] == json!(workspace) && node["kind"] == "task";
        let live = node["deleted"] == false && node["retired"] == false;
        if !scoped || !live {
            return Err(());
        }
        let id = node["id"]
            .as_str()
            .and_then(|s| s.parse::<Uuid>().ok())
            .filter(|id| !id.is_nil())
            .ok_or(())?;
        let revision = |key: &str| {
            node[key]
                .as_u64()
                .filter(|n| *n > 0 && *n <= i64::MAX as u64)
                .map(|n| n.to_string())
                .ok_or(())
        };
        let task_state = node["task_state"].as_str().ok_or(())?;
        Ok(Self {
            id,
            title: node["title"].as_str().ok_or(())?.into(),
            version: revision("version")?,
            content_revision: revision("content_revision")?,
            task_state: task_state.into(),
            updated_at: node["updated_at"].as_str().ok_or(())?.into(),
        })
    }
}
impl ProjectView {
    pub fn complete(&self) -> bool {
        self.coverage == Coverage::Complete
    }
    pub fn failed(project: Project, reason: &'static str) -> ProjectView {
        ProjectView {
            project,
            observed_at: None,
            source_status: SourceStatus::Unavailable,
            coverage: Coverage::Unavailable,
            count_accuracy: Accuracy::Unknown,
            task_counts: Counts::default(),
            tasks: vec![],
            more_tasks: None,
            reason: Some(reason),
        }
    }
    /// Project a schema-validated Graph page, including whether its counts are complete.
    pub fn from_page(project: Project, raw: &Value, limit: u16, at: String) -> Option<Self> {
        let rows = raw["items"].as_array()?;
        if rows.len() > usize::from(limit) {
            return None;
        }
        let more = more_tasks(&raw["next_cursor"])?;
        let mut counts = Counts::empty();
        let mut tasks = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for node in rows {
            let task = Task::read(node, project.workspace_id).ok()?;
            if !seen.insert(task.id) {
                return None;
            }
            counts.record(&task.task_state).ok()?;
            tasks.push(task);
        }
        Some(ProjectView {
            project,
            observed_at: Some(at),
            source_status: SourceStatus::Available,
            coverage: if more {
                Coverage::Partial
            } else {
                Coverage::Complete
            },
            count_accuracy: if more {
                Accuracy::LowerBound
            } else {
                Accuracy::CompleteObservation
            },
            task_counts: counts,
            tasks,
            more_tasks: Some(more),
            reason: None,
        })
    }
}
fn more_tasks(cursor: &Value) -> Option<bool> {
    let Some(cursor) = cursor.as_str() else {
        return Some(false);
    };
    let bounded = (1..=256).contains(&cursor.len()) && !cursor.chars().any(char::is_control);
    bounded.then_some(true)
}
