use super::*;
impl Case {
    pub(super) async fn provision(&self) -> Value {
        let mut small_profile = self.launch.profile.clone();
        small_profile["max_response_bytes"] =
            json!(self.launch.context.to_string().len().max(1024) + 10);
        let small_digest = tokens::verifier(&small_profile.to_string());
        sqlx::query("UPDATE execution_attempts SET state=jsonb_set(state,'{offer,spec,bindings,graph,profile_digest}',$2) WHERE attempt_id=$1").bind(self.attempt_id).bind(json!(small_digest)).execute(&self.pool).await.unwrap();
        let (status, _) = send(
        &self.app,
        "/api/agent-access/provision",
        None,
        Some(json!({"attempt_id":self.attempt_id,"profile":small_profile,"context":self.launch.context})),
    )
    .await;
        assert_eq!(
            status,
            StatusCode::PAYLOAD_TOO_LARGE,
            "snapshot fits but full context does not"
        );
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM agent_launch_bindings WHERE attempt_id=$1")
                .bind(self.attempt_id)
                .fetch_one(&self.pool)
                .await
                .unwrap();
        assert_eq!(count, 0, "oversized provision cannot create authority");
        sqlx::query("UPDATE execution_attempts SET state=jsonb_set(state,'{offer,spec,bindings,graph,profile_digest}',$2) WHERE attempt_id=$1").bind(self.attempt_id).bind(self.launch.binding["profile_digest"].clone()).execute(&self.pool).await.unwrap();
        let (status, _) = send(
        &self.app,
        "/api/agent-access/provision",
        None,
        Some(json!({"attempt_id":self.attempt_id,"profile":self.launch.profile,"context":self.launch.context})),
    )
    .await;
        assert_eq!(status, StatusCode::OK);
        small_profile
    }
    pub(super) async fn reject_identity(&self, issue: &Value) {
        for (field, value, status) in [
            ("node_id", json!(Uuid::new_v4()), StatusCode::UNAUTHORIZED),
            ("boot_id", json!(Uuid::new_v4()), StatusCode::CONFLICT),
        ] {
            let mut wrong = issue.clone();
            wrong["identity"][field] = value;
            assert_eq!(
                send(
                    &self.app,
                    "/v2/worker/agent-session",
                    Some(&self.node_token),
                    Some(wrong)
                )
                .await
                .0,
                status
            );
        }
        let mut wrong_generation = issue.clone();
        wrong_generation["attempt"]["generation"] = json!(2);
        assert_eq!(
            send(
                &self.app,
                "/v2/worker/agent-session",
                Some(&self.node_token),
                Some(wrong_generation)
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
    }
    pub(super) async fn reject_small_response(&self, issue: &Value, small_profile: Value) {
        sqlx::query("UPDATE agent_launch_bindings SET profile=$2 WHERE attempt_id=$1")
            .bind(self.attempt_id)
            .bind(&small_profile)
            .execute(&self.pool)
            .await
            .unwrap();
        assert_eq!(
            send(
                &self.app,
                "/v2/worker/agent-session",
                Some(&self.node_token),
                Some(issue.clone())
            )
            .await
            .0,
            StatusCode::PAYLOAD_TOO_LARGE
        );
        let (version,verifier):(i64,Option<String>)=sqlx::query_as("SELECT credential_version,credential_verifier FROM agent_launch_bindings WHERE attempt_id=$1").bind(self.attempt_id).fetch_one(&self.pool).await.unwrap();
        assert_eq!(version, 0);
        assert!(
            verifier.is_none(),
            "response refusal cannot commit an unreachable credential"
        );
        sqlx::query("UPDATE agent_launch_bindings SET profile=$2 WHERE attempt_id=$1")
            .bind(self.attempt_id)
            .bind(&self.launch.profile)
            .execute(&self.pool)
            .await
            .unwrap();
    }
}
