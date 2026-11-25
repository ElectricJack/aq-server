use super::*;

fn backend(endpoint: &str, secret: std::path::PathBuf) -> ApiBackend {
    ApiBackend::new(vec![ApiEndpoint {
        provider: "openai".into(),
        endpoint: endpoint.into(),
        secret_file: secret,
    }])
    .unwrap()
}

fn claimed(provider: &str) -> Claimed {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    Claimed {
        id: uuid::Uuid::new_v4(),
        request: request(false),
        candidate: api(provider),
        candidate_index: 0,
        deadline_at_ms: now + 30_000,
        claimed_at_ms: now,
        candidate_count: 1,
    }
}

#[tokio::test]
async fn api_backend_returns_text_and_usage_and_never_stores_the_secret() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    let dir = tempfile::tempdir().unwrap();
    let text = r#"{"choice": "small", "rationale": "ok"}"#;
    let (endpoint, seen) = fake::serve(200, fake::completed(text)).await;
    let api_backend = backend(&endpoint, secret_file(dir.path(), 0o600));

    let id = store::submit(&pool, &request(false), &[api("openai")], NOW)
        .await
        .unwrap();
    let held = store::Queue::at(&pool, NOW + 5)
        .claim(Way::Api, &[], "r")
        .await
        .unwrap()
        .unwrap();
    // The claimed deadline is in the past relative to the wall clock.
    let past = api_backend.run(&held).await.unwrap_err();
    assert_eq!(past.0, Reason::DeadlineExceeded);

    let live = Claimed {
        deadline_at_ms: held.deadline_at_ms + 4_000_000_000_000,
        ..held
    };
    let (raw, usage) = api_backend.run(&live).await.unwrap();
    assert_eq!(raw, text);
    let usage = usage.unwrap();
    assert_eq!((usage.input_tokens, usage.output_tokens), (12, 5));
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.authorization, ["Bearer sk-test-secret-value"]);
        let body = &seen.bodies[0];
        assert_eq!(body["model"], "openai-model");
        assert_eq!(body["store"], false);
        assert!(
            body["input"]
                .as_str()
                .unwrap()
                .contains("Task: fix the login page")
        );
    }
    let answer = validate::answer(&choice_shape(), &raw, 4_096).unwrap();
    store::Queue::at(&pool, NOW + 50)
        .complete(store::CandidateAttempt::new(id, 0), &answer, Some(&usage))
        .await
        .unwrap();
    let row = store::get(&pool, id).await.unwrap().unwrap();
    let stored = format!("{row:?}{:?}", row.outcome());
    assert!(!stored.contains("sk-test-secret-value"), "{stored}");

    // Error strings carry no key either.
    let bad = backend(&endpoint, dir.path().join("missing"));
    let err = bad.run(&claimed("openai")).await.unwrap_err();
    assert_eq!(err.0, Reason::ProviderUnavailable);
    assert!(
        !err.1.contains("sk-test") && !err.1.contains("missing"),
        "{}",
        err.1
    );
    let none = api_backend.run(&claimed("anthropic")).await.unwrap_err();
    assert_eq!(
        none,
        (
            Reason::ProviderUnavailable,
            "no api endpoint configured for anthropic".to_string()
        )
    );
}

#[tokio::test]
async fn api_429_and_500_are_unavailable_and_400_is_error() {
    let dir = tempfile::tempdir().unwrap();
    let secret = secret_file(dir.path(), 0o600);
    for (status, reason) in [
        (429, Reason::ProviderUnavailable),
        (408, Reason::ProviderUnavailable),
        (500, Reason::ProviderUnavailable),
        (503, Reason::ProviderUnavailable),
        (400, Reason::ProviderError),
        (401, Reason::ProviderError),
    ] {
        let (endpoint, _) = fake::serve(status, json!({"error": "sk-test-secret-value"})).await;
        let err = backend(&endpoint, secret.clone())
            .run(&claimed("openai"))
            .await
            .unwrap_err();
        assert_eq!(err.0, reason, "HTTP {status}");
        assert!(err.1.contains(&status.to_string()), "{}", err.1);
        assert!(!err.1.contains("sk-test"), "{}", err.1);
    }
    // Nothing listens on this port: a transport error.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/v1/responses", closed.local_addr().unwrap());
    drop(closed);
    let err = backend(&endpoint, secret)
        .run(&claimed("openai"))
        .await
        .unwrap_err();
    assert_eq!(err.0, Reason::ProviderUnavailable);
    // An incomplete response is a provider error.
    let (endpoint, _) = fake::serve(200, json!({"status": "incomplete"})).await;
    let dir2 = tempfile::tempdir().unwrap();
    let err = backend(&endpoint, secret_file(dir2.path(), 0o600))
        .run(&claimed("openai"))
        .await
        .unwrap_err();
    assert_eq!(err.0, Reason::ProviderError);
}

#[tokio::test]
async fn secret_file_that_is_group_readable_or_a_symlink_is_refused() {
    let (endpoint, seen) = fake::serve(200, fake::completed("{}")).await;
    let dir = tempfile::tempdir().unwrap();
    let group = secret_file(dir.path(), 0o640);
    let err = backend(&endpoint, group)
        .run(&claimed("openai"))
        .await
        .unwrap_err();
    assert_eq!(err.0, Reason::ProviderUnavailable);
    assert!(err.1.contains("unusable"), "{}", err.1);

    let dir = tempfile::tempdir().unwrap();
    let real = secret_file(dir.path(), 0o600);
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let err = backend(&endpoint, link)
        .run(&claimed("openai"))
        .await
        .unwrap_err();
    assert_eq!(err.0, Reason::ProviderUnavailable);
    assert!(
        seen.lock().unwrap().authorization.is_empty(),
        "nothing was sent"
    );

    // A relative path and a plain-http remote endpoint are refused up front.
    for (url, file) in [
        (endpoint.as_str(), std::path::PathBuf::from("key")),
        ("http://example.com/v1/responses", real.clone()),
    ] {
        assert!(
            ApiBackend::new(vec![ApiEndpoint {
                provider: "openai".into(),
                endpoint: url.into(),
                secret_file: file,
            }])
            .is_err()
        );
    }
}
