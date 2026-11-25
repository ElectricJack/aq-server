//! environment try (spec 0017, feature `project_environments`).
use super::*;

/// The CLI posts `{name, run?}` to the agent route itself (the node's proxy
/// pushes the workspace for that route and adds `commit`), then polls the job
/// and prints its result.
#[cfg(feature = "project_environments")]
#[tokio::test]
async fn environment_try_posts_to_the_agent_route_and_polls_the_job() {
    const JOB: &str = "00000000-0000-4000-8000-0000000000c1";
    let polls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = polls.clone();
    let result = json!({"job_id":JOB,"outcome":"built","builds":[],"failure":null,"run":{"exit":0,"output":"ok"}});
    let done = result.clone();
    let mut fake = fake(move |method, path, _| match (method, path) {
        ("POST", "/v1/agent/environments/try") => (StatusCode::OK, json!({"job_id":JOB})),
        ("GET", p) if p == format!("/v1/agent/environments/try/{JOB}") => {
            let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let state = if n < 1 { "claimed" } else { "completed" };
            let result = if n < 1 { Value::Null } else { done.clone() };
            (
                StatusCode::OK,
                json!({"job_id":JOB,"state":state,"result":result}),
            )
        }
        _ => (StatusCode::NOT_FOUND, json!({})),
    })
    .await;
    let value = fake
        .run(&[
            "environment",
            "try",
            "web",
            "--run",
            "pnpm test",
            "--interval",
            "1",
        ])
        .await
        .unwrap();
    assert_eq!(value["result"], result);
    let seen = fake.drain();
    assert_eq!(seen.len(), 3);
    assert_eq!(
        (seen[0].method.as_str(), seen[0].path.as_str()),
        ("POST", "/v1/agent/environments/try")
    );
    assert_eq!(
        seen[0].body,
        json!({"api_version":1,"name":"web","run":"pnpm test"})
    );
    assert!(seen[1..].iter().all(|s| s.method == "GET"));
    seen.iter().for_each(assert_no_credential);
    // Without --run the body names only the recipe.
    let mut fake = self::fake(|method, _, _| match method {
        "POST" => (StatusCode::OK, json!({"job_id":JOB})),
        _ => (
            StatusCode::OK,
            json!({"job_id":JOB,"state":"completed","result":{}}),
        ),
    })
    .await;
    fake.run(&["environment", "try", "web"]).await.unwrap();
    assert_eq!(fake.drain()[0].body, json!({"api_version":1,"name":"web"}));
}

#[cfg(feature = "project_environments")]
#[tokio::test]
async fn environment_try_refusals_and_timeouts_are_named() {
    for (status, code) in [
        (StatusCode::CONFLICT, "workspace_not_pushed"),
        (StatusCode::TOO_MANY_REQUESTS, "try_concurrency_reached"),
        (StatusCode::CONFLICT, "no_base_template"),
    ] {
        let body =
            json!({"api_version":1,"code":code,"message":"server text","effect":"not_applied"});
        let mut fake = fake(move |_, _, _| (status, body.clone())).await;
        let err = fake.run(&["environment", "try", "web"]).await.unwrap_err();
        assert_eq!(err["code"], code, "{err}");
        assert!(!err["message"].as_str().unwrap().contains("server text"));
        assert_eq!(aqv2::exit_code(&err), 1);
        assert_eq!(fake.drain().len(), 1, "a refused try is not polled");
    }
    let mut pending = fake(|method, _, _| match method {
        "POST" => (StatusCode::OK, json!({"job_id":OPERATION})),
        _ => (
            StatusCode::OK,
            json!({"job_id":OPERATION,"state":"queued","result":null}),
        ),
    })
    .await;
    let err = pending
        .run(&[
            "environment",
            "try",
            "web",
            "--interval",
            "1",
            "--timeout",
            "1",
        ])
        .await
        .unwrap_err();
    assert_eq!(err["code"], "wait_timeout");
    assert_eq!(aqv2::exit_code(&err), 16);
    assert!(
        err["message"].as_str().unwrap().contains(OPERATION),
        "{err}"
    );
    assert!(pending.drain().len() >= 2);
    // A failed build or a failing command exits nonzero and keeps the result.
    for result in [
        json!({"outcome":"failed","builds":[],"run":null,"failure":{"recipe":"web","step":"install","log":"E: no pnpm"}}),
        json!({"outcome":"built","builds":[],"failure":null,"run":{"exit":1,"output":"1 failing"}}),
    ] {
        let job = json!({"job_id":OPERATION,"state":"completed","result":result});
        let reply = job.clone();
        let fake = fake(move |method, _, _| match method {
            "POST" => (StatusCode::OK, json!({"job_id":OPERATION})),
            _ => (StatusCode::OK, reply.clone()),
        })
        .await;
        let err = fake.run(&["environment", "try", "web"]).await.unwrap_err();
        assert_eq!(err["code"], "environment_try_failed", "{err}");
        assert_eq!(err["job"], job);
        assert_eq!(aqv2::exit_code(&err), 1);
    }
    // A malformed recipe name is the server's to refuse; flags are the CLI's.
    let err = pending.run(&["environment", "try"]).await.unwrap_err();
    assert_eq!(err["code"], "invalid_arguments");
}
