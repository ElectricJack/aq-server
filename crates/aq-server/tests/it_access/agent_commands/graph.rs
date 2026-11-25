use super::*;
use axum::{
    Json,
    body::Bytes,
    extract::State,
    http::{Method, Uri},
};
#[derive(Clone)]
pub(super) struct DraftFixture {
    // (node id, state, metadata) plus the pre-existing edges
    pub(super) nodes: Vec<(Value, &'static str, Value)>,
    pub(super) edges: Vec<Value>,
    pub(super) pause: Arc<Mutex<Option<ScopePause>>>,
}

#[derive(Clone)]
pub(super) struct ScopePause {
    pub(super) task: Value,
    pub(super) started: Arc<tokio::sync::Notify>,
    pub(super) resume: Arc<tokio::sync::Notify>,
}

/// A Graph standing in for aq-graph's node read and mutation routes.
#[derive(Clone)]
pub(super) struct CreatingGraph {
    pub(super) workspace: Value,
    pub(super) project: Value,
    pub(super) bare: Arc<AtomicBool>,
    pub(super) posted: Arc<Mutex<Vec<Value>>>,
    pub(super) draft: Option<DraftFixture>,
}
impl CreatingGraph {
    pub(super) async fn serve(self) -> (String, tokio::task::JoinHandle<()>) {
        let app = axum::Router::new()
            .fallback(handler)
            .with_state(Arc::new(self));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        (
            url,
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }),
        )
    }

    fn node(&self, id: &Value, title: &str, metadata: Value) -> Value {
        let row = self
            .draft
            .as_ref()
            .and_then(|f| f.nodes.iter().find(|n| n.0 == *id));
        let Some((_, state, meta)) = row else {
            return self.task(id, title, metadata);
        };
        let mut value = self.task(id, title, meta.clone());
        value["task_state"] = json!(state);
        value
    }
    fn task(&self, id: &Value, title: &str, metadata: Value) -> Value {
        json!({"id":id,"workspace_id":self.workspace,"kind":"task","version":1,"content_revision":1,"title":title,
            "body":"","acceptance_criteria":"","labels":[],"metadata":metadata,"task_state":"active",
            "verification":"unverified","provenance":[],"retired":false,"deleted":false,
            "created_at":"2026-11-07T00:00:00Z","updated_at":"2026-11-07T00:00:00Z","created_by":"dev-service",
            "last_verified_by":null,"last_verified_at":null,"retirement_reason":null,"content_type":"note",
            "summary":null,"summary_of_revision":null,"valid_from":null,"valid_until":null,"review_at":null,
            "successor_id":null})
    }
    fn read(&self, path: &str) -> Value {
        let id = json!(path.rsplit('/').next().unwrap());
        let metadata = if self.bare.load(Ordering::SeqCst) {
            json!({})
        } else {
            json!({"aq:project_id":self.project})
        };
        self.node(&id, "Draft release notes for X", metadata)
    }
    async fn context(&self, path: &str) -> Value {
        let id = json!(path.rsplit('/').next().unwrap());
        let root = self.node(&id, "Draft fixture", json!({"aq:project_id":self.project}));
        let fixture = self.draft.as_ref().unwrap();
        let pause = fixture.pause.lock().unwrap().take_if(|p| p.task == id);
        if let Some(pause) = pause {
            pause.started.notify_one();
            pause.resume.notified().await;
        }
        let nodes: Vec<_> = fixture
            .nodes
            .iter()
            .filter(|n| n.0 != id)
            .map(|n| self.node(&n.0, "Draft fixture", json!({})))
            .collect();
        json!({"node":root,"nodes":nodes,"edges":fixture.edges,"pinned_revisions":[],"truncated":false})
    }
    fn mutate(&self, body: &Bytes) -> Value {
        let request: Value = serde_json::from_slice(body).unwrap();
        self.posted.lock().unwrap().push(request.clone());
        let results: Vec<_> = request["operations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|op| self.apply(op))
            .collect();
        json!({"change_set_id":Uuid::new_v4(),"results":results})
    }
    fn apply(&self, op: &Value) -> Value {
        match op["op"].as_str() {
            Some("create_node") => {
                let mut result = self.node(
                    &op["id"],
                    op["title"].as_str().unwrap(),
                    op["metadata"].clone(),
                );
                result["task_state"] = op["task_state"].clone();
                result
            }
            Some("update_node") => self.node(
                &op["id"],
                op["patch"]["title"].as_str().unwrap_or("Draft fixture"),
                json!({"aq:project_id":self.project}),
            ),
            Some("remove_edge") => {
                let mut edge = self
                    .draft
                    .as_ref()
                    .unwrap()
                    .edges
                    .iter()
                    .find(|e| e["id"] == op["id"])
                    .unwrap()
                    .clone();
                edge["deleted"] = json!(true);
                edge["version"] = json!(2);
                edge
            }
            _ => {
                json!({"id":op["id"],"workspace_id":self.node(&op["id"],"x",json!({}))["workspace_id"],
                "source":op["source"],"target":op["target"],"edge_type":op["edge_type"],
                "source_revision":null,"target_revision":null,"version":1,"deleted":false,
                "created_at":"2026-11-07T00:00:00Z","description":null})
            }
        }
    }
}
async fn handler(
    State(graph): State<Arc<CreatingGraph>>,
    method: Method,
    uri: Uri,
    body: Bytes,
) -> impl axum::response::IntoResponse {
    let path = uri.path();
    let reply = if method == Method::POST && path.ends_with("/mutations") {
        graph.mutate(&body)
    } else if let Some(path) = path.strip_suffix("/context") {
        graph.context(path).await
    } else {
        graph.read(path)
    };
    ([("X-AQ-Graph-API-Version", "1")], Json(reply))
}
