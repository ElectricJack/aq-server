use super::*;
pub(super) struct Launch {
    pub(super) binding: Value,
    profile: Value,
    context: Value,
    identity: Value,
    attempt: Value,
}
impl Launch {
    pub(super) fn new(
        node: Uuid,
        attempt: Option<Uuid>,
        edit: impl FnOnce(&mut Value, &mut Value),
    ) -> Self {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../../../protocol/agent-access/v1/fixtures/base.json"
        ))
        .unwrap();
        let mut profile = fixture["context"]["profile"].clone();
        for cap in ["message.send", "message.read", "message.ack"] {
            profile["capabilities"]
                .as_array_mut()
                .unwrap()
                .push(json!(cap));
        }
        let mut context = fixture["context"]["context"].clone();
        let original_context = context.clone();
        edit(&mut profile, &mut context);
        let mut binding = fixture["context"]["binding"].clone();
        if let Some(attempt) = attempt {
            binding["attempt_id"] = json!(attempt);
        }
        binding["node_id"] = json!(node);
        binding["profile_digest"] = json!(tokens::verifier(&profile.to_string()));
        if context != original_context {
            binding["context_digest"] = json!(tokens::verifier(&context.to_string()));
        }
        let boot = binding["boot_id"].clone();
        let identity = json!({"node_id":node,"boot_id":boot});
        let attempt = json!({"attempt_id":binding["attempt_id"],"session_id":binding["session_id"],"generation":1});
        Self {
            binding,
            profile,
            context,
            identity,
            attempt,
        }
    }
    pub(super) async fn install(&self, pool: &sqlx::PgPool, node: Uuid) {
        let Self {
            binding,
            profile,
            identity,
            attempt,
            ..
        } = self;
        let spec = json!({"project_id":binding["project_id"],"harness":"deterministic-agent-v1","resolved_spec_digest":binding["resolved_spec_digest"],"fixture_output":"done","resources":{"slots":1,"cpu_millis":100,"memory_bytes":1024,"disk_bytes":1024},"required_capabilities":["deterministic-agent-v1","scoped-agent-access-v1"],"bindings":{"source":{"repository_id":Uuid::new_v4(),"binding_generation":1,"logical_identity":"test","base":{"algorithm":"sha1","hex":"1111111111111111111111111111111111111111"}},"graph":{"workspace_id":binding["workspace_id"],"profile_id":profile["profile_id"],"profile_revision":profile["revision"],"profile_digest":binding["profile_digest"],"context_digest":binding["context_digest"]}}});
        let offer_id = Uuid::new_v4();
        let attempt_id: Uuid = binding["attempt_id"].as_str().unwrap().parse().unwrap();
        let task_id: Uuid = binding["task_id"].as_str().unwrap().parse().unwrap();
        let session_id: Uuid = binding["session_id"].as_str().unwrap().parse().unwrap();
        let execution = json!({"task_id":task_id,"offer":{"offer_id":offer_id,"identity":identity,"attempt":attempt,"spec":spec,"offer_expires_at_ms":2_000_000,"reservation_ttl_ms":10000},"status":"granted","grant":null,"grant_expires_at_ms":1_100_000,"last_observation":null,"report":null,"disposition":null,"cancellation_id":null,"cancellation_reason":null,"stopped":false});
        sqlx::query("INSERT INTO execution_nodes(node_id,state) VALUES($1,$2)")
            .bind(node)
            .bind(json!({"identity":identity}))
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO execution_attempts(attempt_id,task_id,node_id,offer_id,session_id,state) VALUES($1,$2,$3,$4,$5,$6)").bind(attempt_id).bind(task_id).bind(node).bind(offer_id).bind(session_id).bind(execution).execute(pool).await.unwrap();
    }
    pub(super) async fn provision(&self, admin: &Router, grants: Option<Value>) {
        let attempt_id = &self.binding["attempt_id"];
        let Self {
            profile, context, ..
        } = self;
        assert_eq!(
            send(
                admin,
                "/api/agent-access/provision",
                None,
                Some({
                    let mut body =
                        json!({"attempt_id":attempt_id,"profile":profile,"context":context});
                    if let Some(grants) = grants {
                        body["grants"] = grants;
                    }
                    body
                })
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    pub(super) async fn issue(&self, app: &Router, node_token: &str) -> String {
        let Self {
            identity, attempt, ..
        } = self;
        let (status, issued) = send(
            app,
            "/v2/worker/agent-session",
            Some(node_token),
            Some(json!({"identity":identity,"attempt":attempt,"expected_credential_version":0})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{issued}");
        issued["token"].as_str().unwrap().into()
    }
}
