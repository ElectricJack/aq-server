use super::*;
impl Scenario {
    pub(super) async fn confirm_creation(&self) -> (Uuid, Value) {
        let op = Uuid::new_v4();
        let raw = request(op, "1");
        let (code, created) = self.owner(&self.path, Some(&raw)).await;
        assert_eq!(code, StatusCode::OK, "{created}");
        status_schema(&created);
        let ownership = (
            &created["actor_id"],
            &created["task_id"],
            &created["receipt"]["workspace_id"],
        );
        assert_eq!(
            ownership,
            (&json!(self.f.actor), &json!(op), &json!(self.workspace))
        );
        let digest = format!(
            "sha256:{}",
            hex::encode(sha2::Sha256::digest(raw.as_bytes()))
        );
        assert_eq!(
            (&created["state"], &created["request_digest"]),
            (&json!("confirmed"), &json!(digest))
        );
        assert!(!created.to_string().contains(fixture::TOKEN));
        let (_, again) = self.owner(&self.path, Some(&raw)).await;
        assert_eq!(again, created);
        assert_eq!(self.trace.count(), 1);
        let lookup = format!("/api/human/operations/{op}");
        assert_eq!(self.owner(&lookup, None).await.1, created);
        assert_eq!(self.trace.count(), 1);
        assert_eq!(
            self.owner(&self.path, Some(&format!("{raw} "))).await.0,
            StatusCode::CONFLICT
        );
        assert_eq!(self.trace.count(), 1);
        self.check_saved_intent(op, &raw).await;
        self.check_action_replay(op).await;
        (op, created)
    }
    async fn check_saved_intent(&self, op: Uuid, raw: &str) {
        let saved: Vec<u8> = sqlx::query_scalar(
            "SELECT request_bytes FROM human_graph_operations WHERE operation_id=$1",
        )
        .bind(op)
        .fetch_one(&self.pool)
        .await
        .unwrap();
        assert_eq!(saved, raw.as_bytes());
        for sql in [
            "UPDATE human_graph_operations SET request_bytes='changed' WHERE operation_id=$1",
            "UPDATE human_graph_operations SET dispatch_count=0 WHERE operation_id=$1",
            "DELETE FROM human_graph_operations WHERE operation_id=$1",
        ] {
            assert!(sqlx::query(sql).bind(op).execute(&self.pool).await.is_err());
        }
        assert!(
            sqlx::query("TRUNCATE human_graph_operations")
                .execute(&self.pool)
                .await
                .is_err()
        );
    }
    async fn check_action_replay(&self, op: Uuid) {
        let (code, union) = self
            .owner("/api/v1/actions", Some(&self.action(op, "1")))
            .await;
        assert_eq!(code, 200, "{union}");
        assert_eq!(
            (
                &union["data"]["disposition"],
                &union["data"]["affected_entities"][0]["id"]
            ),
            (&json!("confirmed"), &json!(op))
        );
        assert_eq!(
            self.owner(&format!("/api/v1/operations/{op}"), None)
                .await
                .1["data"],
            union["data"]
        );
        assert_eq!(self.trace.count(), 1);
    }
    pub(super) async fn assert_stale_revision(&self) {
        // Stale revision has durable proof of no dispatch, not a generic ambiguous409.
        let stale = Uuid::new_v4();
        let (code, refused) = self.owner(&self.path, Some(&request(stale, "2"))).await;
        assert_eq!(code, StatusCode::CONFLICT, "{refused}");
        status_schema(&refused);
        assert_eq!(
            (&refused["effect"], &refused["recovery"]["reason"]),
            (
                &json!("not_dispatched"),
                &json!("project_binding_changed_before_dispatch")
            )
        );
        assert_eq!(self.trace.count(), 1);
        assert_eq!(
            self.owner(&format!("/api/human/operations/{stale}"), None)
                .await
                .1,
            refused
        );
    }
    pub(super) async fn confirm_lost_response(&self) -> Uuid {
        let (workspace, project) = (self.workspace, self.project); // Real Graph committed, but AQ lost the response. Lookup does not retry.
        let lost = Uuid::new_v4();
        self.trace.lose.store(true, Ordering::SeqCst);
        let (code, unknown) = self.owner(&self.path, Some(&request(lost, "1"))).await;
        assert_eq!(code, StatusCode::ACCEPTED, "{unknown}");
        status_schema(&unknown);
        assert_eq!(unknown["state"], "unknown");
        assert_eq!(unknown["effect"], "possible");
        assert!(
            aq_human_work::task_git_author::lookup(&self.pool, project, workspace, lost)
                .await
                .unwrap()
                .is_none()
        );
        let count = self.trace.count();
        let lost_path = format!("/api/human/operations/{lost}");
        assert_eq!(self.owner(&lost_path, None).await.1, unknown);
        assert_eq!(self.trace.count(), count);
        let (code, unresolved) = self
            .owner(&format!("/api/v1/operations/{lost}"), None)
            .await;
        assert_eq!(code, 200, "{unresolved}");
        assert_eq!(unresolved["data"]["disposition"], "unknown");
        let bootstrap = self.owner("/api/v1/bootstrap", None).await.1;
        assert!(
            bootstrap["pending_operations"]
                .as_array()
                .unwrap()
                .contains(&json!(lost))
        );
        assert_eq!(self.trace.count(), count);
        self.verify_committed_task(lost).await;
        self.recover_lost(lost, count).await;
        lost
    }
    async fn verify_committed_task(&self, lost: Uuid) {
        let (origin, workspace, project) = (&self.origin, self.workspace, self.project);
        let token = fixture::TOKEN;
        let client = reqwest::Client::new();
        let actual: Value = client
            .get(format!("{origin}/v1/workspaces/{workspace}/nodes/{lost}"))
            .bearer_auth(token)
            .header("X-AQ-Graph-API-Version", "1")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let intent = json!({
           "title":"Human authored task","body":"Durable source body",
           "acceptance_criteria":"Exactly one actual Graph task","labels":["human"],"task_state":"draft",
        });
        for key in [
            "title",
            "body",
            "acceptance_criteria",
            "labels",
            "task_state",
        ] {
            assert_eq!(actual[key], intent[key], "{key}");
        }
        assert_eq!(
            (&actual["id"], &actual["version"], &actual["created_by"]),
            (&json!(lost), &json!(1), &json!("local-admin"))
        );
        let meta = &actual["metadata"];
        assert_eq!(
            (&meta["aq:human_actor_id"], &meta["aq:project_id"]),
            (&json!(self.f.actor), &json!(project))
        );
    }
    async fn recover_lost(&self, lost: Uuid, count: usize) {
        let (workspace, project) = (self.workspace, self.project);
        let lost_path = format!("/api/human/operations/{lost}");
        let (code, recovered) = self
            .owner(&format!("{lost_path}/recover"), Some("{}"))
            .await;
        assert_eq!(code, StatusCode::OK, "{recovered}");
        status_schema(&recovered);
        assert_eq!(recovered["state"], "confirmed");
        assert_eq!(recovered["receipt"]["version"], "1");
        let author = aq_human_work::task_git_author::lookup(&self.pool, project, workspace, lost)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (author.name.as_str(), author.email.as_str()),
            ("owner", "owner@localhost")
        );
        assert_eq!(self.owner(&lost_path, None).await.1, recovered);
        assert_eq!(
            aq_human_work::task_git_author::lookup(&self.pool, project, workspace, lost)
                .await
                .unwrap(),
            Some(author)
        );
        assert_eq!(self.trace.count(), count + 1);
        {
            let all = self.trace.bodies.lock().await;
            assert_eq!(
                all[1], all[2],
                "recovery must reuse exact mapped Graph bytes"
            );
        }
        let resolved = self
            .owner(&format!("/api/v1/operations/{lost}"), None)
            .await
            .1;
        assert_eq!(resolved["data"]["disposition"], "confirmed");
        assert_eq!(resolved["data"]["operation_id"], json!(lost));
        assert!(
            !self.owner("/api/v1/bootstrap", None).await.1["pending_operations"]
                .as_array()
                .unwrap()
                .contains(&json!(lost))
        );
    }
}
