use super::*;
impl Case {
    pub(in super::super) async fn verify_invalid_reservation(&self) {
        let attempt = self.attempt;
        // This synthetic fixture reuses a session for each policy. Give the
        // reviewer an empty creation ledger before testing its own admission.
        sqlx::query("DELETE FROM agent_created_tasks WHERE attempt_id=$1")
            .bind(attempt)
            .execute(&self.e.pool)
            .await
            .unwrap();
        let malformed = Uuid::new_v4();
        let (status, invalid) = self
            .command(
                "task.create",
                json!({"params":{
                    "operation_id":malformed,"parent_id":"self","title":"  "
                }}),
            )
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{invalid}");
        let reserved: i64 =
            sqlx::query_scalar("SELECT count(*) FROM agent_created_tasks WHERE attempt_id=$1")
                .bind(attempt)
                .fetch_one(&self.e.pool)
                .await
                .unwrap();
        assert_eq!(reserved, 0, "invalid requests cannot reserve a task");
    }
    pub(in super::super) async fn verify_competing_parents(&self) {
        let root = id(&self.nodes.own);
        let planner = self.nodes.planner;
        let (operation, task) = (Uuid::new_v4(), Uuid::new_v4());
        let compete = |parent: Uuid| {
            self.command(
            "task.graph.create",
            json!({"operation_id":operation,
        "params":{"expected_version":"1","tasks":[{"id":task,"parent_id":parent,"title":"Competing draft","task_state":"draft"}]}}),
        )
        };
        let (own_side, subject_side) = tokio::join!(compete(root), compete(planner));
        let (winner, parent, loser) = if own_side.0 == StatusCode::OK {
            (own_side.1, root, subject_side.0)
        } else {
            (subject_side.1, planner, own_side.0)
        };
        assert_eq!(winner["data"]["disposition"], "confirmed", "{winner}");
        assert_eq!(loser, StatusCode::CONFLICT);
        let saved: (Uuid, Uuid) = sqlx::query_as(
            "SELECT parent_id,root_task_id FROM agent_created_tasks WHERE task_id=$1",
        )
        .bind(task)
        .fetch_one(&self.e.pool)
        .await
        .unwrap();
        assert_eq!(
            saved,
            (parent, parent),
            "Only the journal-winning parent can confer authority"
        );
        let last = self.posted.lock().unwrap().last().unwrap().clone();
        assert!(
            last["operations"]
                .as_array()
                .unwrap()
                .iter()
                .any(|op| op["edge_type"] == "parent-child"
                    && op["target"] == json!(task)
                    && op["source"] == json!(parent))
        );
    }
    pub(in super::super) async fn verify_expiry_before_journaling(&self) {
        let draft = self.nodes.draft;
        let pending = ScopePause {
            task: json!(draft),
            started: Arc::new(tokio::sync::Notify::new()),
            resume: Arc::new(tokio::sync::Notify::new()),
        };
        *self.pause.lock().unwrap() = Some(pending.clone());
        let expiring = change(draft);
        let expiration = async {
            tokio::time::timeout(
                std::time::Duration::from_secs(3),
                pending.started.notified(),
            )
            .await
            .unwrap();
            self.e.time.fetch_add(20_000, Ordering::SeqCst);
            pending.resume.notify_one();
        };
        let ((status, expired), ()) =
            tokio::join!(self.command("task.edit", expiring.clone()), expiration);
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{expired}");
        assert_eq!(expired["code"], "session_expired");
        let journaled: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM dashboard_operations WHERE operation_id=$1)",
        )
        .bind(
            expiring["operation_id"]
                .as_str()
                .unwrap()
                .parse::<Uuid>()
                .unwrap(),
        )
        .fetch_one(&self.e.pool)
        .await
        .unwrap();
        assert!(
            !journaled,
            "Expired credential cannot admit or dispatch an action"
        );
    }
}
