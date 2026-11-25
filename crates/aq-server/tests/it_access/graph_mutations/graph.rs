use super::*;
pub(super) struct FakeData {
    pub(super) nodes: BTreeMap<String, Value>,
    pub(super) edges: BTreeMap<String, Value>,
    pub(super) receipts: BTreeMap<String, (Value, Value)>,
    pub(super) posts: Vec<Value>,
    pub(super) applied: usize,
}
pub(super) struct FakeGraph {
    pub(super) upstream: Option<String>,
    pub(super) data: tokio::sync::Mutex<FakeData>,
    pub(super) lose_once: std::sync::atomic::AtomicBool,
    pub(super) race_after_get: std::sync::atomic::AtomicBool,
    pub(super) hold: std::sync::atomic::AtomicBool,
    pub(super) arrived: tokio::sync::Notify,
    pub(super) release: tokio::sync::Notify,
}
impl FakeGraph {
    pub(super) fn new() -> Self {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../../../protocol/agent-access/v1/fixtures/qualified-graph.json"
        ))
        .unwrap();
        Self {
            upstream: std::env::var("AQ_MUTATION_GRAPH_URL").ok(),
            data: tokio::sync::Mutex::new(FakeData {
                nodes: serde_json::from_value(fixture["nodes"].clone()).unwrap(),
                edges: serde_json::from_value(fixture["edges"].clone()).unwrap(),
                receipts: BTreeMap::new(),
                posts: Vec::new(),
                applied: 0,
            }),
            lose_once: false.into(),
            race_after_get: false.into(),
            hold: false.into(),
            arrived: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        }
    }
}
pub(super) enum ApiError {
    Missing,
    Forbidden,
    Invalid,
    Conflict,
    Internal,
}
impl ApiError {
    fn response(self) -> Response {
        let (status, code) = match self {
            Self::Missing => (StatusCode::NOT_FOUND, "not_found"),
            Self::Forbidden => (StatusCode::FORBIDDEN, "forbidden"),
            Self::Invalid => (StatusCode::BAD_REQUEST, "invalid_request"),
            Self::Conflict => (StatusCode::CONFLICT, "conflict"),
            Self::Internal => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        };
        graph_response(status, json!({"error":{"code":code}}))
    }
}
pub(super) fn graph_response(code: StatusCode, body: Value) -> Response {
    (code, [("X-AQ-Graph-API-Version", "1")], axum::Json(body)).into_response()
}
pub(super) async fn graph_http(
    State(fake): State<Arc<FakeGraph>>,
    request: AxumRequest,
) -> Response {
    if let Some(origin) = &fake.upstream {
        return qualified_proxy(&fake, origin, request).await;
    }
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let parts: Vec<_> = path.split('/').collect();
    let [_, "v1", "workspaces", workspace, collection, tail @ ..] = parts.as_slice() else {
        return ApiError::Missing.response();
    };
    let mut data = fake.data.lock().await;
    if method == axum::http::Method::GET {
        return fake.read(&mut data, collection, tail);
    }
    if method != axum::http::Method::POST {
        return ApiError::Invalid.response();
    }
    fake.write(
        data,
        Target {
            workspace,
            collection,
        },
        request,
    )
    .await
}
struct Target<'a> {
    workspace: &'a str,
    collection: &'a str,
}
impl FakeGraph {
    async fn write(
        &self,
        mut data: tokio::sync::MutexGuard<'_, FakeData>,
        target: Target<'_>,
        request: AxumRequest,
    ) -> Response {
        let raw = axum::body::to_bytes(request.into_body(), 1048576)
            .await
            .unwrap();
        let payload: Value = serde_json::from_slice(&raw).unwrap();
        data.posts.push(payload.clone());
        let key = payload["idempotency_key"].as_str().unwrap().to_string();
        if let Some(response) = data.replay(&key, &payload) {
            return response;
        }
        drop(data);
        if self.hold.load(std::sync::atomic::Ordering::SeqCst) {
            self.arrived.notify_one();
            self.release.notified().await;
        }
        let mut data = self.data.lock().await;
        let result = data.apply(target, key, payload);
        match result {
            Err(error) => error.response(),
            Ok(_)
                if self
                    .lose_once
                    .swap(false, std::sync::atomic::Ordering::SeqCst) =>
            {
                ApiError::Internal.response()
            }
            Ok(response) => graph_response(StatusCode::OK, response),
        }
    }
}
impl FakeData {
    fn replay(&self, key: &str, payload: &Value) -> Option<Response> {
        let (old, response) = self.receipts.get(key)?;
        match old == payload {
            true => Some(graph_response(StatusCode::OK, response.clone())),
            false => Some(ApiError::Conflict.response()),
        }
    }

    fn apply(
        &mut self,
        target: Target<'_>,
        key: String,
        payload: Value,
    ) -> Result<Value, ApiError> {
        let mut changes = Changes {
            nodes: self.nodes.clone(),
            edges: self.edges.clone(),
        };
        let results: Vec<Value> = payload["operations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|op| changes.apply(op))
            .collect::<Result<_, _>>()?;
        let response = if target.collection == "changesets" {
            json!({"id":Uuid::new_v4(),"workspace_id":target.workspace,"actor_id":"api01-writer","name":payload["name"],"reason":payload["reason"],"operations":payload["operations"],"status":"draft","created_at":"2026-10-30T00:00:00Z","applied_at":null,"result":null})
        } else {
            self.nodes = changes.nodes;
            self.edges = changes.edges;
            json!({"change_set_id":Uuid::new_v4(),"results":results})
        };
        self.applied += 1;
        self.receipts.insert(key, (payload, response.clone()));
        Ok(response)
    }
}
impl FakeGraph {
    fn read(&self, data: &mut FakeData, collection: &str, tail: &[&str]) -> Response {
        match tail {
            [id] if collection == "nodes" => self.node(data, id),
            [id, "context"] => Self::context(data, id),
            _ => ApiError::Invalid.response(),
        }
    }
    fn node(&self, data: &mut FakeData, id: &str) -> Response {
        let observed = data.nodes.get(id).cloned();
        if id == uid(2)
            && self
                .race_after_get
                .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            let node = data.nodes.get_mut(id).unwrap();
            node["version"] = json!(node["version"].as_i64().unwrap() + 1);
            node["verification"] = json!("verified");
        }
        match observed {
            Some(node) => graph_response(StatusCode::OK, node),
            None => ApiError::Missing.response(),
        }
    }
    fn context(data: &FakeData, id: &str) -> Response {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../../../protocol/agent-access/v1/fixtures/qualified-graph.json"
        ))
        .unwrap();
        let mut c = fixture["context"].clone();
        c["node"] = data.nodes.get(id).cloned().unwrap_or(Value::Null);
        c["edges"] = json!(data.edges.values().cloned().collect::<Vec<_>>());
        graph_response(StatusCode::OK, c)
    }
}
