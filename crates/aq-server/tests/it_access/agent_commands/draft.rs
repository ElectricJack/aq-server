use super::*;
use crate::messaging::Env;
use aq_agent_grants::resolve::ProjectGrants;
mod nodes;
use nodes::Nodes;
mod creation;
mod reservation;
mod review;
pub(super) struct Case {
    e: Env,
    nodes: Nodes,
    attempt: Uuid,
    policy: ProjectGrants,
    posted: Arc<Mutex<Vec<Value>>>,
    pause: Arc<Mutex<Option<ScopePause>>>,
    graph: tokio::task::JoinHandle<()>,
}
impl Case {
    pub(super) async fn new() -> Self {
        let nodes = Nodes::new();
        let pause = Arc::new(Mutex::new(None));
        let posted = Arc::new(Mutex::new(Vec::new()));
        let (url, graph) = CreatingGraph {
            workspace: nodes.workspace.clone(),
            project: nodes.project.clone(),
            bare: Arc::new(AtomicBool::new(false)),
            posted: posted.clone(),
            draft: Some(nodes.fixture(pause.clone())),
        }
        .serve()
        .await;
        let policy = serde_json::from_value(json!({"by_work_kind":{
            "investigation":{"max_created_tasks":0},
            "spec_ingestion":{"max_created_tasks":30},
            "fidelity_review":{"max_created_tasks":10,"edit_drafts":true}
        }}))
        .unwrap();
        let attempt = Uuid::new_v4();
        let e = setup_graph_with_attempt(
            Some(grants(&policy, "spec_ingestion")),
            crate::messaging::GraphEndpoint {
                url: &url,
                mutation_actor_id: Some("dev-service"),
            },
            Some(attempt),
            |_, _| {},
        )
        .await;
        install_task_project(&e, nodes.workspace.clone(), nodes.project.clone()).await;

        let case = Self {
            e,
            nodes,
            attempt,
            policy,
            posted,
            pause,
            graph,
        };
        case.install_author().await;
        case
    }
    pub(super) fn finish(self) {
        self.graph.abort();
    }
    async fn command(&self, name: &str, body: Value) -> (StatusCode, Value) {
        send(
            &self.e.app,
            &format!("/api/v1/commands/{name}"),
            Some(&self.e.token),
            Some(body),
        )
        .await
    }
    async fn set_grants(&self, kind: &str) {
        sqlx::query("UPDATE agent_launch_grants SET grants=$2 WHERE attempt_id=$1")
            .bind(self.attempt)
            .bind(grants(&self.policy, kind))
            .execute(&self.e.pool)
            .await
            .unwrap();
    }
}
fn grants(policy: &ProjectGrants, kind: &str) -> Value {
    serde_json::to_value(aq_agent_grants::resolve::resolve_work(
        &[],
        policy,
        None,
        Some(kind),
    ))
    .unwrap()
}
fn id(value: &Value) -> Uuid {
    value.as_str().unwrap().parse().unwrap()
}
fn change(task: Uuid) -> Value {
    json!({"operation_id":Uuid::new_v4(),
        "params":{"task_id":task,"expected_version":"1","title":"Reviewer edit"}})
}
