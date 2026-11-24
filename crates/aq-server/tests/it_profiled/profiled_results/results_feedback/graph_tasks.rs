use super::*;

struct GraphPeer {
    child: tokio::process::Child,
    _directory: tempfile::TempDir,
    _db: aq_server::test_support::TestDb,
}

impl GraphPeer {
    async fn start(f: &mut Fixture) -> Self {
        use sqlx::ConnectOptions;
        use std::os::unix::fs::PermissionsExt;
        let graph_db = aq_server::test_support::TestDb::create().await;
        let binary =
            std::env::var("AQ_TEST_GRAPH_BINARY").expect("pinned Graph binary for G6 acceptance");
        use sha2::Digest as _;
        assert_eq!(
            hex::encode(sha2::Sha256::digest(std::fs::read(&binary).unwrap())),
            std::env::var("AQ_TEST_GRAPH_SHA256").unwrap()
        );
        let dir = tempfile::tempdir().unwrap();
        let token = dir.path().join("token");
        std::fs::write(&token, "g6-disposable-graph-fixture-private-token").unwrap();
        std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = socket.local_addr().unwrap();
        drop(socket);
        let mut child = tokio::process::Command::new(binary)
            .args([
                "serve",
                "--bind",
                &addr.to_string(),
                "--bootstrap-workspace",
                &f.selection.lineage.workspace_id.to_string(),
                "--bootstrap-token-file",
                token.to_str().unwrap(),
                "--pool-size",
                "2",
            ])
            .env_clear()
            .env("TOKIO_WORKER_THREADS", "2")
            .env("DATABASE_URL", graph_db.options().to_url_lossy().as_str())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let origin = format!("http://{addr}");
        Self::wait_ready(&mut child, &origin).await;
        f.state.agent_access = Some(Arc::new(
            aq_server::agent_access::Runtime::new(&aq_server::agent_access::AgentAccessConfig {
                graph_url: origin,
                graph_token_file: token,
                mutation_actor_id: Some("local-admin".into()),
                credential_ttl_ms: 120000,
            })
            .await
            .unwrap(),
        ));
        Self {
            child,
            _directory: dir,
            _db: graph_db,
        }
    }

    async fn wait_ready(child: &mut tokio::process::Child, origin: &str) {
        let client = reqwest::Client::new();
        let mut ready = false;
        for _ in 0..200 {
            if client
                .get(format!("{origin}/health/ready"))
                .send()
                .await
                .is_ok_and(|r| r.status().is_success())
            {
                ready = true;
                break;
            }
            if let Some(status) = child.try_wait().unwrap() {
                use tokio::io::AsyncReadExt;
                let mut diagnostics = String::new();
                child
                    .stderr
                    .take()
                    .unwrap()
                    .read_to_string(&mut diagnostics)
                    .await
                    .unwrap();
                panic!("Graph exited {status}: {diagnostics}");
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(ready);
    }
}

#[tokio::test]
#[ignore = "requires explicit pinned AQ_TEST_GRAPH_BINARY and AQ_TEST_GRAPH_SHA256"]
async fn feedback_task_links_result_revision() {
    let (mut f, _, report) = prepare(true, true).await;
    let receipt = results::record(&f.state, &context(&f).await, &report)
        .await
        .unwrap();
    let mut peer = GraphPeer::start(&mut f).await;
    let task = FeedbackTask::new(&f, &report, receipt.report_digest.as_str()).await;
    let pool = f.db.pool().await;
    task.recover_link(&pool).await;
    task.check_created_link(&pool).await;
    task.create_child().await;
    task.recover_creation(&pool).await;
    task.link_existing(&pool).await;
    task.check_revisions().await;
    peer.child.kill().await.unwrap();
}

struct FeedbackTask<'a> {
    f: &'a Fixture,
    report: &'a ResultReport,
    revision: &'a str,
    app: Router,
    root: String,
    comment: Uuid,
    id: Uuid,
    draft: Value,
}

impl<'a> FeedbackTask<'a> {
    async fn new(f: &'a Fixture, report: &'a ResultReport, revision: &'a str) -> Self {
        let app = aq_server::build_admin_router(f.state.clone());
        let root = format!(
            "/api/v1/results/{}/revisions/{}",
            report.report_id, revision
        );
        let comment = Uuid::new_v4();
        let (_,created)=send(&app,"POST",&format!("{root}/comments"),json!({"operation_id":comment,"expected_version":"1","body":"Fix caption","anchor":{"kind":"result"},"follow":true})).await;
        assert_eq!(created["data"]["disposition"], "confirmed", "{created}");
        let id = Uuid::new_v4();
        let draft = json!({"operation_id":id,"expected_version":"1","result_id":report.report_id,"revision":revision,"thread_id":comment,"comment_id":comment,"comment_revision":"1","title":"Correct caption","description":"Use the reviewed caption","acceptance_criteria":"Caption matches feedback","project_id":f.selection.lineage.project_id,"expected_project_revision":"1","parent_id":null,"artifacts":[]});
        Self {
            f,
            report,
            revision,
            app,
            root,
            comment,
            id,
            draft,
        }
    }

    async fn recover_link(&self, pool: &sqlx::PgPool) {
        let app = &self.app;
        let id = self.id;
        let draft = &self.draft;
        sqlx::query("CREATE FUNCTION g6_test_link_loss() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN RAISE EXCEPTION ''injected lost link''; END'").execute(pool).await.unwrap();
        sqlx::query("CREATE TRIGGER g6_test_loss BEFORE INSERT ON result_feedback_links FOR EACH ROW EXECUTE FUNCTION g6_test_link_loss()").execute(pool).await.unwrap();
        let (code, uncertain) = send(app, "POST", "/api/v1/feedback-tasks", draft.clone()).await;
        assert_eq!(code, StatusCode::ACCEPTED, "{uncertain}");
        assert_eq!(uncertain["data"]["disposition"], "unknown");
        let (_, status) = send(
            app,
            "GET",
            &format!("/api/v1/feedback-tasks/{id}"),
            Value::Null,
        )
        .await;
        assert_eq!(status["data"]["task_id"], id.to_string());
        assert_eq!(status["data"]["link_confirmed"], false);
        assert_eq!(status["data"]["creation_disposition"], "confirmed");
        sqlx::query("DROP TRIGGER g6_test_loss ON result_feedback_links")
            .execute(pool)
            .await
            .unwrap();
        let (_, recovered) =
            send(app, "GET", &format!("/api/v1/operations/{id}"), Value::Null).await;
        assert_eq!(recovered["data"]["disposition"], "confirmed", "{recovered}");
        let (code, linked) = send(app, "POST", "/api/v1/feedback-tasks", draft.clone()).await;
        assert_eq!(code, StatusCode::OK, "{linked}");
        assert_eq!(linked["data"]["disposition"], "confirmed", "{linked}");
        assert!(
            linked["data"]["affected_entities"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["type"] == "task" && e["id"] == id.to_string())
        );
        assert_eq!(
            send(app, "POST", "/api/v1/feedback-tasks", draft.clone())
                .await
                .1["data"],
            linked["data"]
        );
    }

    async fn check_pin(&self, pool: &sqlx::PgPool, operation: Uuid) {
        let pin: (Uuid, String, Uuid) = sqlx::query_as(
            "SELECT report_id,report_revision,task_id FROM result_feedback_links WHERE operation_id=$1",
        )
        .bind(operation)
        .fetch_one(pool)
        .await
        .unwrap();
        assert_eq!(pin, (self.report.report_id, self.revision.into(), self.id));
    }

    async fn check_created_link(&self, pool: &sqlx::PgPool) {
        let app = &self.app;
        let root = &self.root;
        let comment = self.comment;
        let id = self.id;
        let revision = self.revision;

        self.check_pin(pool, id).await;
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM human_graph_operations WHERE operation_id=$1")
                .bind(id)
                .fetch_one(pool)
                .await
                .unwrap();
        assert_eq!(count, 1);
        let (_, comments) = send(app, "GET", &format!("{root}/comments"), Value::Null).await;
        assert_eq!(comments["data"]["items"][0]["thread_state"], "open");
        let links = comments["data"]["items"][0]["feedback_links"]
            .as_array()
            .unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0]["operation_id"], id.to_string());
        assert_eq!(links[0]["task_id"], id.to_string());
        assert_eq!(links[0]["origin"]["comment_id"], comment.to_string());
        assert_eq!(links[0]["origin"]["comment_revision"], "1");
        assert_eq!(links[0]["origin"]["revision"], revision);
        assert!(comments["data"]["items"][0]["resolution"].is_null());
    }

    async fn create_child(&self) {
        let app = &self.app;
        let id = self.id;
        let f = self.f;
        let draft = &self.draft;
        let child_id = Uuid::new_v4();
        let mut child_draft = draft.clone();
        child_draft["operation_id"] = json!(child_id);
        child_draft["parent_id"] = json!(id);
        let (code, created_child) = send(app, "POST", "/api/v1/feedback-tasks", child_draft).await;
        assert_eq!(code, StatusCode::OK, "{created_child}");
        assert_eq!(
            created_child["data"]["disposition"], "confirmed",
            "{created_child}"
        );
        let (code, graph) = send(
            app,
            "GET",
            &format!(
                "/api/human/projects/{}/tasks/{child_id}/graph",
                f.selection.lineage.project_id
            ),
            Value::Null,
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{graph}");
        assert!(graph.to_string().contains("parent-child"), "{graph}");
    }

    async fn recover_creation(&self, pool: &sqlx::PgPool) {
        let app = &self.app;
        let draft = &self.draft;
        let orphan_id = Uuid::new_v4();
        let mut original = draft.clone();
        original["operation_id"] = json!(orphan_id);
        sqlx::query("CREATE TRIGGER g6_native_loss BEFORE INSERT ON human_graph_operations FOR EACH ROW EXECUTE FUNCTION g6_test_link_loss()").execute(pool).await.unwrap();
        let (code, lost) = send(app, "POST", "/api/v1/feedback-tasks", original.clone()).await;
        assert_eq!(code, StatusCode::ACCEPTED, "{lost}");
        sqlx::query("DROP TRIGGER g6_native_loss ON human_graph_operations")
            .execute(pool)
            .await
            .unwrap();
        let (code, recovery) = send(
            app,
            "POST",
            &format!("/api/v1/feedback-tasks/{orphan_id}/recover"),
            json!({}),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{recovery}");
        assert_eq!(recovery["data"]["disposition"], "confirmed", "{recovery}");
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM human_graph_operations WHERE operation_id=$1")
                .bind(orphan_id)
                .fetch_one(pool)
                .await
                .unwrap();
        assert_eq!(count, 1);
    }

    async fn link_existing(&self, pool: &sqlx::PgPool) {
        let app = &self.app;
        let root = &self.root;
        let comment = self.comment;
        let id = self.id;
        let f = self.f;
        let report = self.report;
        let revision = self.revision;
        let link_id = Uuid::new_v4();
        let existing_link = json!({"operation_id":link_id,"expected_version":"1","result_id":report.report_id,"revision":revision,"thread_id":comment,"comment_id":comment,"comment_revision":"1","task_id":id,"project_id":f.selection.lineage.project_id,"artifacts":[]});
        let (code, attached) =
            send(app, "POST", "/api/v1/feedback-links", existing_link.clone()).await;
        assert_eq!(code, StatusCode::OK, "{attached}");
        assert_eq!(attached["data"]["disposition"], "confirmed", "{attached}");
        assert_eq!(
            send(app, "POST", "/api/v1/feedback-links", existing_link.clone())
                .await
                .1["data"],
            attached["data"]
        );
        let mut foreign = existing_link.clone();
        foreign["operation_id"] = json!(Uuid::new_v4());
        foreign["project_id"] = json!(Uuid::new_v4());
        assert_eq!(
            send(app, "POST", "/api/v1/feedback-links", foreign).await.0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        self.check_pin(pool, link_id).await;
        let (_, comments) = send(app, "GET", &format!("{root}/comments"), Value::Null).await;
        assert_eq!(comments["data"]["items"][0]["thread_state"], "open");
        let links = comments["data"]["items"][0]["feedback_links"]
            .as_array()
            .unwrap();
        // Creation, recovered creation and existing-task linking retain separate IDs.
        assert_eq!(links.len(), 4);
        let existing = links
            .iter()
            .find(|link| link["operation_id"] == link_id.to_string())
            .unwrap();
        assert_eq!(existing["task_id"], id.to_string());
        assert_eq!(existing["origin"]["comment_revision"], "1");
        assert_eq!(existing["origin"]["artifacts"], json!([]));
        assert!(comments["data"]["items"][0]["resolution"].is_null());
        assert_eq!(
            comments["data"]["follow_up_submission"],
            json!({"state":"unsupported"})
        );
        let (_, detail) = send(app, "GET", root, Value::Null).await;
        assert_eq!(detail["data"]["revision"]["addressed_feedback"], json!([]));
    }

    async fn check_revisions(&self) {
        let app = &self.app;
        let root = &self.root;
        let comment = self.comment;
        let f = self.f;
        let report = self.report;
        let (_, corrected) = send(
            app, "POST", &format!("/api/v1/comments/{comment}/revisions"),
            json!({"operation_id":Uuid::new_v4(),"expected_version":"1","body":"Updated wording after task creation"}),
        ).await;
        assert_eq!(corrected["data"]["disposition"], "confirmed");
        let (_, detail) = send(app, "GET", root, Value::Null).await;
        let reply = Uuid::new_v4();
        let (_, replied) = send(
            app,
            "POST",
            &format!("{root}/comments"),
            json!({
                "operation_id":reply,"expected_version":detail["data"]["result"]["version"],"thread_id":comment,
                "body":"Another comment in this thread","anchor":{"kind":"result"}
            }),
        )
        .await;
        assert_eq!(replied["data"]["disposition"], "confirmed");
        let reloaded = aq_server::build_admin_router(f.state.clone());
        let (_, comments) = send(&reloaded, "GET", &format!("{root}/comments"), Value::Null).await;
        assert_eq!(comments["data"]["items"][0]["revision"], "2");
        for link in comments["data"]["items"][0]["feedback_links"]
            .as_array()
            .unwrap()
        {
            assert_eq!(link["origin"]["comment_revision"], "1");
        }
        assert_eq!(comments["data"]["items"][1]["feedback_links"], json!([]));
        serde_json::from_value::<aq_server::results_feedback::contract::CommentsPage>(
            comments["data"].clone(),
        )
        .unwrap();
        let mut correction = report.clone();
        correction.report_id = Uuid::new_v4();
        correction.operation_id = Uuid::new_v4();
        correction.supersedes = Some(report.report_id);
        let next = results::record(&f.state, &context(f).await, &correction)
            .await
            .unwrap();
        let (_, fresh) = send(
            &reloaded,
            "GET",
            &format!(
                "/api/v1/results/{}/revisions/{}/comments",
                report.report_id,
                next.report_digest.as_str()
            ),
            Value::Null,
        )
        .await;
        assert_eq!(fresh["data"]["items"], json!([]));
    }
}
