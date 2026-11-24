use super::*;

fn action_cases(path: &std::path::Path) -> Vec<(&'static str, Vec<&str>)> {
    vec![
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
    ]
}

// Catches changed action kinds, mismatched nested IDs and use of legacy POST or lookup routes.
#[tokio::test]
async fn cli_and_dashboard_actions_share_operation_ids() {
    let temp = tempfile::tempdir().unwrap();
    let decision = fixture("request-decision-resolve.json");
    let path = temp.path().join("decision.json");
    std::fs::write(&path, serde_json::to_vec(&decision["payload"]).unwrap()).unwrap();
    for (file, args) in action_cases(&path) {
        // The reply intentionally has the wrong ID: the client must refuse it,
        // while preserving the actual minted ID sent in the request.
        let mut responses = vec![];
        if file.contains("supervisor") {
            let mut discovery: Value = serde_json::from_str(include_str!(
                "../../../../protocol/human-supervisor-conversation/v1/discovery.example.json"
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
async fn a_retry_refused_for_an_old_binding_explains_the_fresh_source_option() {
    let srv = server(vec![(
        StatusCode::UNPROCESSABLE_ENTITY,
        json!({
            "code":"replacement_project_revision_changed","message":"untrusted text"
        }),
    )])
    .await;
    let err = aqv2::execute(
        srv.client.clone(),
        &[
            "tasks",
            "retry",
            "00000000-0000-4000-8000-000000000010",
            "--project",
            "00000000-0000-4000-8000-000000000011",
            "--revision",
            "5",
        ],
    )
    .await
    .unwrap_err();
    assert_eq!(err["server_code"], "replacement_project_revision_changed");
    assert!(err["message"].as_str().unwrap().contains("--fresh true"));
    assert!(!err.to_string().contains("untrusted text"));
    assert!(err.get("recovery").is_none());
}
