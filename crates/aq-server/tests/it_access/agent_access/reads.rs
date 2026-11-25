use super::*;
impl Case {
    pub(super) async fn visible_graph(&self, new_agent: &str) {
        let queries = vec![
            json!({"kind":"get","node_id":self.launch.binding["task_id"]}),
            json!({"kind":"revision","node_id":self.launch.binding["task_id"],"revision":"1"}),
            json!({"kind":"context","node_id":self.launch.binding["task_id"],"depth":1,"direction":"both","max_nodes":64,"max_edges":256,"max_bytes":1048576}),
            json!({"kind":"relationships","node_id":self.launch.binding["task_id"],"direction":"both","edge_types":["references","parent-child"],"limit":64,"cursor":null}),
            json!({"kind":"history","node_id":self.launch.binding["task_id"],"limit":64,"cursor":null}),
            json!({"kind":"search","text":"graph","kind_filter":null,"limit":64,"cursor":null}),
        ];
        for query in queries {
            let (status,result)=send(&self.app,"/v1/agent/graph/read",Some(new_agent),Some(json!({"api_version":1,"request_id":Uuid::new_v4(),"binding":self.launch.binding,"query":query}))).await;
            assert_eq!(status, StatusCode::OK, "{query}: {result}");
            let text = result.to_string();
            assert!(!text.contains("UNAUTHORIZED-CANARY"));
            assert!(!text.contains("metadata"));
            assert!(!text.contains("created_by"));
        }
        let denied = json!({"kind":"get","node_id":"00000000-0000-0000-0000-000000000004"});
        assert_eq!(send(&self.app,"/v1/agent/graph/read",Some(new_agent),Some(json!({"api_version":1,"request_id":Uuid::new_v4(),"binding":self.launch.binding,"query":denied}))).await.0,StatusCode::NOT_FOUND);
    }
    pub(super) async fn scope_vectors(&self, new_agent: &str) {
        let vectors: Vec<Value> = serde_json::from_str(include_str!(
            "../../../../../protocol/agent-access/v1/fixtures/scope-cases.json"
        ))
        .unwrap();
        for vector in vectors.iter().filter(|v| v["action"] == "read") {
            let mut request = vector["request"].clone();
            request["binding"] = self.launch.binding.clone();
            let token = match vector["principal_kind"].as_str().unwrap() {
                "agent_session" => new_agent,
                "node" => &self.node_token,
                _ => "ordinary-service-credential-0000000000000000",
            };
            let (status, body) = send(
                &self.app,
                "/v1/agent/graph/read",
                Some(token),
                Some(request),
            )
            .await;
            let expected = match vector["expected"].as_str().unwrap() {
                "allowed" => StatusCode::OK,
                "not_found" => StatusCode::NOT_FOUND,
                "unauthorized" => StatusCode::UNAUTHORIZED,
                _ => panic!("unhandled scope vector"),
            };
            assert_eq!(status, expected, "{}: {body}", vector["name"]);
            assert!(!body.to_string().contains("UNAUTHORIZED-CANARY"));
        }
    }
}
