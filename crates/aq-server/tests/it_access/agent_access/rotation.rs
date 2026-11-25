use super::*;
impl Case {
    pub(super) async fn update_node_token(&mut self, issue: &Value) {
        // A credential authenticated before a real node-lock wait cannot issue an
        // agent credential after that machine credential rotates under the lock.
        let mut locked = self.pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
            .bind(self.node)
            .execute(&mut *locked)
            .await
            .unwrap();
        let xid: String = sqlx::query_scalar("SELECT txid_current()::text")
            .fetch_one(&mut *locked)
            .await
            .unwrap();
        let pending_app = self.app.clone();
        let old_token = self.node_token.clone();
        let pending_issue = issue.clone();
        let pending = tokio::spawn(async move {
            send(
                &pending_app,
                "/v2/worker/agent-session",
                Some(&old_token),
                Some(pending_issue),
            )
            .await
        });
        self.wait_for_node_lock(&xid).await;
        let rotated_node_token = tokens::new_node_credential();
        sqlx::query("UPDATE nodes SET credential_hash=$2 WHERE id=$1")
            .bind(self.node)
            .bind(tokens::verifier(&rotated_node_token))
            .execute(&mut *locked)
            .await
            .unwrap();
        locked.commit().await.unwrap();
        assert_eq!(
            pending.await.unwrap().0,
            StatusCode::UNAUTHORIZED,
            "the old credential cannot mint agent authority after rotation"
        );
        let (version, verifier): (i64, Option<String>) = sqlx::query_as(
        "SELECT credential_version,credential_verifier FROM agent_launch_bindings WHERE attempt_id=$1")
        .bind(self.attempt_id).fetch_one(&self.pool).await.unwrap();
        assert_eq!(version, 0);
        assert!(
            verifier.is_none(),
            "refused stale machine credential changed issuance state"
        );
        assert_eq!(
            send(
                &self.app,
                "/v2/worker/agent-session",
                Some(&self.node_token),
                Some(issue.clone())
            )
            .await
            .0,
            StatusCode::UNAUTHORIZED
        );
        // The following positive issuance and all other cases use the new credential.
        self.node_token = rotated_node_token;
    }
    pub(super) async fn wait_for_node_lock(&self, xid: &str) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let waiting: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM pg_locks WHERE transactionid::text=$1 AND NOT granted)",
        )
        .bind(xid)
        .fetch_one(&self.pool)
        .await
        .unwrap();
            if waiting {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "issuance did not reach the observed node lock wait"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }
    pub(super) async fn issue_and_rotate(&self, issue: Value) -> String {
        let (status, issued) = send(
            &self.app,
            "/v2/worker/agent-session",
            Some(&self.node_token),
            Some(issue.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{issued}");
        let agent = issued["token"].as_str().unwrap();
        assert_eq!(
            send(&self.app, "/v1/agent/context", Some(&self.node_token), None)
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            send(&self.app, "/v1/agent/context", Some(agent), None)
                .await
                .0,
            StatusCode::OK
        );
        assert_eq!(
            send(
                &self.app,
                "/v2/worker/agent-session",
                Some(&self.node_token),
                Some(issue.clone())
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            send(&self.app, "/v1/agent/context", Some(agent), None)
                .await
                .0,
            StatusCode::OK,
            "retry cannot invalidate credential"
        );
        let persisted: Option<String> = sqlx::query_scalar(
            "SELECT credential_verifier FROM agent_launch_bindings WHERE attempt_id=$1",
        )
        .bind(self.attempt_id)
        .fetch_one(&self.pool)
        .await
        .unwrap();
        assert_eq!(persisted.as_deref(), Some(tokens::verifier(agent).as_str()));
        let mut rotate = issue;
        rotate["expected_credential_version"] = json!(1);
        let (status, rotated) = send(
            &self.app,
            "/v2/worker/agent-session",
            Some(&self.node_token),
            Some(rotate),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            send(&self.app, "/v1/agent/context", Some(agent), None)
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        rotated["token"].as_str().unwrap().to_owned()
    }
}
