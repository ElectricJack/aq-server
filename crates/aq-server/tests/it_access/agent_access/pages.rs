use super::*;
impl Case {
    pub(super) async fn first_page(&self, new_agent: &str, paged: &Value) -> Value {
        let (status, first) = send(
            &self.app,
            "/v1/agent/graph/read",
            Some(new_agent),
            Some(self.graph_request(paged.clone())),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{first}");
        assert_eq!(first["result"]["nodes"].as_array().unwrap().len(), 1);
        assert_eq!(
            first["result"]["nodes"][0]["id"],
            self.launch.binding["task_id"]
        );
        let mut hidden = paged.clone();
        hidden["text"] = json!("hidden");
        let (status, partial) = send(
            &self.app,
            "/v1/agent/graph/read",
            Some(new_agent),
            Some(self.graph_request(hidden)),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(partial["result"]["nodes"].as_array().unwrap().is_empty());
        assert_eq!(partial["result"]["coverage"]["status"], "partial");
        assert!(!partial.to_string().contains("UNAUTHORIZED-CANARY"));
        let budget = json!({"kind":"context","node_id":self.launch.binding["task_id"],"depth":1,"direction":"both","max_nodes":64,"max_edges":256,"max_bytes":1024});
        assert_eq!(
            send(
                &self.app,
                "/v1/agent/graph/read",
                Some(new_agent),
                Some(self.graph_request(budget))
            )
            .await
            .0,
            StatusCode::PAYLOAD_TOO_LARGE
        );
        let budget = json!({"kind":"context","node_id":self.launch.binding["task_id"],"depth":1,"direction":"both","max_nodes":1,"max_edges":1,"max_bytes":1048576});
        let (_, partial) = send(
            &self.app,
            "/v1/agent/graph/read",
            Some(new_agent),
            Some(self.graph_request(budget)),
        )
        .await;
        assert_eq!(partial["result"]["coverage"]["status"], "partial");
        first
    }
    pub(super) async fn cursor_replay(&self, new_agent: &str, paged: Value, first: Value) -> Value {
        let cursor = first["result"]["next_cursor"].clone();
        // aq-worker's leak guard refuses agent output holding an `aqa_` credential
        // shape, so a cursor an agent prints must never look like one.
        let opaque = cursor.as_str().unwrap();
        assert!(opaque.starts_with("aqc_") && !opaque.starts_with("aqa_"));
        assert_eq!(opaque.len(), 47);
        assert_ne!(cursor, "visible-two");
        let mut next = paged.clone();
        next["cursor"] = cursor.clone();
        next["text"] = json!("changed");
        assert_eq!(
            send(
                &self.app,
                "/v1/agent/graph/read",
                Some(new_agent),
                Some(self.graph_request(next.clone()))
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        next["text"] = json!("paged");
        let (status, second) = send(
            &self.app,
            "/v1/agent/graph/read",
            Some(new_agent),
            Some(self.graph_request(next.clone())),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{second}");
        assert!(second["result"]["next_cursor"].is_null());
        // A cursor issued before the `aqc_` prefix (`aqa_`) is still honoured: the
        // server looks cursors up by the verifier of the whole opaque string.
        let legacy = format!("aqa_{}", &opaque[4..]);
        sqlx::query("UPDATE agent_read_cursors SET verifier=$1 WHERE verifier=$2")
            .bind(tokens::verifier(&legacy))
            .bind(tokens::verifier(opaque))
            .execute(&self.pool)
            .await
            .unwrap();
        next["cursor"] = json!(legacy);
        let (status, legacy_page) = send(
            &self.app,
            "/v1/agent/graph/read",
            Some(new_agent),
            Some(self.graph_request(next.clone())),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{legacy_page}");
        assert_eq!(legacy_page["result"], second["result"]);
        next
    }
    pub(super) async fn rotation_expires_cursor(&self, new_agent: &str, next: Value) {
        // The same opaque cursor ceases to authorize after credential rotation.
        let rotate = json!({"identity":self.launch.identity,"attempt":self.launch.attempt,"expected_credential_version":2});
        let (_, third) = send(
            &self.app,
            "/v2/worker/agent-session",
            Some(&self.node_token),
            Some(rotate),
        )
        .await;
        let third_token = third["token"].as_str().unwrap();
        assert_eq!(
            send(
                &self.app,
                "/v1/agent/graph/read",
                Some(third_token),
                Some(self.graph_request(next))
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        // Return to version 2 only in this disposable fixture so the remaining
        // independent queued-expiry/cancellation checks use the known token.
        sqlx::query("UPDATE agent_launch_bindings SET credential_version=2,credential_verifier=$2 WHERE attempt_id=$1").bind(self.attempt_id).bind(tokens::verifier(new_agent)).execute(&self.pool).await.unwrap();
    }
}
