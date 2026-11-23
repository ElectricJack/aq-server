use super::*;

impl Host {
    /// A routing artifact is active: what the owner last activated on `aq.routing`.
    pub(super) async fn activated(&self, digest: &str) {
        sqlx::query("INSERT INTO playbook_engine_bindings(namespace_id,generation,binding,artifact,at_ms) VALUES($1,1,'aq.routing',$2,1) ON CONFLICT(namespace_id,generation,binding) DO UPDATE SET artifact=$2")
            .bind(self.service.namespace.namespace_id)
            .bind(digest)
            .execute(&self.f.db.pool().await)
            .await
            .unwrap();
    }
    pub(super) fn clock(&self) -> i64 {
        (self.f.state.clock)().timestamp_millis()
    }
    /// What placement records for a ready task that waits for a route.
    pub(super) async fn wanted(&self, task: Uuid, version: i64, project: Uuid) {
        sqlx::query("INSERT INTO route_needed_emissions(task_id,task_version,emitted_at_ms,wanted_version,wanted_at_ms,workspace_id,project_id,wanted_since_ms) VALUES($1,0,0,$2,$3,$4,$5,$3) ON CONFLICT(task_id) DO UPDATE SET wanted_since_ms=CASE WHEN route_needed_emissions.wanted_version=$2 THEN route_needed_emissions.wanted_since_ms ELSE $3 END,wanted_version=$2,wanted_at_ms=$3")
            .bind(task)
            .bind(version)
            .bind(self.clock())
            .bind(self.scope.workspace_id)
            .bind(project)
            .execute(&self.f.db.pool().await)
            .await
            .unwrap();
    }
    pub(super) async fn asked(&self) -> usize {
        (self.service.collect_route_needed(&self.runtime, 8).await).unwrap()
    }
    /// Time passes; placement keeps seeing the task wait.
    pub(super) async fn later(&self, ms: i64, task: Uuid, version: i64) {
        self.f
            .time
            .fetch_add(ms, std::sync::atomic::Ordering::SeqCst);
        self.wanted(task, version, self.scope.project_id).await;
    }

    /// What was raised about a task: (reason, cause, escalation, detail, project).
    pub(super) async fn raised(
        &self,
        task: Uuid,
    ) -> Vec<(String, String, String, String, Option<Uuid>)> {
        sqlx::query_as("SELECT reason,request->>'input',escalation,detail,project_id FROM internal_tasks WHERE purpose='routing_classification' AND affected_kind='task' AND affected_id=$1 AND escalate ORDER BY created_at_ms,request->>'input'")
            .bind(task.to_string())
            .fetch_all(&self.f.db.pool().await)
            .await
            .unwrap()
    }
    pub(super) async fn causes(&self, task: Uuid) -> Vec<String> {
        (self.raised(task).await.into_iter()).map(|r| r.1).collect()
    }

    pub(super) async fn waiting(&self, task: Uuid) -> Option<i64> {
        sqlx::query_scalar("SELECT wanted_at_ms FROM route_needed_emissions WHERE task_id=$1")
            .bind(task)
            .fetch_one(&self.f.db.pool().await)
            .await
            .unwrap()
    }

    pub(super) async fn assert_escalation_is_idempotent(&self, task: Uuid, unoffered: &Value) {
        // It follows the internal work failure path: a supervisor first.
        let pool = self.f.db.pool().await;
        let moved = aq_internal_work::store::escalate_due(&pool, self.clock())
            .await
            .unwrap();
        assert_eq!(moved.len(), 1);
        assert_eq!(
            (
                moved[0].affected_kind.as_str(),
                moved[0].affected_id.clone()
            ),
            ("task", task.to_string())
        );
        assert_eq!(self.raised(task).await[0].2, "supervisor");
        // Even resolved, the same cause at the same version is never raised again.
        aq_internal_work::store::resolve(&pool, moved[0].id)
            .await
            .unwrap();
        assert_eq!(
            self.route(task, 3, unoffered.clone()).await.output["reason"],
            "not_offered"
        );
        assert_eq!(self.causes(task).await, ["not_offered@3"]);
    }

    pub(super) fn assert_judged_route(&self, task: Uuid, artifact_digest: &str) {
        let stored = self.graph.stored(task).expect("the run routed the task");
        assert_eq!(
            (stored.c.to_string().as_str(), stored.s),
            ("simple-high", Source::Router)
        );
        assert_eq!(stored.d.as_deref(), Some(&artifact_digest[..16]));
        let (_, route) = self.routing.writes.lock().unwrap()[0].clone();
        let route = route.unwrap();
        assert_eq!(route.judgment.as_ref().unwrap()["labels"], json!(["low"]));
        assert!(!route.reasons.is_empty());
    }
}
