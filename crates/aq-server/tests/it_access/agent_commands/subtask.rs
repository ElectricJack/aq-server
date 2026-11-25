use super::*;
use crate::messaging::Env;
pub(super) struct Case {
    e: Env,
    graph: tokio::task::JoinHandle<()>,
    bare: Arc<AtomicBool>,
    posted: Arc<Mutex<Vec<Value>>>,
    attempt: Uuid,
    own: Value,
    project: Value,
}
impl Case {
    pub(super) async fn new() -> Self {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../../../protocol/agent-access/v1/fixtures/base.json"
        ))
        .unwrap();
        let base = &fixture["context"]["binding"];
        let (workspace, project) = (base["workspace_id"].clone(), base["project_id"].clone());
        let bare = Arc::new(AtomicBool::new(false));
        let posted = Arc::new(Mutex::new(Vec::new()));
        let (url, graph) = CreatingGraph {
            workspace: workspace.clone(),
            project: project.clone(),
            bare: bare.clone(),
            posted: posted.clone(),
            draft: None,
        }
        .serve()
        .await;
        let e = setup_graph_with(
        Some(json!([
            {"operation":"task.create.subtask","scope":"task:subtree","limits":{"max_created_tasks":5,"max_depth":2}},
            {"operation":"task.read","scope":"project:self"}
        ])),
        &url,
        Some("dev-service"),
        |_, _| {},
    )
    .await;
        let attempt = Uuid::parse_str(e.binding["attempt_id"].as_str().unwrap()).unwrap();
        let own = e.binding["task_id"].clone();
        install_task_project(&e, workspace, project.clone()).await;
        Self {
            e,
            graph,
            bare,
            posted,
            attempt,
            own,
            project,
        }
    }
    pub(super) async fn create(&self) -> Uuid {
        let op = Uuid::new_v4();
        let (status, body) = invoke(
            &self.e,
            "task.create",
            json!({"operation_id":op,"title":"Verify changelog links",
        "body":"Created by the agent.","parent_id":"self","role":"reviewer"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["data"]["disposition"], "confirmed", "{body}");
        // Journaled under the attempt's own principal domain: neither an owner
        // nor a supervisor session.
        let row: (Uuid, Option<Uuid>, Option<Uuid>, String) = sqlx::query_as(
        "SELECT principal_id,owner_id,supervisor_session_id,kind FROM dashboard_operations WHERE operation_id=$1",
    )
    .bind(op)
    .fetch_one(&self.e.pool)
    .await
    .unwrap();
        assert_eq!(row, (self.attempt, None, None, "task.create".into()));
        op
    }
    pub(super) fn verify_graph(&self, op: Uuid) {
        // The Graph write: an active task with the agent's attribution, under
        // the attempt's own task.
        let posted = self.posted.lock().unwrap().clone();
        assert_eq!(posted.len(), 1);
        let ops = posted[0]["operations"].as_array().unwrap();
        let created = ops.iter().find(|o| o["op"] == "create_node").unwrap();
        assert_eq!(created["id"], json!(op));
        assert_eq!(created["task_state"], "active");
        for (key, expected) in [
            ("aq:agent_attempt_id", json!(self.attempt)),
            ("aq:project_id", self.project.clone()),
            ("aq:role", json!("reviewer")),
        ] {
            assert_eq!(created["metadata"][key], expected, "{key}");
        }
        assert!(
            ops.iter().any(|o| o["op"] == "add_edge"
                && o["edge_type"] == "parent-child"
                && o["source"] == self.own
                && o["target"] == json!(op)),
            "{posted:?}"
        );
    }
    pub(super) async fn verify_depth(&self, op: Uuid) {
        let depth: Option<i32> =
            sqlx::query_scalar("SELECT depth FROM agent_created_tasks WHERE task_id=$1")
                .bind(op)
                .fetch_one(&self.e.pool)
                .await
                .unwrap();
        assert_eq!(depth, Some(1));
    }
    pub(super) async fn verify_unscoped_parent(&self) {
        // A parent the Graph does not record as a task of this project is a
        // §6.8 refusal with the reason, not a bare 403, and nothing is written.
        self.bare.store(true, Ordering::SeqCst);
        let (status, body) = invoke(
            &self.e,
            "task.create",
            json!({"operation_id":Uuid::new_v4(),"title":"x","parent_id":"self"}),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        for (key, expected) in [
            ("code", "scope_denied"),
            ("grant", "task.create.subtask"),
            ("effect", "not_applied"),
        ] {
            assert_eq!(body[key], expected, "{body}");
        }
        assert!(
            body["message"].as_str().unwrap().contains("aq:project_id"),
            "{body}"
        );
    }
    pub(super) fn finish(self) {
        self.graph.abort();
    }
}
