use super::*;
struct Launch {
    app: Router,
    path: String,
    body: String,
    receipt: Value,
}
impl Launch {
    async fn post(&self, path: &str, body: &str) -> (StatusCode, Value) {
        send(&self.app, path, Some(body), "owner@localhost").await
    }
    async fn replay(&self) -> (StatusCode, Value) {
        self.post(&self.path, &self.body).await
    }
    async fn projects(&self, picker: &str) -> Value {
        send(&self.app, picker, None, "owner@localhost").await.1["projects"].clone()
    }
}
impl Scenario {
    pub(super) async fn check_worker_launch(&mut self) {
        // A selected-worker launch is a normal retained Graph task, pinned before readiness.
        let project = self.project;
        let cfg = Arc::make_mut(&mut self.f.state.config);
        cfg.profiled_execution = Some(Arc::new(
            aq_server::profiled_execution::config::ProfiledConfig {
                revision: 1,
                qualified_runtimes: vec![],
                definition_bodies: vec![],
                projects: vec![aq_server::profiled_execution::config::ProjectPolicy {
                    project_id: project,
                    enabled: true,
                    priority: 0,
                    source: aq_admission::SourceSelection::NamedRef {
                        reference: aq_admission::Label::new("refs/heads/main").unwrap(),
                    },
                    retention_handle: Uuid::new_v4(),
                    placement: None,
                    agent_grants: None,
                }],
            },
        ));
        sqlx::query("UPDATE nodes SET last_seen_at=$1 WHERE id=$2")
            .bind((self.f.state.clock)())
            .bind(self.f.node.node_id)
            .execute(&self.pool)
            .await
            .unwrap();
        let launch_app = aq_server::build_admin_router(self.f.state.clone());
        let launch_path = format!(
            "/api/human/projects/{project}/workers/{}/launch",
            self.f.node.node_id
        );
        let launch_id = Uuid::new_v4();
        let launch_body = request(launch_id, "1").replace("\"draft\"", "\"active\"");
        let (code, launched) = send(
            &launch_app,
            &launch_path,
            Some(&launch_body),
            "owner@localhost",
        )
        .await;
        assert_eq!(code, 200, "{launched}");
        self.assert_worker_pin(launch_id).await;
        let original = Launch {
            app: launch_app,
            path: launch_path,
            body: launch_body,
            receipt: launched,
        };
        self.check_launch_replay(&original).await;
        let picker = format!("/api/human/workers/{}/launch-projects", self.f.node.node_id);
        assert_eq!(
            original.projects(&picker).await.as_array().unwrap().len(),
            1
        );
        retirement::check_epic_retirement_registration(
            &self.f,
            &self.origin,
            self.config.graph_token_file.clone(),
            fixture::TOKEN,
        )
        .await;
        self.check_stopped_launch(&original, &picker).await;
    }
    async fn assert_worker_pin(&self, launch_id: Uuid) {
        let payload: Vec<u8> = sqlx::query_scalar(
            "SELECT payload_bytes FROM human_graph_operations WHERE operation_id=$1",
        )
        .bind(launch_id)
        .fetch_one(&self.pool)
        .await
        .unwrap();
        let payload: Value = serde_json::from_slice(&payload).unwrap();
        let metadata = &payload["operations"][0]["metadata"];
        assert_eq!(metadata["aq:requested_node_id"], json!(self.f.node.node_id));
        assert!(
            aq_machine_control::launch::eligible(
                &self.pool,
                launch_id,
                self.f.node.node_id,
                metadata
            )
            .await
            .unwrap()
        );
        assert!(
            !aq_machine_control::launch::eligible(&self.pool, launch_id, Uuid::new_v4(), metadata)
                .await
                .unwrap()
        );
    }
    async fn check_launch_replay(&self, launch: &Launch) {
        let project = self.project;
        let calls_before = self.trace.count();
        assert_eq!(launch.replay().await.1, launch.receipt);
        let other_path = format!(
            "/api/human/projects/{project}/workers/{}/launch",
            Uuid::new_v4()
        );
        assert_eq!(launch.post(&other_path, &launch.body).await.0, 409);
        assert_eq!(self.trace.count(), calls_before);
        assert_eq!(
            launch
                .post(&launch.path, &request(Uuid::new_v4(), "1"))
                .await
                .0,
            400
        );
        assert_eq!(
            launch
                .post(
                    &other_path,
                    &request(Uuid::new_v4(), "1").replace("\"draft\"", "\"active\"")
                )
                .await
                .0,
            409
        );
    }
    async fn check_stopped_launch(&self, launch: &Launch, picker: &str) {
        let project = self.project;
        let stop = json!({"operation_id":Uuid::new_v4(),"expected_revision":1}).to_string();
        let stopped = send(
            &launch.app,
            &format!("/api/projects/{project}/disable"),
            Some(&stop),
            "owner@localhost",
        )
        .await;
        assert_eq!(stopped.0, 200, "{}", stopped.1);
        assert_eq!(launch.projects(picker).await, json!([]));
        let new_launch = request(Uuid::new_v4(), "2").replace("\"draft\"", "\"active\"");
        assert_eq!(launch.post(&launch.path, &new_launch).await.0, 409);
        assert_eq!(
            launch.replay().await.1,
            launch.receipt,
            "Original launch receipts remain replayable after Stop"
        );
    }
}
