use super::*;
mod released;
mod settlement;
const OWNER: &str = "owner@localhost";
fn disposition(result: &Value) -> Value {
    result["data"]["disposition"].clone()
}
pub(super) async fn check_epic_retirement_registration(
    f: &common::profiled::Fixture,
    origin: &str,
    token_file: std::path::PathBuf,
    token: &str,
) {
    let mut state = f.state.clone();
    state.agent_access = Some(Arc::new(
        Runtime::new(&AgentAccessConfig {
            graph_url: origin.into(),
            graph_token_file: token_file,
            mutation_actor_id: Some("local-admin".into()),
            credential_ttl_ms: 120000,
        })
        .await
        .unwrap(),
    ));
    let app = aq_server::build_admin_router(state.clone());
    let project = f.selection.lineage.project_id;
    let workspace = f.selection.lineage.workspace_id;
    let at = At {
        app: &app,
        origin,
        workspace,
        token,
    };
    let ids = at.create_tasks(project).await;
    let op = at.register(&ids, &state).await;
    let principal: Uuid =
        sqlx::query_scalar("SELECT owner_id FROM dashboard_operations WHERE operation_id=$1")
            .bind(op)
            .fetch_one(&f.db.pool().await)
            .await
            .unwrap();
    assert_eq!(principal, f.actor);
    let registration = Registration { at: &at, ids: &ids };
    registration.verify_nodes(principal).await;
    registration.check_settlement().await;
    at.check_released_before_decision(project).await;
}
const REPOSITORIES: [&str; 5] = [
    "Acme/service",
    "Acme/dashboard",
    "Acme/worker",
    "Acme/playbooks",
    "Acme/git",
];
fn repositories() -> Value {
    REPOSITORIES
        .iter()
        .map(|repo| (repo.to_string(), json!("feature_a")))
        .collect()
}
struct Retirement {
    project: Uuid,
    epic: Uuid,
    approval: Uuid,
    cleanup: Uuid,
    registration: Uuid,
}
/// One owner command, as the CLI posts it; returns its operation and result.
async fn owner_command(app: &Router, kind: &str, params: Value, email: &str) -> (Uuid, Value) {
    let op = Uuid::new_v4();
    let body = json!({"operation_id":op,"params":params}).to_string();
    let (_, result) = send(app, &format!("/api/v1/commands/{kind}"), Some(&body), email).await;
    (op, result)
}
fn artifact(seed: u8) -> Value {
    json!({"artifact_id":Uuid::from_bytes([seed; 16]),"sha256":format!("{seed:02x}").repeat(32)})
}
fn commit(seed: usize) -> String {
    format!("{:040x}", 0x1000 + seed)
}

/// The Graph workspace and admin router one settlement check runs against.
struct At<'a> {
    app: &'a Router,
    origin: &'a str,
    workspace: Uuid,
    token: &'a str,
}
impl At<'_> {
    async fn node(&self, id: Uuid) -> Value {
        self.graph_node(id, "").await
    }
    async fn graph_node(&self, id: Uuid, path: &str) -> Value {
        let Self {
            origin,
            workspace,
            token,
            ..
        } = *self;
        reqwest::Client::new()
            .get(format!(
                "{origin}/v1/workspaces/{workspace}/nodes/{id}{path}"
            ))
            .bearer_auth(token)
            .header("X-AQ-Graph-API-Version", "1")
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    }
    /// One task edit outside the retirement workflow, as ordinary work and
    /// its settlement make it.
    async fn edit(&self, id: Uuid, patch: Value) {
        let node = self.node(id).await;
        let edited = reqwest::Client::new()
            .post(format!("{}/v1/workspaces/{}/mutations", self.origin, self.workspace))
            .bearer_auth(self.token)
            .header("X-AQ-Graph-API-Version", "1")
            .json(&json!({"idempotency_key":format!("edit-{id}-{}", node["version"]),"reason":"Ordinary work",
                "operations":[{"op":"update_node","id":id,"expected_version":node["version"],"patch":patch}]}))
            .send()
            .await
            .unwrap();
        let status = edited.status();
        let text = edited.text().await.unwrap();
        assert!(status.is_success(), "{id}: {text}");
    }

    async fn create_tasks(&self, project: Uuid) -> Retirement {
        let [epic, approval, cleanup] = std::array::from_fn(|_| Uuid::new_v4());
        let tasks = format!("/api/human/projects/{project}/tasks");
        for id in [epic, approval, cleanup] {
            let (_, result) = send(self.app, &tasks, Some(&request(id, "1")), OWNER).await;
            assert_eq!(result["state"], "confirmed", "{result}");
        }
        let dependency = json!({"project_id":project,"task_id":cleanup,"expected_version":"1","blocker_id":approval});
        let (_, linked) = owner_command(self.app, "task.dependency.add", dependency, OWNER).await;
        assert_eq!(disposition(&linked), "confirmed", "{linked}");
        let registration = Uuid::new_v5(
            &self.workspace,
            format!("aq/epic-retirement/1/{project}/{epic}/1").as_bytes(),
        );
        Retirement {
            project,
            epic,
            approval,
            cleanup,
            registration,
        }
    }
    async fn register(&self, ids: &Retirement, state: &aq_server::AppState) -> Uuid {
        let Retirement {
            project,
            epic,
            approval,
            cleanup,
            ..
        } = *ids;
        let op = Uuid::new_v4();
        let request = json!({"operation_id":op,"params":{"project_id":project,"epic_id":epic,
        "expected_version":"1","generation":1,"repositories":repositories(),
        "ordinary_default":false,"permanent_behavior":"Enable A in ordinary builds",
        "adopt":{"approval":approval,"cleanup":cleanup}}});
        let route = "/api/v1/commands/epic.retirement.register";
        let denied = send(
            self.app,
            route,
            Some(&request.to_string()),
            "viewer@localhost",
        )
        .await;
        assert!(!denied.0.is_success(), "{denied:?}");
        let mut invalid = request.clone();
        invalid["params"]["choice"] = json!("approve");
        let refused = send(self.app, route, Some(&invalid.to_string()), OWNER).await;
        assert!(!refused.0.is_success(), "{refused:?}");

        let (_, registered) = send(self.app, route, Some(&request.to_string()), OWNER).await;
        assert_eq!(
            registered["data"]["disposition"], "confirmed",
            "{registered}"
        );
        let query = format!("/api/v1/operations/{op}");
        let (_, replay) = send(
            &aq_server::build_admin_router(state.clone()),
            &query,
            None,
            OWNER,
        )
        .await;
        assert_eq!(
            registered["data"]["receipt_refs"],
            replay["data"]["receipt_refs"]
        );
        op
    }
}
struct Registration<'a> {
    at: &'a At<'a>,
    ids: &'a Retirement,
}
impl Registration<'_> {
    /// An owner action on the registration at its current version.
    async fn command(&self, kind: &str, params: Value) -> (Uuid, Value) {
        self.settle(kind, params, OWNER).await
    }
    async fn settle(&self, kind: &str, extra: Value, email: &str) -> (Uuid, Value) {
        let ids = self.ids;
        let current = self.at.node(ids.registration).await;
        let mut params = json!({"project_id":ids.project,"registration_id":ids.registration,
            "expected_version":current["version"].to_string()});
        params
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        owner_command(self.at.app, kind, params, email).await
    }
    async fn verify_nodes(&self, principal: Uuid) {
        let Retirement {
            registration,
            approval,
            cleanup,
            ..
        } = *self.ids;
        for (id, kind) in [
            (registration, "knowledge"),
            (approval, "task"),
            (cleanup, "task"),
            (Uuid::new_v5(&registration, b"closure-gate"), "task"),
        ] {
            let graph = self.at.node(id).await;
            assert_eq!(graph["kind"], kind, "{graph}");
            if id == approval {
                assert_eq!(graph["task_state"], "draft");
                assert_eq!(graph["version"], 2);
                assert_eq!(graph["verification"], "unverified");
                assert!(
                    graph["metadata"]
                        .get("aq:epic_feature_retirement")
                        .is_none()
                );
                continue;
            }
            assert_eq!(graph["verification"], "verified", "{graph}");
            let retirement = &graph["metadata"]["aq:epic_feature_retirement"];
            if kind == "task" {
                assert_eq!(graph["task_state"], "draft");
                assert_eq!(retirement["registration"], json!(registration));
            } else {
                assert_eq!(graph["created_by"], "local-admin");
                assert_eq!(
                    retirement["spec"]["owner_authority"]["principal_id"],
                    json!(principal)
                );
                assert_eq!(retirement["spec"]["approval"], json!(approval));
                assert_eq!(retirement["spec"]["cleanup"], json!(cleanup));
                assert!(retirement.get("approved").is_none());
            }
        }
    }
}
impl Registration<'_> {
    async fn record_repositories(
        &self,
        first_commit: usize,
        pull: usize,
        build_seed: u8,
    ) -> (
        serde_json::Map<String, Value>,
        serde_json::Map<String, Value>,
    ) {
        let mut merges = serde_json::Map::new();
        let mut builds = serde_json::Map::new();
        for (i, repo) in REPOSITORIES.iter().enumerate() {
            let sha = commit(first_commit + i);
            let merge = json!({"kind":"merge","approval_revision":1,"evidence":artifact(0x10),
            "repository":repo,"pull_request":format!("https://github.com/{repo}/pull/{pull}"),"sha":sha});
            let (_, merged) = self.command("epic.retirement.evidence", merge).await;
            assert_eq!(disposition(&merged), "confirmed", "{merged}");
            let build = artifact(build_seed + i as u8);
            let ordinary = json!({"kind":"ordinary","approval_revision":1,"evidence":build,
            "repository":repo,"sha":sha,"passed":true,"flag_absent":true});
            let (_, built) = self.command("epic.retirement.evidence", ordinary).await;
            assert_eq!(disposition(&built), "confirmed", "{built}");
            merges.insert(repo.to_string(), json!(sha));
            builds.insert(repo.to_string(), build);
        }
        (merges, builds)
    }
}
