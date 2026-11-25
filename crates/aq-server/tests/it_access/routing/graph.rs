use super::*;

#[derive(Default)]
pub(super) struct Fake {
    pub(super) nodes: BTreeMap<String, Value>,
    pub(super) posts: usize,
    /// Bump the node's version right after the next GET (a concurrent edit).
    pub(super) race_after_get: bool,
}
pub(super) type Shared = Arc<Mutex<Fake>>;

fn reply(code: StatusCode, body: Value) -> Response {
    (code, [("X-AQ-Graph-API-Version", "1")], axum::Json(body)).into_response()
}

/// GET a node; POST `update_node` with the version check and a whole-field patch.
pub(super) async fn graph(State(fake): State<Shared>, request: Request) -> Response {
    let parts: Vec<String> = request.uri().path().split('/').map(String::from).collect();
    let mut fake = fake.lock().await;
    let node_path = matches!(parts.as_slice(), [_, _, _, _, kind, _] if kind == "nodes");
    if request.method() == Method::GET && node_path {
        return fake.read(&parts[5]);
    }
    let bytes = axum::body::to_bytes(request.into_body(), 1 << 20)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    fake.posts += 1;
    let op = &body["operations"][0];
    assert_eq!(op["op"], "update_node");
    let node = fake.nodes.get_mut(op["id"].as_str().unwrap()).unwrap();
    if node["retired"] == true {
        return reply(StatusCode::FORBIDDEN, json!({"error":{"code":"forbidden"}}));
    }
    if node["version"] != op["expected_version"] {
        return reply(StatusCode::CONFLICT, json!({"error":{"code":"conflict"}}));
    }
    for (k, v) in op["patch"].as_object().unwrap() {
        node[k] = v.clone();
    }
    node["version"] = json!(node["version"].as_u64().unwrap() + 1);
    node["content_revision"] = json!(node["content_revision"].as_u64().unwrap() + 1);
    let result = node.clone();
    reply(
        StatusCode::OK,
        json!({"change_set_id":Uuid::new_v4(),"results":[result]}),
    )
}

impl Fake {
    fn read(&mut self, id: &str) -> Response {
        let Some(observed) = self.nodes.get(id).cloned() else {
            return reply(StatusCode::NOT_FOUND, json!({"error":{"code":"not_found"}}));
        };
        if std::mem::take(&mut self.race_after_get) {
            let n = self.nodes.get_mut(id).unwrap();
            n["version"] = json!(n["version"].as_u64().unwrap() + 1);
        }
        reply(StatusCode::OK, observed)
    }
}
