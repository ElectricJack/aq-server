use super::*;
impl At<'_> {
    /// The registered Epic 0 shape on disposable Graph: an AQ owner registration
    /// whose gate was deleted and restored, and whose approval task completed as
    /// ordinary work and released the cleanup before any decision was recorded.
    /// Only an explicit, vetted owner decision settles it, without touching the
    /// original tasks; then the ordinary five-repository evidence closes the epic.
    pub(super) async fn check_released_before_decision(&self, project: Uuid) {
        let ids = self.create_tasks(project).await;
        let Retirement {
            epic,
            approval,
            cleanup,
            ..
        } = ids;
        let register = json!({"project_id":project,"epic_id":epic,"expected_version":"1","generation":1,
        "repositories":repositories(),"ordinary_default":false,
        "permanent_behavior":"Enable B in ordinary builds","adopt":{"approval":approval,"cleanup":cleanup}});
        let (_, registered) =
            owner_command(self.app, "epic.retirement.register", register, OWNER).await;
        assert_eq!(disposition(&registered), "confirmed", "{registered}");
        let registration = Registration {
            at: self,
            ids: &ids,
        };
        let before = registration.release_cleanup().await;
        registration.approve_released_tasks(before).await;
        registration.complete_released_cleanup().await;
    }
}
fn decision(revision: u32, choice: &str, vetted: &Value) -> Value {
    json!({"revision":revision,"choice":choice,"vetted":vetted,"evidence":artifact(0xd1)})
}
impl Registration<'_> {
    async fn release_cleanup(&self) -> [Value; 2] {
        let Retirement {
            registration,
            project,
            approval,
            cleanup,
            ..
        } = *self.ids;
        let closure = Uuid::new_v5(&registration, b"closure-gate");
        let gate = self.at.node(closure).await;
        let guarded = gate["content_revision"].as_i64().unwrap();
        let delete = json!({"project_id":project,"task_id":closure,"expected_version":gate["version"].to_string()});
        let (_, deleted) = owner_command(self.at.app, "task.delete", delete, OWNER).await;
        assert_eq!(disposition(&deleted), "confirmed", "{deleted}");
        let restore = json!({"revision":guarded});
        let (_, restored) = self.command("epic.retirement.restore", restore).await;
        assert_eq!(disposition(&restored), "confirmed", "{restored}");
        self.at.edit(approval, json!({"task_state":"active"})).await;
        self.at
            .edit(approval, json!({"task_state":"completed"}))
            .await;
        self.at.edit(cleanup, json!({"task_state":"active"})).await;
        let before = [self.at.node(approval).await, self.at.node(cleanup).await];
        let state = &self.at.node(registration).await["metadata"]["aq:epic_feature_retirement"];
        assert!(state.get("decision").is_none(), "{state}");
        before
    }
    async fn approve_released_tasks(&self, before: [Value; 2]) {
        let Retirement {
            registration,
            approval,
            cleanup,
            ..
        } = *self.ids;
        let vetted: Value = (REPOSITORIES.iter().enumerate())
            .map(|(i, repo)| (repo.to_string(), json!(commit(200 + i))))
            .collect();
        self.assert_rejected_decisions(&vetted).await;
        let approve = decision(1, "approve", &vetted);
        let (op, decided) = self.command("epic.retirement.decide", approve).await;
        assert_eq!(disposition(&decided), "confirmed", "{decided}");
        let (_, replay) = send(
            self.at.app,
            &format!("/api/v1/operations/{op}"),
            None,
            OWNER,
        )
        .await;
        assert_eq!(
            replay["data"]["receipt_refs"],
            decided["data"]["receipt_refs"]
        );
        for (id, was) in [approval, cleanup].into_iter().zip(&before) {
            let now = self.at.node(id).await;
            for key in ["version", "task_state", "metadata"] {
                assert_eq!(now[key], was[key], "{id} {key}");
            }
        }
        let anchor = Uuid::new_v5(&registration, b"decision/1");
        let state =
            self.at.node(registration).await["metadata"]["aq:epic_feature_retirement"].clone();
        assert_eq!(state["approved"], json!(anchor));
        assert_eq!(state["reconciled"], json!(anchor));
        assert_eq!(self.at.node(anchor).await["verification"], "verified");
        let again = decision(2, "approve", &vetted);
        let (_, again) = self.command("epic.retirement.decide", again).await;
        assert_eq!(disposition(&again), "rejected", "{again}");
    }
    async fn assert_rejected_decisions(&self, vetted: &Value) {
        let Retirement {
            registration,
            project,
            ..
        } = *self.ids;
        let early = json!({"kind":"merge","approval_revision":1,"evidence":artifact(0x10),
        "repository":REPOSITORIES[0],"pull_request":"https://github.com/Acme/service/pull/2","sha":commit(300)});
        let (_, early) = self.command("epic.retirement.evidence", early).await;
        assert_eq!(
            disposition(&early),
            "rejected",
            "task state is not approval: {early}"
        );
        let approve = decision(1, "approve", vetted);
        let (_, denied) = self
            .settle(
                "epic.retirement.decide",
                approve.clone(),
                "viewer@localhost",
            )
            .await;
        assert_ne!(disposition(&denied), "confirmed", "{denied}");
        let changes = decision(1, "request_changes", vetted);
        let (_, changes) = self.command("epic.retirement.decide", changes).await;
        assert_eq!(disposition(&changes), "rejected", "{changes}");
        let mut partial = vetted.clone();
        partial.as_object_mut().unwrap().remove(REPOSITORIES[4]);
        let partial = decision(1, "approve", &partial);
        let (_, partial) = self.command("epic.retirement.decide", partial).await;
        assert_eq!(disposition(&partial), "rejected", "{partial}");
        let mut stale = approve.clone();
        let fence =
            json!({"project_id":project,"registration_id":registration,"expected_version":"1"});
        stale
            .as_object_mut()
            .unwrap()
            .extend(fence.as_object().unwrap().clone());
        let (_, stale) = owner_command(self.at.app, "epic.retirement.decide", stale, OWNER).await;
        assert_eq!(disposition(&stale), "conflict", "{stale}");
    }
    async fn complete_released_cleanup(&self) {
        let Retirement {
            epic,
            registration,
            cleanup,
            ..
        } = *self.ids;
        let closure = Uuid::new_v5(&registration, b"closure-gate");
        let (merges, builds) = self.record_repositories(300, 2, 0x30).await;
        let combined = json!({"kind":"integrated","approval_revision":1,"evidence":artifact(0xef),
        "revisions":merges,"builds":builds,"passed":true});
        let (_, combined) = self.command("epic.retirement.evidence", combined).await;
        assert_eq!(disposition(&combined), "confirmed", "{combined}");
        // Stand-in for the cleanup's normal delivery settlement.
        self.at
            .edit(cleanup, json!({"task_state":"completed"}))
            .await;
        let (_, closed) = self.command("epic.retirement.close", json!({})).await;
        assert_eq!(disposition(&closed), "confirmed", "{closed}");
        assert_eq!(self.at.node(epic).await["task_state"], "completed");
        assert_eq!(self.at.node(closure).await["task_state"], "completed");
    }
}
