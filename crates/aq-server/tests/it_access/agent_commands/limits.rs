use super::*;
use crate::messaging::Env;
pub(super) struct Case {
    e: Env,
    attempt: Uuid,
    own: Uuid,
    project: Uuid,
}
impl Case {
    pub(super) async fn new() -> Self {
        let e = setup_with(Some(json!([
        {"operation":"task.create.subtask","scope":"task:subtree","limits":{"max_created_tasks":3,"max_depth":2}},
        {"operation":"task.read","scope":"project:self"}
    ])))
    .await;
        let attempt = Uuid::parse_str(e.binding["attempt_id"].as_str().unwrap()).unwrap();
        let own = Uuid::parse_str(e.binding["task_id"].as_str().unwrap()).unwrap();
        let project = Uuid::parse_str(e.binding["project_id"].as_str().unwrap()).unwrap();

        Self {
            e,
            attempt,
            own,
            project,
        }
    }
    async fn create(&self, params: Value) -> (StatusCode, Value) {
        send(
            &self.e.app,
            "/api/v1/commands/task.create",
            Some(&self.e.token),
            Some(json!({"params":params})),
        )
        .await
    }
    async fn grants(&self) -> Vec<(String, Option<String>, Option<String>)> {
        sqlx::query_as::<_, (String, Option<String>, Option<String>)>("SELECT decision,reason_code,grant_operation FROM command_audit WHERE attempt_id=$1 AND command='task.create' ORDER BY recorded_at_ms,audit_id")
            .bind(self.attempt).fetch_all(&self.e.pool).await.unwrap()
    }
    pub(super) async fn verify_top_level(&self) {
        // No parent: a top-level task needs task.create.project, which is not held.
        let (status, body) = self
            .create(json!({"operation_id":Uuid::new_v4(),"title":"Unrelated top-level task"}))
            .await;
        assert_eq!(
            (status, code(&body)),
            (StatusCode::FORBIDDEN, "grant_missing".into()),
            "{body}"
        );
        assert_eq!(body["grant"], "task.create.project");
        assert_eq!(body["held"][0]["operation"], "task.create.subtask");
        // Its audit row names the grant the refusal names, not the command's
        // default `task.create.subtask` (spec 0006 phase 6 proof, item 9).
        let deny = |grant: &str| {
            (
                "deny".to_string(),
                Some("grant_missing".to_string()),
                Some(grant.to_string()),
            )
        };
        assert_eq!(self.grants().await, vec![deny("task.create.project")]);
    }
    pub(super) async fn verify_refused_inputs(&self) {
        // A parent outside the subtree.
        let (status, body) = self
            .create(json!({"operation_id":Uuid::new_v4(),"title":"x","parent_id":Uuid::new_v4()}))
            .await;
        assert_eq!(
            (status, code(&body)),
            (StatusCode::FORBIDDEN, "scope_denied".into()),
            "{body}"
        );
        // A hint that could launch the child on another level.
        let (status, body) = self.create(json!({"operation_id":Uuid::new_v4(),"title":"x","parent_id":"self","intelligence_hint":"deep-high"})).await;
        assert_eq!(
            (status, code(&body)),
            (StatusCode::FORBIDDEN, "scope_denied".into()),
            "{body}"
        );
        // `profile_hint` no longer exists: it is an unknown field.
        let (status, body) = self.create(json!({"operation_id":Uuid::new_v4(),"title":"x","parent_id":"self","profile_hint":"admin"})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        // An agent names the role, and it must exist (spec 0014 §5.5). The
        // refusal comes before the quota is reserved.
        let (status, body) = self.create(
        json!({"operation_id":Uuid::new_v4(),"title":"x","parent_id":"self","role":"tester"}),
    )
    .await;
        assert_eq!(
            (status, code(&body)),
            (StatusCode::BAD_REQUEST, "invalid_request".into()),
            "{body}"
        );
        assert_eq!(
            body["message"],
            "Role `tester` does not exist. The roles are: developer, reviewer."
        );
        let reserved: i64 = sqlx::query_scalar("SELECT count(*) FROM agent_created_tasks")
            .fetch_one(&self.e.pool)
            .await
            .unwrap();
        assert_eq!(reserved, 0);
    }
    pub(super) async fn verify_depth_and_grant(&self) {
        // Depth: a grandchild (depth 2) is allowed, a great-grandchild is not.
        let (child, grandchild) = (Uuid::new_v4(), Uuid::new_v4());
        for (task, parent, depth) in [(child, self.own, 1), (grandchild, child, 2)] {
            sqlx::query("INSERT INTO agent_created_tasks(task_id,attempt_id,project_id,parent_id,root_task_id,depth,operation_id,created_at_ms) VALUES($1,$2,$3,$4,$5,$6,$1,0)")
            .bind(task).bind(self.attempt).bind(self.project).bind(parent).bind(self.own).bind(depth)
            .execute(&self.e.pool).await.unwrap();
        }
        let (status, body) = self
            .create(json!({"operation_id":Uuid::new_v4(),"title":"x","parent_id":grandchild}))
            .await;
        assert_eq!(
            (status, code(&body)),
            (StatusCode::FORBIDDEN, "quota_exceeded".into()),
            "{body}"
        );
        assert!(body["message"].as_str().unwrap().contains("max_depth"));
        // Within scope and depth: admitted and reserved against the quota (the
        // Graph write itself has no project binding in this fixture).
        let op = Uuid::new_v4();
        let (status, body) = self
            .create(json!({"operation_id":op,"title":"Verify changelog links","parent_id":child}))
            .await;
        assert!(
            !matches!(
                code(&body).as_str(),
                "grant_missing" | "scope_denied" | "quota_exceeded"
            ),
            "{status} {body}"
        );
        let depth: Option<i32> =
            sqlx::query_scalar("SELECT depth FROM agent_created_tasks WHERE task_id=$1")
                .bind(op)
                .fetch_one(&self.e.pool)
                .await
                .unwrap();
        assert_eq!(depth, Some(2));
        // An admitted subtask create records the grant it used.
        let allowed = self
            .grants()
            .await
            .into_iter()
            .filter(|r| r.0 == "allow")
            .collect::<Vec<_>>();
        assert!(!allowed.is_empty());
        assert!(
            allowed
                .iter()
                .all(|r| r.2.as_deref() == Some("task.create.subtask")),
            "{allowed:?}"
        );
    }
    pub(super) async fn verify_quota(&self) {
        // Quota: three created by this attempt; a fourth is refused.
        let (status, body) = self
            .create(json!({"operation_id":Uuid::new_v4(),"title":"x","parent_id":"self"}))
            .await;
        assert_eq!(
            (status, code(&body)),
            (StatusCode::FORBIDDEN, "quota_exceeded".into()),
            "{body}"
        );
        assert!(
            body["message"]
                .as_str()
                .unwrap()
                .contains("max_created_tasks")
        );
        let denials: i64 = sqlx::query_scalar("SELECT count(*) FROM command_audit WHERE attempt_id=$1 AND command='task.create' AND decision='deny'")
        .bind(self.attempt).fetch_one(&self.e.pool).await.unwrap();
        assert!(denials >= 5, "{denials}");
    }
}
fn code(body: &Value) -> String {
    body["code"].as_str().unwrap_or_default().to_owned()
}
