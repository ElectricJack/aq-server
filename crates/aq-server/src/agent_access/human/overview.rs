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
use aq_human_work::overview::{Coverage, ProjectView};
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
    ProjectView::from_page(project, raw, limit, at).ok_or(())
}
impl QueryArgs {
    fn limits(&self) -> Result<(u16, u16)> {
        let project = self.project_limit.unwrap_or(4);
        let tasks = self.task_limit.unwrap_or(16);
        let bounded = (1..=8).contains(&project) && (1..=32).contains(&tasks);
        let valid_ids = [self.project_id, self.after]
            .into_iter()
            .flatten()
            .all(|id| !id.is_nil());
        let valid_scope = valid_ids && (self.project_id.is_none() || self.after.is_none());
        if !bounded || !valid_scope {
            return Err(invalid());
        }
        Ok((project, tasks))
    }
    async fn page(
        &self,
        runtime: &ReadRuntime,
        project_limit: u16,
    ) -> Result<(Vec<ProjectRecord>, bool, Option<Uuid>)> {
        let rows:Vec<Value> = sqlx::query_scalar("SELECT jsonb_build_object('revision',revision,'binding',binding,'validated_at_ms',validated_at_ms,'validation',binding_observation) FROM project_execution_bindings WHERE ($1::uuid IS NULL OR project_id=$1) AND ($2::uuid IS NULL OR project_id>$2) ORDER BY project_id LIMIT $3").bind(self.project_id).bind(self.after).bind(i64::from(project_limit)+1).fetch_all(&runtime.pool).await.map_err(|_|unavailable())?;
        if self.project_id.is_some() && rows.is_empty() {
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
        Ok((records, more_projects, next_after))
    }
}
struct TaskReader<'a> {
    state: &'a AppState,
    graph: &'a Runtime,
    limit: u16,
    deadline: tokio::time::Instant,
}
impl TaskReader<'_> {
    async fn observe(&self, record: &ProjectRecord) -> Result<ProjectView> {
        if record.binding.graph_adapter_reference != "aq.graph.configured"
            || record.binding.graph_workspace_id.is_nil()
        {
            return Ok(ProjectView::failed(project(record), "unsupported_adapter"));
        }
        let remaining = self
            .deadline
            .saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Ok(ProjectView::failed(project(record), "budget_exhausted"));
        }
        let query = vec![
            ("kind".into(), "task".into()),
            ("include_retired".into(), "false".into()),
            ("limit".into(), self.limit.to_string()),
        ];
        let path = format!("/v1/workspaces/{}/nodes", record.binding.graph_workspace_id);
        let read = self.graph.human_get(&path, &query, 1_000_000);
        let view = match tokio::time::timeout(remaining.min(PROJECT_CALL), read).await {
            Ok(Ok(raw)) => observed(project(record), &raw, self.limit, timestamp(self.state)?)
                .unwrap_or_else(|_| ProjectView::failed(project(record), "invalid_graph_response")),
            Ok(Err(_)) => ProjectView::failed(project(record), "graph_unavailable"),
            Err(_) => ProjectView::failed(project(record), "read_timeout"),
        };
        Ok(view)
    }
}
pub(super) async fn read(
    State(state): State<AppState>,
    Extension(runtime): Extension<Arc<ReadRuntime>>,
    RequireOwner(actor): RequireOwner,
    args: std::result::Result<Query<QueryArgs>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Reply>> {
    let Query(args) = args.map_err(|_| invalid())?;
    let (project_limit, task_limit) = args.limits()?;
    let graph = super::runtime(&state)?;
    owner(&runtime, &actor).await?;
    let requested = stamp(&state);
    let (records, more_projects, next_after) = args.page(&runtime, project_limit).await?;
    let reader = TaskReader {
        state: &state,
        graph,
        limit: task_limit,
        deadline: tokio::time::Instant::now() + GRAPH_BUDGET,
    };
    let mut projects = Vec::new();
    for record in &records {
        projects.push(reader.observe(record).await?);
    }
    fence_records(&runtime, &actor, &records).await?;
    let at = stamp(&state);
    if at < requested || at - requested >= 5000 {
        return Err(unavailable());
    }
    let coverage = if more_projects || projects.iter().any(|p| !p.complete()) {
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
        // `role` is AQ's projection (spec 0014 §5.5), not a Graph field.
        node.as_object_mut().unwrap().remove("role");
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
