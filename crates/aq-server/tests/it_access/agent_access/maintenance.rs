use super::*;
impl Case {
    pub(super) async fn maintenance(&self, new_agent: &str) {
        let spool = tempfile::tempdir().unwrap();
        std::fs::create_dir(spool.path().join("status")).unwrap();
        std::fs::write(spool.path().join("status/maintenance"), "").unwrap();
        let mut maintenance_state = self.state.clone();
        let mut maintenance_config = self.state.config.as_ref().clone();
        maintenance_config.updates = Some(aq_server::config::UpdatesConfig {
            spool_dir: spool.path().to_path_buf(),
        });
        maintenance_state.config = Arc::new(maintenance_config);
        let admin = aq_server::build_admin_router(maintenance_state.clone());
        let worker = aq_server::build_worker_router(maintenance_state);
        let rotate = json!({"identity":self.launch.identity,"attempt":self.launch.attempt,"expected_credential_version":2});
        assert_eq!(
            send(
                &worker,
                "/v2/worker/agent-session",
                Some(&self.node_token),
                Some(rotate)
            )
            .await
            .0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            send(&worker, "/v1/agent/context", Some(new_agent), None)
                .await
                .0,
            StatusCode::OK,
            "existing read authority survives maintenance"
        );
        assert_eq!(
            send(
                &admin,
                &format!("/api/agent-access/{}/revoke", self.attempt_id),
                None,
                Some(json!({}))
            )
            .await
            .0,
            StatusCode::OK,
            "maintenance permits owner revocation"
        );
        assert_eq!(
            send(&worker, "/v1/agent/context", Some(new_agent), None)
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
    }
    pub(super) async fn cancellation(&self, new_agent: &str) {
        // Restore fixture authority solely to isolate the following cancellation check.
        sqlx::query(
        "UPDATE agent_launch_bindings SET revoked=FALSE,credential_verifier=$2 WHERE attempt_id=$1",
    )
    .bind(self.attempt_id)
    .bind(tokens::verifier(new_agent))
    .execute(&self.pool)
    .await
    .unwrap();
        sqlx::query("UPDATE execution_attempts SET state=jsonb_set(state,'{status}','\"cancelling\"') WHERE attempt_id=$1").bind(self.attempt_id).execute(&self.pool).await.unwrap();
        assert_eq!(
            send(&self.app, "/v1/agent/context", Some(new_agent), None)
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
    }
}
