//! Boundary tests: real CLI translation and HTTP transport, pinned owner fixtures.
use aq_human_http::{Authentication, Client, Limits};
use axum::{
    Router,
    body::to_bytes,
    extract::{Request, State},
    http::StatusCode,
    response::IntoResponse,
    routing::any,
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::mpsc;

#[path = "cli/channel.rs"]
mod channel;
#[path = "cli/result.rs"]
mod result;

struct Seen {
    method: String,
    path: String,
    headers: axum::http::HeaderMap,
    body: Value,
}
struct Server {
    client: Client,
    seen: mpsc::UnboundedReceiver<Seen>,
    task: tokio::task::JoinHandle<()>,
}
impl Server {
    async fn next(&mut self) -> Seen {
        tokio::time::timeout(Duration::from_secs(1), self.seen.recv())
            .await
            .expect("CLI did not send expected request")
            .unwrap()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
#[derive(Clone)]
struct Responses {
    values: Arc<Mutex<VecDeque<(StatusCode, Value)>>>,
    seen: mpsc::UnboundedSender<Seen>,
}
async fn handle(State(state): State<Responses>, request: Request) -> impl IntoResponse {
    let (parts, body) = request.into_parts();
    let bytes = to_bytes(body, 65537).await.unwrap();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    let request_id = body["operation_id"].clone();
    state
        .seen
        .send(Seen {
            method: parts.method.to_string(),
            path: parts.uri.to_string(),
            headers: parts.headers,
            body,
        })
        .unwrap();
    let mut response = state.values.lock().unwrap().pop_front().unwrap_or((
        StatusCode::INTERNAL_SERVER_ERROR,
        json!({"private":"must not leak"}),
    ));
    if response.1["data"]["operation_id"] == "echo-request-id" {
        response.1["data"]["operation_id"] = request_id;
    }
    (response.0, axum::Json(response.1))
}
async fn server(responses: Vec<(StatusCode, Value)>) -> Server {
    let (tx, rx) = mpsc::unbounded_channel();
    let app = Router::new().fallback(any(handle)).with_state(Responses {
        values: Arc::new(Mutex::new(responses.into())),
        seen: tx,
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = Client::new(
        &origin,
        Authentication::LoopbackDevelopmentEmail("owner@example.test".into()),
        Limits::default(),
    )
    .unwrap();
    Server {
        client,
        seen: rx,
        task,
    }
}
fn fixture(name: &str) -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../protocol/dashboard-actions/v1/fixtures")
        .join(name);
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}
fn confirmed(id: &str) -> Value {
    let mut value = fixture("operation-confirmed.json");
    value["data"]["operation_id"] = json!(id);
    value
}

// Catches changed action kinds, mismatched nested IDs and use of legacy POST or lookup routes.
#[tokio::test]
async fn cli_and_dashboard_actions_share_operation_ids() {
    let temp = tempfile::tempdir().unwrap();
    let decision = fixture("request-decision-resolve.json");
    let path = temp.path().join("decision.json");
    std::fs::write(&path, serde_json::to_vec(&decision["payload"]).unwrap()).unwrap();
    let cases = vec![
        (
            "request-task-create.json",
            vec![
                "tasks",
                "create",
                "--project",
                "00000000-0000-4000-8000-000000000010",
                "--expected-project-revision",
                "1",
                "--title",
                "Write launch notes",
                "--body",
                "Document the accepted behavior.",
                "--acceptance-criteria",
                "Notes reviewed",
                "--label",
                "docs",
            ],
        ),
        (
            "request-decision-resolve.json",
            vec![
                "decisions",
                "respond",
                "00000000-0000-4000-8000-000000000020",
                "--request",
                path.to_str().unwrap(),
            ],
        ),
        (
            "request-supervisor-message-send.json",
            vec!["messages", "send", "Please inspect the pending decision."],
        ),
    ];
    for (file, args) in cases {
        // The reply intentionally has the wrong ID: the client must refuse it,
        // while preserving the actual minted ID sent in the request.
        let mut responses = vec![];
        if file.contains("supervisor") {
            let mut discovery: Value = serde_json::from_str(include_str!(
                "../../../protocol/human-supervisor-conversation/v1/discovery.example.json"
            ))
            .unwrap();
            discovery["conversation"]["id"] = json!("00000000-0000-4000-8000-000000000050");
            responses.push((StatusCode::OK, discovery));
        }
        responses.push((
            StatusCode::OK,
            confirmed("00000000-0000-4000-8000-000000000099"),
        ));
        let mut srv = server(responses).await;
        let error = aqv2::execute(srv.client.clone(), &args).await.unwrap_err();
        let mut sent = srv.next().await;
        if sent.method == "GET" {
            assert_eq!(sent.path, "/api/human/conversations/global");
            sent = srv.next().await;
        }
        assert_eq!(sent.method, "POST");
        assert_eq!(sent.path, "/api/v1/actions");
        assert_eq!(sent.headers["x-aq-request"], "1");
        assert!(sent.headers.contains_key("origin"));
        let id = sent.body["operation_id"].as_str().unwrap();
        assert!(!uuid::Uuid::parse_str(id).unwrap().is_nil());
        assert_eq!(sent.body["payload"]["operation_id"], id);
        assert_eq!(error["operation_id"], id);
        let mut expected = fixture(file);
        expected["operation_id"] = json!(id);
        expected["payload"]["operation_id"] = json!(id);
        if file.contains("supervisor") {
            let mid = sent.body["payload"]["message_id"].as_str().unwrap();
            assert_ne!(mid, id);
            assert!(!uuid::Uuid::parse_str(mid).unwrap().is_nil());
            expected["payload"]["message_id"] = json!(mid);
        }
        assert_eq!(sent.body, expected);
        let retained = confirmed(id);
        let mut recovery = server(vec![(StatusCode::OK, retained.clone())]).await;
        let result = aqv2::execute(recovery.client.clone(), &["operations", "show", id])
            .await
            .unwrap();
        assert_eq!(result, retained);
        let read = recovery.next().await;
        assert_eq!(read.method, "GET");
        assert_eq!(read.path, format!("/api/v1/operations/{id}"));
        assert!(srv.seen.try_recv().is_err());
    }
}

#[tokio::test]
async fn reads_use_existing_owner_routes_and_preserve_pages() {
    let project = "00000000-0000-4000-8000-000000000010";
    let id = "00000000-0000-4000-8000-000000000020";
    let cases = vec![
        (vec!["tasks", "list"], "/api/human/overview/tasks".into()),
        (
            vec!["tasks", "list", "--limit", "3", "--cursor", project],
            format!("/api/human/overview/tasks?task_limit=3&after={project}"),
        ),
        (
            vec![
                "tasks",
                "list",
                "--project",
                project,
                "--limit",
                "3",
                "--cursor",
                "a/b?c",
            ],
            format!("/api/human/projects/{project}/tasks?limit=3&cursor=a%2Fb%3Fc"),
        ),
        (
            vec!["tasks", "show", id, "--project", project],
            format!("/api/human/projects/{project}/tasks/{id}"),
        ),
        (vec!["decisions", "list"], "/api/reviews/decisions".into()),
        (
            vec!["decisions", "show", id],
            format!("/api/reviews/decisions/{id}"),
        ),
        (vec!["workers", "list"], "/api/workers".into()),
        (vec!["projects", "list"], "/api/v1/projects".into()),
        (
            vec!["projects", "show", project],
            format!("/api/projects/{project}"),
        ),
    ];
    for (args, path) in cases {
        let page = json!({"data":{"items":[{"id":id}]},"next_cursor":"next","has_more":true});
        let mut srv = server(vec![(StatusCode::OK, page.clone())]).await;
        assert_eq!(
            aqv2::execute(srv.client.clone(), &args).await.unwrap(),
            page
        );
        let read = srv.next().await;
        assert_eq!(read.method, "GET");
        assert_eq!(read.path, path);
    }
}
#[tokio::test]
async fn unsupported_mutations_never_call_legacy_routes() {
    for args in [vec!["workers", "enroll"], vec!["projects", "onboard"]] {
        let mut srv = server(vec![]).await;
        let error = aqv2::execute(srv.client.clone(), &args).await.unwrap_err();
        assert_eq!(error["code"], "unsupported");
        assert!(srv.seen.try_recv().is_err());
    }
}
// `tasks cancel|retry` and `workers revoke` are registered action kinds
// (task.cancel, task.retry, worker.revoke) and go through the action union.
#[tokio::test]
async fn task_cancel_and_retry_send_the_registered_action() {
    let (id, project) = (
        "00000000-0000-4000-8000-000000000020",
        "00000000-0000-4000-8000-000000000010",
    );
    for (verb, kind) in [("cancel", "task.cancel"), ("retry", "task.retry")] {
        // With --revision: one POST, no read.
        let mut srv = server(vec![]).await;
        let args = ["tasks", verb, id, "--project", project, "--revision", "7"];
        let _ = aqv2::execute(srv.client.clone(), &args).await;
        let post = srv.next().await;
        assert_eq!(
            (post.method.as_str(), post.path.as_str()),
            ("POST", "/api/v1/actions")
        );
        assert_eq!(post.body["kind"], kind);
        assert_eq!(
            post.body["subject"],
            json!({"type":"task","id":id,"revision":"7"})
        );
        assert_eq!(post.body["expected_versions"], json!({"task":"7"}));
        assert_eq!(post.body["payload"]["project_id"], project);
        assert_eq!(post.body["payload"]["expected_version"], "7");
        assert_eq!(
            post.body["payload"]["operation_id"],
            post.body["operation_id"]
        );
        assert!(srv.seen.try_recv().is_err());
        // Without it: the task is read first and its version is sent.
        let task =
            json!({"contract":"human-graph-1","data":{"id":id,"kind":"task","version":"12"}});
        let mut srv = server(vec![(StatusCode::OK, task)]).await;
        let _ = aqv2::execute(
            srv.client.clone(),
            &["tasks", verb, id, "--project", project],
        )
        .await;
        let read = srv.next().await;
        assert_eq!(read.method, "GET");
        assert_eq!(
            read.path,
            format!("/api/human/projects/{project}/tasks/{id}")
        );
        let post = srv.next().await;
        assert_eq!(post.body["kind"], kind);
        assert_eq!(post.body["subject"]["revision"], "12");
        assert_eq!(post.body["payload"]["expected_version"], "12");
    }
    // A read without a usable version sends nothing.
    let mut srv = server(vec![(StatusCode::OK, json!({"data":{"id":id}}))]).await;
    let error = aqv2::execute(
        srv.client.clone(),
        &["tasks", "cancel", id, "--project", project],
    )
    .await
    .unwrap_err();
    assert_eq!(error["code"], "invalid_response");
    assert_eq!(srv.next().await.method, "GET");
    assert!(srv.seen.try_recv().is_err());
}
#[tokio::test]
async fn worker_revoke_sends_the_registered_action() {
    let node = "00000000-0000-4000-8000-000000000020";
    for (arg, machine) in [
        ("desktop", "desktop".to_string()),
        (node, format!("node:{node}")),
    ] {
        let mut srv = server(vec![]).await;
        let _ = aqv2::execute(srv.client.clone(), &["workers", "revoke", arg]).await;
        let post = srv.next().await;
        assert_eq!(
            (post.method.as_str(), post.path.as_str()),
            ("POST", "/api/v1/actions")
        );
        let mut expected = fixture("request-worker-revoke.json");
        expected["operation_id"] = post.body["operation_id"].clone();
        expected["payload"]["operation_id"] = post.body["operation_id"].clone();
        expected["subject"]["id"] = json!(machine);
        expected["payload"]["machine"] = json!(machine);
        if let Some(o) = expected.as_object_mut() {
            o.remove("reason");
        }
        assert_eq!(post.body, expected);
    }
    // A confirmed receipt for a machine subject is accepted.
    let mut srv = server(vec![(StatusCode::OK, confirmed("echo-request-id"))]).await;
    let done = aqv2::execute(srv.client.clone(), &["workers", "revoke", "desktop"])
        .await
        .unwrap();
    assert_eq!(done["data"]["disposition"], "confirmed");
    assert_eq!(srv.next().await.method, "POST");
    // 32 bare hex characters are a valid key, not a node ID.
    let key = "0123456789abcdef0123456789abcdef";
    let mut srv = server(vec![]).await;
    let _ = aqv2::execute(srv.client.clone(), &["workers", "revoke", key]).await;
    assert_eq!(srv.next().await.body["payload"]["machine"], key);
    let mut srv = server(vec![]).await;
    for bad in [
        vec![
            "tasks",
            "cancel",
            key,
            "--project",
            key,
            "--revision",
            "9999999999999999999",
        ],
        vec!["tasks", "cancel", key, "--project", key, "--revision", "07"],
    ] {
        let error = aqv2::execute(srv.client.clone(), &bad).await.unwrap_err();
        assert_eq!(error["code"], "invalid_arguments");
    }
    let error = aqv2::execute(srv.client.clone(), &["workers", "revoke", "Not A Key"])
        .await
        .unwrap_err();
    assert_eq!(error["code"], "invalid_arguments");
    assert!(srv.seen.try_recv().is_err());
}
#[tokio::test]
async fn server_refusals_and_unavailable_reads_do_not_leak_error_bodies() {
    for (status, code) in [
        (StatusCode::FORBIDDEN, "http_error"),
        (StatusCode::NOT_FOUND, "unsupported"),
        (StatusCode::UPGRADE_REQUIRED, "unsupported"),
    ] {
        let mut srv = server(vec![(status, json!({"private":"sensitive server detail"}))]).await;
        let error = aqv2::execute(srv.client.clone(), &["projects", "list"])
            .await
            .unwrap_err();
        assert_eq!(error["code"], code);
        assert!(!error.to_string().contains("sensitive"));
        assert_eq!(srv.next().await.method, "GET");
        assert!(srv.seen.try_recv().is_err());
    }
}
#[test]
fn binary_json_errors_and_help_are_usable() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_aqv2"))
        .env_remove("AQ_CHANNEL")
        .args(["--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    for group in [
        "tasks",
        "decisions",
        "messages",
        "chat",
        "workers",
        "projects",
        "operations",
    ] {
        assert!(help.contains(group), "{group}");
    }
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_aqv2"))
        .env_remove("AQ_CHANNEL")
        .args([
            "--origin",
            "https://aq.example",
            "--cf-access-token-file",
            "/nonexistent",
            "--json",
            "workers",
            "enroll",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(error["schema_version"], 1);
    assert_eq!(error["error"]["code"], "unsupported");
    assert!(output.stderr.is_empty());
}

// A queued/unknown/conflicting action is not a successful command, and must not
// cause another POST, even when the body arrived with a successful HTTP status.
#[tokio::test]
async fn mutation_dispositions_and_http_failures_preserve_the_original_id() {
    for name in [
        "operation-pending.json",
        "operation-unknown.json",
        "operation-conflict.json",
        "operation-rejected.json",
    ] {
        let mut value = fixture(name);
        value["data"]["operation_id"] = json!("echo-request-id");
        let status = if name.contains("pending") || name.contains("unknown") {
            StatusCode::ACCEPTED
        } else {
            StatusCode::OK
        };
        let mut srv = server(vec![(status, value)]).await;
        let err = aqv2::execute(
            srv.client.clone(),
            &[
                "tasks",
                "create",
                "--project",
                "00000000-0000-4000-8000-000000000010",
                "--expected-project-revision",
                "1",
                "--title",
                "Task",
            ],
        )
        .await
        .unwrap_err();
        assert_eq!(err["code"], "operation_unconfirmed");
        let sent = srv.next().await;
        assert_eq!(err["operation_id"], sent.body["operation_id"]);
        assert_eq!(
            err["operation"]["data"]["disposition"],
            fixture(name)["data"]["disposition"]
        );
        assert!(srv.seen.try_recv().is_err());
    }
    for status in [
        StatusCode::FORBIDDEN,
        StatusCode::CONFLICT,
        StatusCode::UNPROCESSABLE_ENTITY,
        StatusCode::SERVICE_UNAVAILABLE,
    ] {
        let mut srv = server(vec![(status, json!({"message":"private detailed body"}))]).await;
        let err = aqv2::execute(
            srv.client.clone(),
            &[
                "tasks",
                "create",
                "--project",
                "00000000-0000-4000-8000-000000000010",
                "--expected-project-revision",
                "1",
                "--title",
                "Task",
            ],
        )
        .await
        .unwrap_err();
        let sent = srv.next().await;
        assert_eq!(err["operation_id"], sent.body["operation_id"]);
        assert!(!err.to_string().contains("private"));
        assert!(srv.seen.try_recv().is_err());
    }
}
#[tokio::test]
async fn transport_loss_and_malformed_receipts_never_retry_a_mutation() {
    use tokio::io::AsyncReadExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let client = Client::new(
        &origin,
        Authentication::LoopbackDevelopmentEmail("owner@example.test".into()),
        Limits {
            timeout: Duration::from_millis(100),
            ..Limits::default()
        },
    )
    .unwrap();
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut bytes = vec![0; 65536];
        let n = stream.read(&mut bytes).await.unwrap();
        drop(stream); // Lost response, with no evidence whether admission happened.
        assert!(
            tokio::time::timeout(Duration::from_millis(150), listener.accept())
                .await
                .is_err()
        );
        String::from_utf8(bytes[..n].to_vec()).unwrap()
    });
    let err = aqv2::execute(
        client,
        &[
            "tasks",
            "create",
            "--project",
            "00000000-0000-4000-8000-000000000010",
            "--expected-project-revision",
            "1",
            "--title",
            "Task",
        ],
    )
    .await
    .unwrap_err();
    assert_eq!(err["code"], "transport_error");
    let request = task.await.unwrap();
    assert!(request.contains(err["operation_id"].as_str().unwrap()));
    assert!(
        err["recovery"]
            .as_str()
            .unwrap()
            .contains("operations show")
    );

    let mut srv=server(vec![(StatusCode::OK,json!({"api_version":"dashboard-actions-1","data":{"operation_id":"echo-request-id","disposition":"confirmed"}}))]).await;
    let err = aqv2::execute(
        srv.client.clone(),
        &[
            "tasks",
            "create",
            "--project",
            "00000000-0000-4000-8000-000000000010",
            "--expected-project-revision",
            "1",
            "--title",
            "Task",
        ],
    )
    .await
    .unwrap_err();
    assert_eq!(err["code"], "invalid_response");
    let sent = srv.next().await;
    assert_eq!(err["operation_id"], sent.body["operation_id"]);
    assert!(srv.seen.try_recv().is_err());
}
#[tokio::test]
async fn chat_and_inbox_read_history_but_live_chat_refuses_the_unavailable_contract() {
    let discovery: Value = serde_json::from_str(include_str!(
        "../../../protocol/human-supervisor-conversation/v1/discovery.example.json"
    ))
    .unwrap();
    let history: Value = serde_json::from_str(include_str!(
        "../../../protocol/human-supervisor-conversation/v1/history.example.json"
    ))
    .unwrap();
    for args in [
        vec!["messages", "inbox", "--after", "0", "--limit", "3"],
        vec!["chat", "--after", "0", "--limit", "3"],
    ] {
        let mut srv = server(vec![
            (StatusCode::OK, discovery.clone()),
            (StatusCode::OK, history.clone()),
        ])
        .await;
        assert_eq!(
            aqv2::execute(srv.client.clone(), &args).await.unwrap(),
            history
        );
        assert_eq!(srv.next().await.path, "/api/human/conversations/global");
        let read = srv.next().await;
        assert_eq!(
            read.path,
            "/api/human/conversations/00000000-0000-0000-0000-000000000001/messages?limit=3&after=0"
        );
        assert_eq!(read.method, "GET");
    }
    let mut srv = server(vec![(StatusCode::OK, discovery)]).await;
    let err = aqv2::execute(
        srv.client.clone(),
        &["chat", "--once", "hello", "--timeout", "1"],
    )
    .await
    .unwrap_err();
    assert_eq!(err["code"], "unsupported");
    assert!(err["message"].as_str().unwrap().contains("F1b"));
    assert_eq!(srv.next().await.method, "GET");
    assert!(srv.seen.try_recv().is_err());
}
#[tokio::test]
async fn operation_lookup_preserves_unknown_and_never_treats_not_found_as_non_admission() {
    let id = "00000000-0000-4000-8000-000000000001";
    let mut value = fixture("operation-unknown.json");
    value["data"]["operation_id"] = json!(id);
    let mut srv = server(vec![
        (StatusCode::OK, value.clone()),
        (StatusCode::NOT_FOUND, json!({"code":"not_found"})),
    ])
    .await;
    assert_eq!(
        aqv2::execute(srv.client.clone(), &["operations", "show", id])
            .await
            .unwrap(),
        value
    );
    let error = aqv2::execute(srv.client.clone(), &["operations", "show", id])
        .await
        .unwrap_err();
    assert_eq!(error["code"], "not_found");
    assert_eq!(srv.next().await.method, "GET");
    assert_eq!(srv.next().await.method, "GET");
    assert!(srv.seen.try_recv().is_err());
}

#[tokio::test]
async fn confirmed_submission_returns_the_matching_operation_and_does_not_poll_or_resubmit() {
    let mut srv = server(vec![(StatusCode::OK, confirmed("echo-request-id"))]).await;
    let value = aqv2::execute(
        srv.client.clone(),
        &[
            "tasks",
            "create",
            "--project",
            "00000000-0000-4000-8000-000000000010",
            "--expected-project-revision",
            "1",
            "--title",
            "Task",
        ],
    )
    .await
    .unwrap();
    let sent = srv.next().await;
    assert_eq!(value["data"]["operation_id"], sent.body["operation_id"]);
    assert_eq!(value["data"]["disposition"], "confirmed");
    assert!(srv.seen.try_recv().is_err());
}
#[tokio::test]
async fn decision_request_files_refuse_unsafe_or_mismatched_inputs_without_http() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("payload.json");
    std::fs::write(
        &file,
        serde_json::to_vec(&fixture("request-decision-resolve.json")["payload"]).unwrap(),
    )
    .unwrap();
    let link = dir.path().join("link");
    symlink(&file, &link).unwrap();
    let fifo = dir.path().join("fifo");
    nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRUSR).unwrap();
    for path in [&link, &fifo, dir.path()] {
        let mut srv = server(vec![]).await;
        let err = aqv2::execute(
            srv.client.clone(),
            &[
                "decisions",
                "respond",
                "00000000-0000-4000-8000-000000000020",
                "--request",
                path.to_str().unwrap(),
            ],
        )
        .await
        .unwrap_err();
        assert_eq!(err["code"], "invalid_request_file");
        assert!(srv.seen.try_recv().is_err());
    }
    let mut srv = server(vec![]).await;
    let err = aqv2::execute(
        srv.client.clone(),
        &[
            "decisions",
            "respond",
            "00000000-0000-4000-8000-000000000099",
            "--request",
            file.to_str().unwrap(),
        ],
    )
    .await
    .unwrap_err();
    assert_eq!(err["code"], "invalid_request_file");
    assert!(srv.seen.try_recv().is_err());
}
#[test]
fn binary_reports_parse_and_auth_errors_as_json() {
    for args in [
        vec!["--json", "tasks", "show", "not-a-uuid"],
        vec!["--json", "projects", "list"],
        vec![
            "--json",
            "--origin",
            "http://example.test",
            "--cf-access-token-file",
            "/missing",
            "projects",
            "list",
        ],
        vec![
            "--json",
            "--origin",
            "https://example.test",
            "--cf-access-token-file",
            "/missing",
            "projects",
            "list",
        ],
    ] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_aqv2"))
            .env_remove("AQ_CHANNEL")
            .args(args)
            .output()
            .unwrap();
        assert!(!output.status.success());
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["schema_version"], 1);
        assert!(value["error"]["code"].is_string());
        assert!(output.stderr.is_empty());
    }
}

#[tokio::test]
async fn missing_server_action_kind_is_explicitly_unsupported_without_fallback() {
    let mut srv=server(vec![(StatusCode::UNPROCESSABLE_ENTITY,json!({
        "request_id":"00000000-0000-4000-8000-000000000100",
        "code":"unknown_action_kind","message":"This action kind is unsupported","retryable":false
    }))]).await;
    let err = aqv2::execute(
        srv.client.clone(),
        &[
            "tasks",
            "create",
            "--project",
            "00000000-0000-4000-8000-000000000010",
            "--expected-project-revision",
            "1",
            "--title",
            "Task",
        ],
    )
    .await
    .unwrap_err();
    assert_eq!(err["code"], "unsupported");
    assert_eq!(err["server_code"], "unknown_action_kind");
    let sent = srv.next().await;
    assert_eq!(sent.path, "/api/v1/actions");
    assert_eq!(err["operation_id"], sent.body["operation_id"]);
    assert!(srv.seen.try_recv().is_err());
}

#[tokio::test]
async fn review_regression_rejects_duplicate_decisions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("decision.json");
    let payload =
        serde_json::to_string(&fixture("request-decision-resolve.json")["payload"]).unwrap();
    for text in [
        payload.replace(
            "\"choice\":\"approve\"",
            "\"choice\":\"reject\",\"choice\":\"approve\"",
        ),
        payload.replace("\"digest\":", "\"digest\":\"other\",\"digest\":"),
    ] {
        std::fs::write(&path, text).unwrap();
        let mut srv = server(vec![]).await;
        let err = aqv2::execute(
            srv.client.clone(),
            &[
                "decisions",
                "respond",
                "00000000-0000-4000-8000-000000000020",
                "--request",
                path.to_str().unwrap(),
            ],
        )
        .await
        .unwrap_err();
        assert_eq!(err["code"], "invalid_request_file");
        assert!(srv.seen.try_recv().is_err());
    }
}
#[tokio::test]
async fn review_regression_preserves_project_not_found() {
    let mut srv = server(vec![(StatusCode::NOT_FOUND, json!({"code":"not_found"}))]).await;
    let err = aqv2::execute(
        srv.client.clone(),
        &[
            "tasks",
            "list",
            "--project",
            "00000000-0000-4000-8000-000000000099",
        ],
    )
    .await
    .unwrap_err();
    assert_eq!(err["code"], "not_found");
    assert_eq!(err["http_status"], 404);
    assert_eq!(srv.next().await.method, "GET");
    assert!(srv.seen.try_recv().is_err());
}

#[test]
fn review_regression_positional_json_text_keeps_human_output() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_aqv2"))
        .env_remove("AQ_CHANNEL")
        .args(["messages", "send", "--", "--json"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(!output.stderr.is_empty());
}

// Generated flags (flags.rs): a command's params schema gives its flags, so
// `project show --project-id ID` is `run project.show --param project_id=ID`.
#[tokio::test]
async fn generated_flags_build_the_same_request_as_params() {
    let project = "00000000-0000-4000-8000-000000000010";
    let mut srv = server(vec![]).await;
    let _ = aqv2::execute(
        srv.client.clone(),
        &["project", "show", "--project-id", project],
    )
    .await;
    let flagged = srv.next().await;
    let _ = aqv2::execute(
        srv.client.clone(),
        &[
            "run",
            "project.show",
            "--param",
            &format!("project_id={project}"),
        ],
    )
    .await;
    let plain = srv.next().await;
    assert_eq!(flagged.method, "POST");
    assert_eq!(flagged.path, "/api/v1/commands/project.show");
    assert_eq!(flagged.body, json!({"params":{"project_id":project}}));
    assert_eq!((flagged.path, flagged.body), (plain.path, plain.body));

    // Values are typed by the schema: integers are numbers, `=` is accepted,
    // and a flag mixes with --param.
    let args = [
        "task",
        "list",
        "--project-id",
        project,
        "--limit=5",
        "--param",
        "q=launch notes",
    ];
    let _ = aqv2::execute(srv.client.clone(), &args).await;
    let list = srv.next().await;
    assert_eq!(list.path, "/api/v1/commands/task.list");
    assert_eq!(
        list.body,
        json!({"params":{"project_id":project,"limit":5,"q":"launch notes"}})
    );

    // A mutation keeps `run`'s own flags and its generated operation ID.
    let args = [
        "task",
        "cancel",
        "--project-id",
        project,
        "--expected-version",
        "7",
        "--subject",
        "task:00000000-0000-4000-8000-000000000020:7",
    ];
    let _ = aqv2::execute(srv.client.clone(), &args).await;
    let cancel = srv.next().await;
    assert_eq!(cancel.path, "/api/v1/commands/task.cancel");
    let op = cancel.body["operation_id"].clone();
    assert!(op.is_string());
    assert_eq!(
        cancel.body["params"],
        json!({"operation_id":op,"project_id":project,"expected_version":"7"})
    );
    assert_eq!(cancel.body["subject"]["revision"], "7");
    assert!(srv.seen.try_recv().is_err());
}
#[tokio::test]
async fn generated_flags_refuse_bad_input_before_sending() {
    let project = "00000000-0000-4000-8000-000000000010";
    let mut srv = server(vec![]).await;
    for (args, needle) in [
        (
            vec!["task", "list", "--project-id", project, "--limit", "many"],
            "an integer",
        ),
        (
            vec!["task", "list", "--project-id", project, "--limit"],
            "takes a value",
        ),
        (
            vec![
                "task",
                "list",
                "--project-id",
                project,
                "--limit",
                "1",
                "--limit",
                "2",
            ],
            "twice",
        ),
        (
            vec![
                "task",
                "list",
                "--project-id",
                project,
                "--param",
                &format!("project_id={project}"),
            ],
            "both",
        ),
        (
            vec!["question", "list", "--state", "pending"],
            "open, closed",
        ),
        (
            vec!["task", "list", "--project-id", project, "--bogus", "1"],
            "--help",
        ),
    ] {
        let error = aqv2::execute(srv.client.clone(), &args).await.unwrap_err();
        assert_eq!(error["code"], "invalid_arguments", "{args:?}");
        let message = error["message"].as_str().unwrap();
        assert!(message.contains(needle), "{args:?}: {message}");
    }
    assert!(srv.seen.try_recv().is_err());
}
#[tokio::test]
async fn generated_help_needs_no_request() {
    let mut srv = server(vec![]).await;
    let help = |v: Value| v["help"].as_str().unwrap().to_owned();
    let show = help(
        aqv2::execute(srv.client.clone(), &["task", "list", "--help"])
            .await
            .unwrap(),
    );
    for needle in [
        "Tasks in a project",
        "task.list (read, needs task.read)",
        "--project-id",
        "required",
        "--limit       integer 1..64, default 25",
        "--param KEY=VALUE",
    ] {
        assert!(show.contains(needle), "{needle}: {show}");
    }
    let cancel = help(
        aqv2::execute(srv.client.clone(), &["task", "cancel", "-h"])
            .await
            .unwrap(),
    );
    assert!(
        cancel.contains("owner only") && cancel.contains("--subject TYPE:ID"),
        "{cancel}"
    );
    assert!(
        !cancel.contains("--operation-id <"),
        "the operation ID is filled in: {cancel}"
    );
    // An agent-only command says so to the owner instead of listing flags.
    let wait = aqv2::flags::help("wait.register", "owner").unwrap();
    assert!(wait.contains("Not available to the owner"), "{wait}");
    // Tokens that name a group list its commands.
    let group = help(
        aqv2::execute(srv.client.clone(), &["project"])
            .await
            .unwrap(),
    );
    for needle in ["project list", "project show", "project revision show"] {
        assert!(group.contains(needle), "{group}");
    }
    assert!(srv.seen.try_recv().is_err());
}
#[test]
fn every_catalog_command_has_help_and_resolvable_flags() {
    let mut described = 0;
    for c in aqv2::catalog::commands() {
        let name = c["name"].as_str().unwrap();
        for principal in c["principals"].as_object().unwrap().keys() {
            if !matches!(principal.as_str(), "owner" | "agent") {
                continue;
            }
            let help = aqv2::flags::help(name, principal).unwrap();
            assert!(
                help.contains(name) && help.contains("Usage:"),
                "{name}: {help}"
            );
            let flags = aqv2::flags::flags(name, principal);
            let mut names: Vec<String> = flags.iter().map(|f| f.name()).collect();
            names.sort();
            names.dedup();
            assert_eq!(names.len(), flags.len(), "{name}: flag names are unique");
            if aqv2::flags::schema(name, principal).is_some() {
                described += 1;
            }
        }
    }
    // Every schema but the three lifecycle requests resolves in this binary.
    let total: usize = aqv2::catalog::commands()
        .iter()
        .map(|c| {
            c["principals"]
                .as_object()
                .unwrap()
                .keys()
                .filter(|p| matches!(p.as_str(), "owner" | "agent"))
                .count()
        })
        .sum();
    assert!(total - described <= 3, "{described} of {total} described");
}
