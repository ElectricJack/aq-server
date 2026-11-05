use super::*;
use axum::{Router, body::Bytes, http::HeaderMap, routing::post};
use std::sync::{Arc, Mutex};
#[tokio::test]
async fn exact_cap_reaches_allowlisted_peer_with_distinct_upstream_key_and_preserved_sse() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let capture = seen.clone();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new().route(
        "/v1/responses",
        post(move |headers: HeaderMap, body: Bytes| {
            let capture = capture.clone();
            async move {
                assert_eq!(headers["authorization"], "Bearer upstream-private-key");
                capture
                    .lock()
                    .unwrap()
                    .push(serde_json::from_slice::<serde_json::Value>(&body).unwrap());
                (
                    [("content-type", "text/event-stream")],
                    "data: {\"type\":\"response.completed\"}\n\n",
                )
            }
        }),
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let scope = policy::tests::scope();
    let expected_scope = scope.owner_scope().clone();
    let owner = ScopedResponsesTransport::start_with_keys(
        scope,
        UpstreamEndpoint::development_loopback(&origin).unwrap(),
        Zeroizing::new(b"local-attempt-capability".to_vec()),
        Zeroizing::new(b"upstream-private-key".to_vec()),
    )
    .await
    .unwrap();
    assert_eq!(owner.owner_scope(), &expected_scope);
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    for bad in [
        br#"{"model":"offline-model","model":"foreign"}"#.as_slice(),
        br#"{"model":"foreign"}"#.as_slice(),
    ] {
        assert!(
            client
                .post(format!("{}/v1/responses", owner.origin()))
                .bearer_auth("local-attempt-capability")
                .header("content-type", "application/json")
                .body(bad.to_vec())
                .send()
                .await
                .is_err()
        );
    }
    let mut foreign: serde_json::Value = serde_json::from_slice(&policy::tests::body()).unwrap();
    foreign["input"] = serde_json::json!([{"type":"item_reference","id":"item_foreign"}]);
    assert!(
        client
            .post(format!("{}/v1/responses", owner.origin()))
            .bearer_auth("local-attempt-capability")
            .header("content-type", "application/json")
            .body(serde_json::to_vec(&foreign).unwrap())
            .send()
            .await
            .is_err()
    );
    assert!(seen.lock().unwrap().is_empty());
    let reply = client
        .post(format!("{}/v1/responses", owner.origin()))
        .bearer_auth("local-attempt-capability")
        .header("content-type", "application/json")
        .body(policy::tests::body())
        .send()
        .await
        .unwrap();
    assert_eq!(
        reply.text().await.unwrap(),
        "data: {\"type\":\"response.completed\"}\n\n"
    );
    let mut request = seen.lock().unwrap()[0].clone();
    assert_eq!(
        request.as_object_mut().unwrap().remove("max_output_tokens"),
        Some(100.into())
    );
    assert_eq!(
        request,
        serde_json::from_slice::<serde_json::Value>(&policy::tests::body()).unwrap()
    );
    owner.stop().await.unwrap();
    server.abort();
}
#[tokio::test]
async fn stop_cancels_pending_upstream_and_ambiguous_http_never_forwards() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let notify = entered.clone();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new().route(
        "/v1/responses",
        post(move || {
            let notify = notify.clone();
            async move {
                notify.notify_one();
                std::future::pending::<&'static str>().await
            }
        }),
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let owner = ScopedResponsesTransport::start_with_keys(
        policy::tests::scope(),
        UpstreamEndpoint::development_loopback(&origin).unwrap(),
        Zeroizing::new(b"local-capability".to_vec()),
        Zeroizing::new(b"upstream-private-key".to_vec()),
    )
    .await
    .unwrap();
    let authority = owner.origin().strip_prefix("http://").unwrap();
    for extra in [
        "Content-Length: 2\r\nContent-Length: 2\r\n",
        "Content-Length: 2\r\nTransfer-Encoding: chunked\r\n",
        "Content-Length: 2\r\nContent-Encoding: gzip\r\n",
    ] {
        let mut connection = TcpStream::connect(authority).await.unwrap();
        connection.write_all(format!("POST /v1/responses HTTP/1.1\r\nHost: {authority}\r\nAuthorization: Bearer local-capability\r\nContent-Type: application/json\r\n{extra}\r\n{{}}").as_bytes()).await.unwrap();
        let mut bytes = Vec::new();
        let closed =
            tokio::time::timeout(Duration::from_secs(1), connection.read_to_end(&mut bytes))
                .await
                .unwrap();
        assert!(
            closed.is_ok()
                || closed.is_err_and(|error| error.kind() == std::io::ErrorKind::ConnectionReset)
        );
        assert!(bytes.is_empty());
    }
    let endpoint = format!("{}/v1/responses", owner.origin());
    let request = tokio::spawn(async move {
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .post(endpoint)
            .bearer_auth("local-capability")
            .header("content-type", "application/json")
            .body(policy::tests::body())
            .send()
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), owner.stop())
        .await
        .unwrap()
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(1), request)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    server.abort();
}
#[tokio::test]
async fn endpoint_and_key_scope_refuse_before_serving() {
    for url in [
        "https://127.0.0.1:444",
        "http://localhost:444",
        "http://127.0.0.1:444/path",
        "http://127.0.0.1:444/?q=1",
        "http://user@127.0.0.1:444",
        "http://127.0.0.1",
        "http://127.0.0.2:444",
    ] {
        assert!(UpstreamEndpoint::development_loopback(url).is_err());
    }
    assert!(
        ScopedResponsesTransport::start_with_keys(
            policy::tests::scope(),
            UpstreamEndpoint::openai(),
            Zeroizing::new(b"same-secret".to_vec()),
            Zeroizing::new(b"same-secret".to_vec())
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn exact_http_route_host_capability_and_framing_refuse_without_forwarding() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let forwarded = Arc::new(AtomicUsize::new(0));
    let count = forwarded.clone();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route(
                "/v1/responses",
                post(move || {
                    count.fetch_add(1, Ordering::SeqCst);
                    async { ([("content-type", "text/event-stream")], "data: done\n\n") }
                }),
            ),
        )
        .await
        .unwrap();
    });
    let owner = ScopedResponsesTransport::start_with_keys(
        policy::tests::scope(),
        UpstreamEndpoint::development_loopback(&origin).unwrap(),
        Zeroizing::new(b"local-capability".to_vec()),
        Zeroizing::new(b"upstream-private-key".to_vec()),
    )
    .await
    .unwrap();
    let authority = owner.origin().strip_prefix("http://").unwrap();
    let body = policy::tests::body();
    let good = format!(
        "POST /v1/responses HTTP/1.1\r\nHost: {authority}\r\nAuthorization: Bearer local-capability\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    for head in [
        good.replace("POST /v1/responses", "GET /v1/responses"),
        good.replace("/v1/responses HTTP", "/v1/responses?foreign=1 HTTP"),
        good.replace(&format!("Host: {authority}"), "Host: localhost:1"),
        good.replace("Bearer local-capability", "Bearer upstream-private-key"),
        good.replace(
            "Content-Type: application/json",
            "Content-Type: application/json; charset=utf-8",
        ),
        good.replace("Content-Length:", "Content-Length: +"),
        good.replace("\r\n\r\n", "\r\nUpgrade: websocket\r\n\r\n"),
        good.replace("\r\n\r\n", "\r\nExpect: 100-continue\r\n\r\n"),
        good.replace(
            "\r\n\r\n",
            "\r\nAuthorization: Bearer local-capability\r\n\r\n",
        ),
    ] {
        let mut connection = TcpStream::connect(authority).await.unwrap();
        connection.write_all(head.as_bytes()).await.unwrap();
        // Some refusals close before the body is sent. A reset is a refusal.
        let _ = connection.write_all(&body).await;
        let mut reply = Vec::new();
        let closed =
            tokio::time::timeout(Duration::from_secs(1), connection.read_to_end(&mut reply))
                .await
                .unwrap();
        assert!(
            closed.is_ok()
                || closed.is_err_and(|e| e.kind() == std::io::ErrorKind::ConnectionReset)
        );
        assert!(reply.is_empty());
        assert_eq!(forwarded.load(Ordering::SeqCst), 0);
    }
    owner.stop().await.unwrap();
    server.abort();
}

#[tokio::test]
async fn upstream_redirect_or_non_sse_never_becomes_success_or_follows_redirect() {
    use axum::{http::StatusCode, response::IntoResponse};
    use std::sync::atomic::{AtomicUsize, Ordering};
    for redirect in [true, false] {
        let followed = Arc::new(AtomicUsize::new(0));
        let count = followed.clone();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let location = format!("{origin}/foreign");
        let app = Router::new()
            .route(
                "/v1/responses",
                post(move || {
                    let location = location.clone();
                    async move {
                        if redirect {
                            (
                                StatusCode::TEMPORARY_REDIRECT,
                                [("location", location)],
                                "redirect",
                            )
                                .into_response()
                        } else {
                            ([("content-type", "application/json")], "{}").into_response()
                        }
                    }
                }),
            )
            .route(
                "/foreign",
                post(move || {
                    count.fetch_add(1, Ordering::SeqCst);
                    async { ([("content-type", "text/event-stream")], "data: foreign\n\n") }
                }),
            );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let owner = ScopedResponsesTransport::start_with_keys(
            policy::tests::scope(),
            UpstreamEndpoint::development_loopback(&origin).unwrap(),
            Zeroizing::new(b"local-capability".to_vec()),
            Zeroizing::new(b"upstream-private-key".to_vec()),
        )
        .await
        .unwrap();
        assert!(
            reqwest::Client::builder()
                .no_proxy()
                .build()
                .unwrap()
                .post(format!("{}/v1/responses", owner.origin()))
                .bearer_auth("local-capability")
                .header("content-type", "application/json")
                .body(policy::tests::body())
                .send()
                .await
                .is_err()
        );
        assert_eq!(followed.load(Ordering::SeqCst), 0);
        owner.stop().await.unwrap();
        server.abort();
    }
}

#[tokio::test]
async fn bounded_stream_preserves_exact_limit_and_aborts_surplus_without_second_status() {
    for surplus in [false, true] {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let bytes = vec![b'x'; MAX_STREAM + usize::from(surplus)];
        let expected = Digest::of(&bytes);
        let app = Router::new().route(
            "/v1/responses",
            post(move || {
                let bytes = bytes.clone();
                async { ([("content-type", "text/event-stream")], bytes) }
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let owner = ScopedResponsesTransport::start_with_keys(
            policy::tests::scope(),
            UpstreamEndpoint::development_loopback(&origin).unwrap(),
            Zeroizing::new(b"local-capability".to_vec()),
            Zeroizing::new(b"upstream-private-key".to_vec()),
        )
        .await
        .unwrap();
        let reply = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .post(format!("{}/v1/responses", owner.origin()))
            .bearer_auth("local-capability")
            .header("content-type", "application/json")
            .body(policy::tests::body())
            .send()
            .await
            .unwrap();
        assert_eq!(reply.status(), reqwest::StatusCode::OK);
        let body = reply.bytes().await;
        if surplus {
            assert!(
                body.is_err(),
                "surplus aborts the existing chunked response"
            );
        } else {
            let body = body.unwrap();
            assert_eq!(body.len(), MAX_STREAM);
            assert_eq!(Digest::of(&body), expected);
        }
        owner.stop().await.unwrap();
        server.abort();
    }
}
