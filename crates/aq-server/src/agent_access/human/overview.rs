//! Bounded observed task lifecycle facts, never execution or scheduling state.
use super::*;
const GRAPH_BUDGET: Duration = Duration::from_secs(3);
const PROJECT_CALL: Duration = Duration::from_millis(750);
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct QueryArgs {
    project_id: Option<Uuid>,
    project_limit: Option<u16>,
    task_limit: Option<u16>,
    after: Option<Uuid>,
}
#[derive(Serialize)]
struct Scope {
    project_id: Option<Uuid>,
}
#[derive(Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Coverage {
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
struct ProjectView {
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
#[derive(Serialize)]
pub(super) struct Reply {
    contract: &'static str,
    observed_at: String,
    scope: Scope,
    projects: Vec<ProjectView>,
    next_after: Option<Uuid>,
    coverage: Coverage,
}
fn project(record: &ProjectRecord) -> Project {
    Project {
        id: record.binding.project_id,
        name: record.binding.name.clone(),
        revision: record.revision.to_string(),
        workspace_id: record.binding.graph_workspace_id,
    }
}
fn failed(record: &ProjectRecord, reason: &'static str) -> ProjectView {
    ProjectView {
        project: project(record),
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
fn timestamp(state: &AppState) -> Result<String> {
    chrono::DateTime::from_timestamp_millis(stamp(state))
        .map(|d| d.to_rfc3339())
        .ok_or_else(unavailable)
}
fn observed(
    project: Project,
    raw: &Value,
    limit: u16,
    at: String,
) -> std::result::Result<ProjectView, ()> {
    if !contract::upstream_valid("NodePage", raw) {
        return Err(());
    }
    let rows = raw["items"].as_array().ok_or(())?;
    if rows.len() > usize::from(limit) {
        return Err(());
    }
    let more = raw["next_cursor"].is_string();
    if more
        && raw["next_cursor"]
            .as_str()
            .is_none_or(|s| s.is_empty() || s.len() > 256 || s.chars().any(char::is_control))
    {
        return Err(());
    }
    let mut counts = Counts {
        draft: Some(0),
        active: Some(0),
        completed: Some(0),
        canceled: Some(0),
    };
    let mut tasks = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for node in rows {
        if node["workspace_id"] != json!(project.workspace_id)
            || node["kind"] != "task"
            || node["deleted"] != false
            || node["retired"] != false
        {
            return Err(());
        }
        let id = node["id"]
            .as_str()
            .and_then(|s| s.parse::<Uuid>().ok())
            .filter(|id| !id.is_nil())
            .ok_or(())?;
        if !seen.insert(id) {
            return Err(());
        }
        let revision = |key: &str| {
            node[key]
                .as_u64()
                .filter(|n| *n > 0 && *n <= i64::MAX as u64)
                .map(|n| n.to_string())
                .ok_or(())
        };
        let task_state = node["task_state"].as_str().ok_or(())?;
        let count = match task_state {
            "draft" => &mut counts.draft,
            "active" => &mut counts.active,
            "completed" => &mut counts.completed,
            "canceled" => &mut counts.canceled,
            _ => return Err(()),
        };
        *count = Some(count.ok_or(())?.checked_add(1).ok_or(())?);
        tasks.push(Task {
            id,
            title: node["title"].as_str().ok_or(())?.into(),
            version: revision("version")?,
            content_revision: revision("content_revision")?,
            task_state: task_state.into(),
            updated_at: node["updated_at"].as_str().ok_or(())?.into(),
        });
    }
    Ok(ProjectView {
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
pub(super) async fn read(
    State(state): State<AppState>,
    Extension(runtime): Extension<Arc<ReadRuntime>>,
    RequireOwner(actor): RequireOwner,
    args: std::result::Result<Query<QueryArgs>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Reply>> {
    let Query(args) = args.map_err(|_| invalid())?;
    let project_limit = args.project_limit.unwrap_or(4);
    let task_limit = args.task_limit.unwrap_or(16);
    if !(1..=8).contains(&project_limit)
        || !(1..=32).contains(&task_limit)
        || args.project_id.is_some_and(|id| id.is_nil())
        || args.after.is_some_and(|id| id.is_nil())
        || (args.project_id.is_some() && args.after.is_some())
    {
        return Err(invalid());
    }
    let graph = super::runtime(&state)?;
    owner(&runtime, &actor).await?;
    let requested = stamp(&state);
    let rows:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('revision',revision,'binding',binding,'validated_at_ms',validated_at_ms,'validation',binding_observation) FROM project_execution_bindings WHERE ($1::uuid IS NULL OR project_id=$1) AND ($2::uuid IS NULL OR project_id>$2) ORDER BY project_id LIMIT $3").bind(args.project_id).bind(args.after).bind(i64::from(project_limit)+1).fetch_all(&runtime.pool).await.map_err(|_|unavailable())?;
    if args.project_id.is_some() && rows.is_empty() {
        return Err(AppError::NotFound("project not found".into()));
    }
    let more_projects = rows.len() > usize::from(project_limit);
    let records: Vec<ProjectRecord> = rows
        .into_iter()
        .take(usize::from(project_limit))
        .map(|v| serde_json::from_value(v).map_err(|_| unavailable()))
        .collect::<Result<_>>()?;
    let next_after = if more_projects {
        records.last().map(|p| p.binding.project_id)
    } else {
        None
    };
    let deadline = tokio::time::Instant::now() + GRAPH_BUDGET;
    let mut projects = Vec::new();
    for record in &records {
        if record.binding.graph_adapter_reference != "aq.graph.configured"
            || record.binding.graph_workspace_id.is_nil()
        {
            projects.push(failed(record, "unsupported_adapter"));
            continue;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            projects.push(failed(record, "budget_exhausted"));
            continue;
        }
        let query = vec![
            ("kind".into(), "task".into()),
            ("include_retired".into(), "false".into()),
            ("limit".into(), task_limit.to_string()),
        ];
        let path = format!("/v1/workspaces/{}/nodes", record.binding.graph_workspace_id);
        let read = graph.human_get(&path, &query, 1_000_000);
        let view = match tokio::time::timeout(remaining.min(PROJECT_CALL), read).await {
            Ok(Ok(raw)) => observed(project(record), &raw, task_limit, timestamp(&state)?)
                .unwrap_or_else(|_| failed(record, "invalid_graph_response")),
            Ok(Err(_)) => failed(record, "graph_unavailable"),
            Err(_) => failed(record, "read_timeout"),
        };
        projects.push(view);
    }
    fence_records(&runtime, &actor, &records).await?;
    let at = stamp(&state);
    if at < requested || at - requested >= 5000 {
        return Err(unavailable());
    }
    let coverage = if more_projects || projects.iter().any(|p| p.coverage != Coverage::Complete) {
        Coverage::Partial
    } else {
        Coverage::Complete
    };
    let reply = Reply {
        contract: "human-overview-tasks-1",
        observed_at: timestamp(&state)?,
        scope: Scope {
            project_id: args.project_id,
        },
        projects,
        next_after,
        coverage,
    };
    if serde_json::to_vec(&reply).map_err(|_| unavailable())?.len() > 1_048_576 {
        return Err(unavailable());
    }
    Ok(Json(reply))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn project() -> Project {
        Project {
            id: Uuid::from_u128(1),
            name: "fixture".into(),
            revision: "1".into(),
            workspace_id: Uuid::from_u128(2),
        }
    }
    fn page() -> Value {
        let mut node: Value = serde_json::from_str::<Value>(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../protocol/human-graph/v1/examples/task.json"
        )))
        .unwrap()["data"]
            .clone();
        node["workspace_id"] = json!(Uuid::from_u128(2));
        node["metadata"] = json!({"aq:private-canary":"not-a-summary-field"});
        node["version"] = json!(1);
        node["content_revision"] = json!(1);
        json!({"items":[node],"next_cursor":null})
    }
    #[test]
    fn complete_and_lower_bound_are_observations_of_task_lifecycle_only() {
        let raw = page();
        let complete = observed(project(), &raw, 16, "2026-11-01T00:00:00Z".into()).unwrap();
        let value = serde_json::to_value(complete).unwrap();
        assert_eq!(value["count_accuracy"], "complete_observation");
        assert_eq!(value["task_counts"]["active"], 1);
        assert_eq!(value["task_counts"]["completed"], 0);
        assert_eq!(value["more_tasks"], false);
        assert!(value.get("running").is_none());
        assert!(!value.to_string().contains("private-canary"));
        assert!(value["tasks"][0].get("body").is_none());
        let mut partial = raw;
        partial["next_cursor"] = json!("another-page");
        let value = serde_json::to_value(
            observed(project(), &partial, 16, "2026-11-01T00:00:00Z".into()).unwrap(),
        )
        .unwrap();
        assert_eq!(value["count_accuracy"], "lower_bound");
        assert_eq!(value["coverage"], "partial");
        assert_eq!(value["task_counts"]["active"], 1);
    }
    #[test]
    fn future_states_wrong_scope_duplicate_and_over_budget_records_refuse() {
        let raw = page();
        let mut unknown = raw.clone();
        unknown["items"][0]["task_state"] = json!("future");
        assert!(observed(project(), &unknown, 16, "time".into()).is_err());
        let mut foreign = raw.clone();
        foreign["items"][0]["workspace_id"] = json!(Uuid::new_v4());
        assert!(observed(project(), &foreign, 16, "time".into()).is_err());
        let mut duplicate = raw;
        let item = duplicate["items"][0].clone();
        duplicate["items"].as_array_mut().unwrap().push(item);
        assert!(observed(project(), &duplicate, 16, "time".into()).is_err());
        assert!(observed(project(), &duplicate, 1, "time".into()).is_err());
    }
}
