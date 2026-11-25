use super::*;
impl Registration<'_> {
    /// Spec 0020 settlement of the AQ-owned registration on disposable Graph:
    /// the dashboard deletes the verified closure gate, the owner restores it,
    /// approves, records five merges and ordinary builds plus the combined check,
    /// and closes the epic once the cleanup task has completed.
    pub(super) async fn check_settlement(&self) {
        let closure = Uuid::new_v5(&self.ids.registration, b"closure-gate");
        let guarded = self.assert_restore_guards(closure).await;
        self.check_restored_gate(closure, guarded).await;
        self.record_approval().await;
        self.check_evidence().await;
        self.close_after_cleanup(closure).await;
    }
    async fn assert_restore_guards(&self, closure: Uuid) -> i64 {
        let ids = self.ids;
        let version = |node: &Value| node["version"].to_string();
        let gate = self.at.node(closure).await;
        let guarded = gate["content_revision"].as_i64().unwrap();
        let (_, deleted) = owner_command(
            self.at.app,
            "task.delete",
            json!({"project_id":ids.project,"task_id":closure,"expected_version":version(&gate)}),
            OWNER,
        )
        .await;
        assert_eq!(deleted["data"]["disposition"], "confirmed", "{deleted}");
        assert_eq!(self.at.node(closure).await["deleted"], true);

        // Only the owner, only the current registration version, only the
        // guarded revision.
        let restore = json!({"revision":guarded});
        let (_, denied) = self
            .settle(
                "epic.retirement.restore",
                restore.clone(),
                "viewer@localhost",
            )
            .await;
        assert_ne!(disposition(&denied), "confirmed", "{denied}");
        let stale = json!({"project_id":ids.project,"registration_id":ids.registration,
        "expected_version":"1","revision":guarded});
        let (_, stale) = owner_command(self.at.app, "epic.retirement.restore", stale, OWNER).await;
        assert_eq!(disposition(&stale), "conflict", "{stale}");
        let (_, unverified) = self
            .command("epic.retirement.restore", json!({"revision":1}))
            .await;
        assert_eq!(disposition(&unverified), "rejected", "{unverified}");
        let (_, early) = self.command("epic.retirement.close", json!({})).await;
        assert_eq!(disposition(&early), "rejected", "{early}");

        guarded
    }
    async fn check_restored_gate(&self, closure: Uuid, guarded: i64) {
        let ids = self.ids;
        let restore = json!({"revision":guarded});
        let (op, restored) = self.command("epic.retirement.restore", restore).await;
        assert_eq!(disposition(&restored), "confirmed", "{restored}");
        let (_, replay) = send(
            self.at.app,
            &format!("/api/v1/operations/{op}"),
            None,
            OWNER,
        )
        .await;
        assert_eq!(
            replay["data"]["receipt_refs"],
            restored["data"]["receipt_refs"]
        );
        let gate = self.at.node(closure).await;
        assert_eq!(gate["deleted"], false);
        assert_eq!(gate["task_state"], "draft");
        assert_eq!(gate["verification"], "verified", "{gate}");
        assert_eq!(gate["last_verified_by"], "local-admin");
        assert_eq!(
            gate["metadata"]["aq:epic_feature_retirement"],
            json!({"registration":ids.registration,"role":"closure_gate"})
        );
        let context = self.at.graph_node(closure, "/context").await;
        assert!(
            context["edges"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["edge_type"] == "parent-child"
                    && e["source"] == json!(ids.epic)
                    && e["target"] == json!(closure))
        );
    }
    async fn record_approval(&self) {
        let ids = self.ids;
        let vetted: Value = (REPOSITORIES.iter().enumerate())
            .map(|(i, repo)| (repo.to_string(), json!(commit(i))))
            .collect();
        let decision =
            json!({"revision":1,"choice":"approve","vetted":vetted,"evidence":artifact(0xd0)});
        let (_, decided) = self.command("epic.retirement.decide", decision).await;
        assert_eq!(disposition(&decided), "confirmed", "{decided}");
        assert_eq!(self.at.node(ids.approval).await["task_state"], "completed");
        assert_eq!(self.at.node(ids.cleanup).await["task_state"], "active");
    }
    async fn check_evidence(&self) {
        let (merges, builds) = self.record_repositories(100, 1, 0x20).await;
        let mut partial = builds.clone();
        partial.remove(REPOSITORIES[4]);
        let integrated = |builds: serde_json::Map<String, Value>| {
            json!({"kind":"integrated","approval_revision":1,"evidence":artifact(0xee),
            "revisions":merges.clone(),"builds":builds,"passed":true})
        };
        let (_, refused) = self
            .command("epic.retirement.evidence", integrated(partial))
            .await;
        assert_eq!(disposition(&refused), "rejected", "{refused}");
        let (_, combined) = self
            .command("epic.retirement.evidence", integrated(builds))
            .await;
        assert_eq!(disposition(&combined), "confirmed", "{combined}");
        let (_, pending) = self.command("epic.retirement.close", json!({})).await;
        assert_eq!(disposition(&pending), "rejected", "{pending}");
    }
    async fn close_after_cleanup(&self, closure: Uuid) {
        let ids = self.ids;
        let At {
            origin,
            workspace,
            token,
            ..
        } = *self.at;
        // Stand-in for the cleanup's normal delivery settlement.
        let cleanup = self.at.node(ids.cleanup).await;
        let settled = reqwest::Client::new()
        .post(format!("{origin}/v1/workspaces/{workspace}/mutations"))
        .bearer_auth(token)
        .header("X-AQ-Graph-API-Version", "1")
        .json(&json!({"idempotency_key":format!("settle-{}", ids.cleanup),"reason":"Cleanup settled",
            "operations":[{"op":"update_node","id":ids.cleanup,"expected_version":cleanup["version"],
                "patch":{"task_state":"completed"}}]}))
        .send()
        .await
        .unwrap();
        assert!(settled.status().is_success());
        let (op, closed) = self.command("epic.retirement.close", json!({})).await;
        assert_eq!(disposition(&closed), "confirmed", "{closed}");
        let (_, replay) = send(
            self.at.app,
            &format!("/api/v1/operations/{op}"),
            None,
            OWNER,
        )
        .await;
        assert_eq!(
            replay["data"]["receipt_refs"],
            closed["data"]["receipt_refs"]
        );
        assert_eq!(self.at.node(ids.epic).await["task_state"], "completed");
        let gate = self.at.node(closure).await;
        assert_eq!(gate["task_state"], "completed");
        assert_eq!(gate["verification"], "verified");
        let state = &self.at.node(ids.registration).await["metadata"]["aq:epic_feature_retirement"];
        assert_eq!(
            state["closed"],
            json!(Uuid::new_v5(&ids.registration, b"close"))
        );
    }
}
