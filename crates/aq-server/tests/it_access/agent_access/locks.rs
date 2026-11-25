use super::*;
impl Case {
    pub(super) async fn read_during_graph_barrier(&self, new_agent: &str, expire: bool) {
        self.fake
            .hold
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let read_app = self.app.clone();
        let read_token = new_agent.to_owned();
        let read_request =
            self.graph_request(json!({"kind":"get","node_id":self.launch.binding["task_id"]}));
        let pending = tokio::spawn(async move {
            send(
                &read_app,
                "/v1/agent/graph/read",
                Some(&read_token),
                Some(read_request),
            )
            .await
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            self.fake.arrived.notified(),
        )
        .await
        .unwrap();
        if expire {
            self.time
                .store(1_020_000, std::sync::atomic::Ordering::SeqCst);
        } else {
            sqlx::query("UPDATE agent_launch_bindings SET revoked=TRUE WHERE attempt_id=$1")
                .bind(self.attempt_id)
                .execute(&self.pool)
                .await
                .unwrap();
        }
        self.fake.release.notify_one();
        let (status, body) = pending.await.unwrap();
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
        assert!(body.get("result").is_none());
        self.time
            .store(1_000_000, std::sync::atomic::Ordering::SeqCst);
        sqlx::query("UPDATE agent_launch_bindings SET revoked=FALSE WHERE attempt_id=$1")
            .bind(self.attempt_id)
            .execute(&self.pool)
            .await
            .unwrap();
    }
    pub(super) async fn expire_queued_issuance(&self) {
        let mut locked = self.pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
            .bind(self.node)
            .execute(&mut *locked)
            .await
            .unwrap();
        let pending_app = self.app.clone();
        let pending_token = self.node_token.clone();
        let queued = json!({"identity":self.launch.identity,"attempt":self.launch.attempt,"expected_credential_version":2});
        let pending = tokio::spawn(async move {
            send(
                &pending_app,
                "/v2/worker/agent-session",
                Some(&pending_token),
                Some(queued),
            )
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert!(!pending.is_finished());
        self.time
            .store(1_200_000, std::sync::atomic::Ordering::SeqCst);
        locked.commit().await.unwrap();
        assert_eq!(pending.await.unwrap().0, StatusCode::UNAUTHORIZED);
        self.time
            .store(1_000_000, std::sync::atomic::Ordering::SeqCst);
    }
    pub(super) async fn expire_queued_read(&self, new_agent: &str) {
        let mut locked = self.pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
            .bind(self.node)
            .execute(&mut *locked)
            .await
            .unwrap();
        let pending_app = self.app.clone();
        let pending_token = new_agent.to_owned();
        let pending = tokio::spawn(async move {
            send(
                &pending_app,
                "/v1/agent/context",
                Some(&pending_token),
                None,
            )
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert!(!pending.is_finished(), "authority waits for node lock");
        self.time
            .store(1_020_000, std::sync::atomic::Ordering::SeqCst);
        locked.commit().await.unwrap();
        assert_eq!(
            pending.await.unwrap().0,
            StatusCode::UNAUTHORIZED,
            "expiry sampled after lock"
        );
        self.time
            .store(1_000_000, std::sync::atomic::Ordering::SeqCst);
    }
}
