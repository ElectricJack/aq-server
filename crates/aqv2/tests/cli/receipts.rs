use super::*;

const CREATE_TASK: &[&str] = &[
    "tasks",
    "create",
    "--project",
    "00000000-0000-4000-8000-000000000010",
    "--expected-project-revision",
    "1",
    "--title",
    "Task",
];

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
        let err = aqv2::execute(srv.client.clone(), CREATE_TASK)
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
        let err = aqv2::execute(srv.client.clone(), CREATE_TASK)
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
    let err = aqv2::execute(client, CREATE_TASK).await.unwrap_err();
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
    let err = aqv2::execute(srv.client.clone(), CREATE_TASK)
        .await
        .unwrap_err();
    assert_eq!(err["code"], "invalid_response");
    let sent = srv.next().await;
    assert_eq!(err["operation_id"], sent.body["operation_id"]);
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
    let value = aqv2::execute(srv.client.clone(), CREATE_TASK)
        .await
        .unwrap();
    let sent = srv.next().await;
    assert_eq!(value["data"]["operation_id"], sent.body["operation_id"]);
    assert_eq!(value["data"]["disposition"], "confirmed");
    assert!(srv.seen.try_recv().is_err());
}
