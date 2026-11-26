//! Optional resource phase for a server candidate with workspace migrations.
use super::*;
use aq_admission::{ObjectFormat, ObjectId, SourceSelection};
use aq_server::{
    build_admin_router,
    profiled_execution::config::ProjectPolicy,
    source_retention::{Runtime, SourceRetentionConfig},
};
use aq_session_protocol::ArtifactRef;
use axum::{Router, body::Body, http::Request};
use http_body_util::BodyExt;
use serde_json::Value;
use std::os::unix::fs::FileTypeExt;
use tower::ServiceExt;

pub(super) struct Workspace {
    sibling: Uuid,
    home_pin: Pin,
    sibling_pin: Pin,
    socket: PathBuf,
    profile: Value,
    context: Value,
}

struct Pin {
    repository: Uuid,
    commit: String,
    retention: Uuid,
}

impl Pin {
    fn read(value: &Value) -> Self {
        Self {
            repository: serde_json::from_value(value["repository_id"].clone()).unwrap(),
            commit: value["commit_oid"].as_str().unwrap().into(),
            retention: serde_json::from_value(
                value["retained_fixture"]["base_retention_handle"].clone(),
            )
            .unwrap(),
        }
    }

    fn project_policy(&self, project_id: Uuid) -> ProjectPolicy {
        ProjectPolicy {
            project_id,
            enabled: true,
            priority: 0,
            source: SourceSelection::Immutable {
                object: ObjectId {
                    format: ObjectFormat::Sha1,
                    hex: self.commit.clone(),
                },
            },
            retention_handle: self.retention,
            placement: None,
            agent_grants: None,
        }
    }
}

impl Workspace {
    pub(super) fn from_environment() -> Option<Self> {
        let path = PathBuf::from(std::env::var_os("AQ_TERMINAL_PILOT_GIT_INVENTORY")?);
        let directory = path.parent().unwrap().canonicalize().unwrap();
        assert_eq!(
            std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let inventory: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let socket = directory.join("git.sock");
        assert_eq!(inventory["socket"], json!(socket));
        assert!(
            std::fs::symlink_metadata(&socket)
                .unwrap()
                .file_type()
                .is_socket()
        );
        let api: Value = serde_json::from_str(include_str!(
            "../../../../../../protocol/agent-access/v1/fixtures/base.json"
        ))
        .unwrap();
        Some(Self {
            sibling: Uuid::new_v4(),
            home_pin: Pin::read(&inventory["aq-server"]),
            sibling_pin: Pin::read(&inventory["aq-dashboard"]),
            socket,
            profile: api["context"]["profile"].clone(),
            context: api["context"]["context"].clone(),
        })
    }

    pub(super) fn setup(&self, f: &mut Fixture, runtime: &std::path::Path) {
        let source = &mut f.profiled.source;
        source.repository_id = self.home_pin.repository;
        source.base.hex.clone_from(&self.home_pin.commit);
        source.logical_identity = "task-workspace-pilot/aq-server".into();
        let graph = &mut f.profiled.graph;
        graph.profile_id = self.profile["profile_id"].as_str().unwrap().into();
        graph.profile_revision = self.profile["revision"].as_str().unwrap().into();
        graph.profile_digest = aq_agent_api::digest(&self.profile).unwrap();
        graph.context_digest = aq_agent_api::digest(&self.context).unwrap();
        let artifact = ArtifactRef {
            id: Text::new(&graph.profile_id).unwrap(),
            version: Text::new(&graph.profile_revision).unwrap(),
            digest: serde_json::from_value(json!(graph.profile_digest)).unwrap(),
        };
        f.profiled.offer.binding.graph_access_profile_digest = artifact.digest.clone();
        f.profiled.offer.manifest.bindings.graph_access_profile = artifact;
        f.state.source_retention = Some(Arc::new(
            Runtime::new(SourceRetentionConfig {
                git_root: self.socket.parent().unwrap().join("state"),
                git_socket: self.socket.clone(),
                staging_root: runtime.join("source-staging"),
                max_staging_bytes: 1 << 30,
                max_bundle_bytes: 1 << 28,
                bulk_concurrency: 1,
                incomplete_ttl_secs: 3600,
                keep_per_task: 2,
                max_age: 3600,
                git_deadline_ms: 30_000,
            })
            .unwrap(),
        ));
    }

    async fn bind(&self, f: &mut Fixture) {
        let pool = f.db.pool().await;
        let home = f.selection.lineage.project_id;
        let (mut target, mut observation): (Value, Value) = sqlx::query_as(
            "SELECT binding,binding_observation FROM project_execution_bindings WHERE project_id=$1",
        )
        .bind(home)
        .fetch_one(&pool)
        .await
        .unwrap();
        target["name"] = json!("Disposable dashboard repository");
        target["project_id"] = json!(self.sibling);
        target["repository_id"] = json!(self.sibling_pin.repository);
        target["ceilings"]["permissions"] = json!([{
            "operation": "workspace.share.request",
            "scope_reference": format!("project:{home}"),
        }]);
        observation["repository_id"] = json!(self.sibling_pin.repository);
        sqlx::query(
            "INSERT INTO project_execution_bindings
             (project_id,revision,binding,validated_at_ms,binding_observation) VALUES($1,1,$2,$3,$4)",
        )
        .bind(self.sibling)
        .bind(target)
        .bind(NOW as i64)
        .bind(observation)
        .execute(&pool)
        .await
        .unwrap();
        let mut config = (*f.state.config).clone();
        Arc::make_mut(config.profiled_execution.as_mut().unwrap())
            .projects
            .extend([
                self.home_pin.project_policy(home),
                self.sibling_pin.project_policy(self.sibling),
            ]);
        f.state.config = Arc::new(config);
    }

    async fn issue(&self, f: &Fixture, worker: &Router) -> String {
        let pool = f.db.pool().await;
        let offer = &f.profiled.offer;
        let attempt = offer.binding.attempt_id;
        let aq_session_protocol::SessionIdentity::Remote {
            session_id,
            assignment_generation,
            ..
        } = &offer.binding.session
        else {
            unreachable!();
        };
        let binding = json!({
            "task_id": f.selection.lineage.task_id,
            "project_id": f.selection.lineage.project_id,
            "workspace_id": f.selection.lineage.workspace_id,
            "session_id": session_id,
            "generation": assignment_generation,
            "attempt_id": attempt,
            "node_id": f.node.node_id,
            "boot_id": f.node.boot_id,
            "resolved_spec_digest": offer.binding.execution_spec_digest,
            "profile_digest": f.profiled.graph.profile_digest,
            "context_digest": f.profiled.graph.context_digest,
        });
        assert!(aq_agent_api::valid("Binding", &binding));
        let grants = aq_agent_grants::GrantSet::new(
            serde_json::from_value(json!([{
                "operation": "task.workspace.request",
                "scope": "task:self",
            }]))
            .unwrap(),
        )
        .unwrap();
        // The synthetic Started fixture has no producer; seed its immutable
        // binding and grants, and use the real worker credential issuance route.
        sqlx::query(
            "INSERT INTO agent_launch_bindings(attempt_id,node_id,binding,profile,context)
             VALUES($1,$2,$3,$4,$5)",
        )
        .bind(attempt)
        .bind(f.node.node_id)
        .bind(&binding)
        .bind(&self.profile)
        .bind(&self.context)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO agent_launch_grants(attempt_id,grants,grants_digest) VALUES($1,$2,$3)",
        )
        .bind(attempt)
        .bind(serde_json::to_value(&grants).unwrap())
        .bind(grants.digest())
        .execute(&pool)
        .await
        .unwrap();
        let (status, response) = post(
            worker,
            "/v2/worker/agent-session",
            Some(&f.credential),
            json!({
                "identity": f.node,
                "attempt": {"attempt_id": attempt, "session_id": session_id, "generation": assignment_generation},
                "expected_credential_version": 0,
            }),
        )
        .await;
        assert_eq!(status, 200, "agent credential issuance failed");
        assert_eq!(response["context"]["binding"], binding);
        response["token"].as_str().unwrap().to_owned()
    }

    pub(super) async fn qualify(&self, f: &mut Fixture) -> Value {
        let home = f.selection.lineage.project_id;
        let task = f.selection.lineage.task_id;
        let attempt = f.profiled.offer.binding.attempt_id;
        let pool = f.db.pool().await;
        let state_format: String =
            sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(state_format, aq_server::STATE_FORMAT_VERSION.to_string());
        let empty: i64 = sqlx::query_scalar(
            "SELECT (SELECT count(*) FROM task_workspace_sources)
                  + (SELECT count(*) FROM task_workspace_services)",
        )
        .fetch_one(&pool)
        .await
        .expect("candidate must include workspace migrations");
        assert_eq!(empty, 0);
        self.bind(f).await;
        let admin = build_admin_router(f.state.clone());
        let worker = build_worker_router(f.state.clone());
        let token = self.issue(f, &worker).await;
        let (status, slots) = post(
            &worker,
            "/v1/worker/slots",
            Some(&f.credential),
            json!({"schema_version":1,"machine_key":"task-workspace-pilot","slots":1}),
        )
        .await;
        assert_eq!(status, 200, "{slots}");
        let control = || {
            post(
                &worker,
                "/v1/worker/control",
                Some(&f.credential),
                json!({"schema_version":1}),
            )
        };
        let (status, before) = control().await;
        assert_eq!(status, 200, "{before}");
        assert_eq!(before["handoff"], json!([]));
        let path = "/v1/agent/workspace/request";
        let batch = json!({"workspace": [
            {"target_project_id": self.sibling, "access": "write"},
            {"service": "postgres"},
        ]});
        let (status, _) = post(&worker, path, None, batch.clone()).await;
        assert!(status == 401 || status == 403);
        let (status, response) = post(&worker, path, Some(&token), batch).await;
        assert_eq!(status, 200, "{response}");
        let state: (String, String) = sqlx::query_as(
            "SELECT state,commit_oid FROM task_workspace_sources
             WHERE home_project_id=$1 AND task_id=$2 AND target_project_id=$3",
        )
        .bind(home)
        .bind(task)
        .bind(self.sibling)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            state,
            ("pending".to_owned(), self.sibling_pin.commit.clone())
        );
        let checkpoint: String =
            sqlx::query_scalar("SELECT reason FROM attempt_handoffs WHERE attempt_id=$1")
                .bind(attempt)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(checkpoint, "workspace_expansion");
        for _ in 0..2 {
            let (status, pending) = control().await;
            assert_eq!(status, 200, "{pending}");
            assert_eq!(pending["handoff"], json!([attempt]));
        }
        let decision = format!(
            "/api/projects/{}/workspace-requests/{home}/{task}/decision",
            self.sibling
        );
        let approval = |oid: &str| json!({"decision":"approved","project_revision":1,"commit_oid":oid,"access":"write"});
        let stale = "0".repeat(40);
        assert_eq!(post(&admin, &decision, None, approval(&stale)).await.0, 409);
        let (status, response) =
            post(&admin, &decision, None, approval(&self.sibling_pin.commit)).await;
        assert_eq!(status, 200, "{response}");
        assert_eq!(response["sources"][0]["state"], "approved");
        let owner = format!("/api/projects/{home}/tasks/{task}/workspace");
        let response = send(&admin, Request::get(&owner).body(Body::empty()).unwrap()).await;
        assert_eq!(response.0, 200, "{:?}", response.1);
        assert_eq!(response.1["services"], json!([{"service":"postgres"}]));
        json!({"target_project_id":self.sibling,"repository_id":self.sibling_pin.repository,
               "home_repository_id":self.home_pin.repository,
               "pinned_commit":self.sibling_pin.commit,"checkpoint_intent":checkpoint,
               "postgres_requested":true,"approval":"approved",
               "store_state_format":state_format})
    }
}

async fn post(app: &Router, path: &str, token: Option<&str>, value: Value) -> (u16, Value) {
    let mut request = Request::post(path)
        .header("content-type", "application/json")
        .header("x-aq-request", "1")
        .header("x-aq-agent-api-version", "1");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    send(app, request.body(Body::from(value.to_string())).unwrap()).await
}

async fn send(app: &Router, request: Request<Body>) -> (u16, Value) {
    let response = app.clone().oneshot(request).await.unwrap();
    (
        response.status().as_u16(),
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
            .unwrap_or(Value::Null),
    )
}
