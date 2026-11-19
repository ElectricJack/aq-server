//! Spec 0016 §5.2 (feature `delivery_playbook`): an offer carries a git
//! grant, and a git client holding only the node credential clones and
//! pushes the task ref through the worker route `/v1/git`. The server derives
//! the grant from the attempt and enforces it: the attempt's node and
//! repository, the task ref, the lease and Settings `max_push_bytes`. aq-git's
//! git socket is a fake served by `git http-backend`, which records the
//! headers each request reached it with.
use crate::common::native_offer::{Case, poll_offered, setup};
use aq_admission::{
    control::{ControlReply, decode_reply},
    startup::NativeOffer,
};
use aq_delivery::git_grant::GitGrant;
use aq_delivery::git_grant::fake::{FakeGitHttp, git};
use aq_server::{store::NewNode, tokens};
use chrono::{Duration, Utc};
use std::{path::Path, sync::Arc};
use uuid::Uuid;

/// `git -c http.extraHeader=<each header> args…`.
async fn client(dir: &Path, headers: &[String], args: &[&str]) -> std::process::Output {
    let mut command = tokio::process::Command::new("git");
    command.arg("-C").arg(dir);
    for header in headers {
        command.arg("-c").arg(format!("http.extraHeader={header}"));
    }
    (command.args(args))
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "aq")
        .env("GIT_AUTHOR_EMAIL", "aq@example.invalid")
        .env("GIT_COMMITTER_NAME", "aq")
        .env("GIT_COMMITTER_EMAIL", "aq@example.invalid")
        .output()
        .await
        .unwrap()
}
fn refused(out: &std::process::Output, status: &str) {
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(stderr.contains(status), "expected {status}: {stderr}");
}
/// Another enrolled node's credential.
async fn other_node(c: &Case) -> String {
    let (store, now) = (&c.env.env.state.store, Utc::now());
    let expires = now + Duration::minutes(5);
    store
        .create_enrollment("other-enroll", c.env.actor, now, expires)
        .await
        .unwrap();
    let token = tokens::new_node_credential();
    let node = NewNode {
        name: "other-worker".into(),
        credential_hash: tokens::verifier(&token),
        worker_version: "0.1.0".into(),
        protocol_version: 1,
    };
    let redeemed = store.redeem_enrollment("other-enroll", node, now).await;
    redeemed.unwrap().unwrap();
    token
}
/// The offer of `c` with the server serving git through `fake`.
async fn granted_offer(c: &mut Case, fake: &FakeGitHttp) -> (NativeOffer, GitGrant) {
    let mut config = (*c.env.env.state.config).clone();
    config.delivery = Some(aq_delivery::github::DeliveryConfig {
        git_http_socket: Some(fake.socket.clone()),
        ..Default::default()
    });
    c.env.env.state.config = Arc::new(config);
    let (status, body) = poll_offered(c).await;
    assert_eq!(status, 200, "{body}");
    let ControlReply::Poll(mut page) = decode_reply(&serde_json::to_vec(&body).unwrap()).unwrap()
    else {
        panic!("{body}")
    };
    assert_eq!(page.offers.len(), 1, "{body}");
    let offer: NativeOffer = page.offers.remove(0);
    offer.validate().unwrap();
    // The frozen offer types carry no grant: the node reads it beside the offer.
    let attempt = offer.offered.lifecycle_binding.attempt_id;
    let (status, grant) = get_grant(c, &c.env.env.credential, attempt).await;
    assert_eq!(status, 200, "{grant}");
    (offer, serde_json::from_value(grant).unwrap())
}
/// `GET /v1/git/grants/<attempt>` with `credential`.
async fn get_grant(c: &Case, credential: &str, attempt: Uuid) -> (u16, serde_json::Value) {
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let request = axum::http::Request::get(format!("/v1/git/grants/{attempt}"))
        .header("authorization", format!("Bearer {credential}"))
        .body(axum::body::Body::empty())
        .unwrap();
    let router = aq_server::build_worker_router(c.env.env.state.clone());
    let response = router.oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
    )
}

#[tokio::test]
async fn a_node_clones_and_pushes_only_its_attempts_task_ref_through_v1_git() {
    let mut c = setup().await;
    let repository = c.env.frozen.project.binding.repository_id;
    let git_root = (c.env.env.state.source_retention.as_ref().unwrap().config)
        .git_root
        .clone();
    let fake = FakeGitHttp::start(&git_root, repository, "refs/heads/main").await;
    let (offer, grant) = granted_offer(&mut c, &fake).await;
    let binding = &offer.offered.lifecycle_binding;
    let (attempt, task) = (binding.attempt_id, binding.task_id);
    assert_eq!(grant.endpoint, format!("/v1/git/{repository}"));
    assert_eq!(grant.repository_id, repository);
    assert_eq!(grant.base, "a".repeat(40));
    assert_eq!(grant.task_ref, format!("refs/heads/aq/{task}"));
    assert_eq!(grant.lease, None, "the task ref does not exist yet");
    assert!(!grant.update_from_main, "the task was never returned");
    let pool = &c.env.env.pool;
    let recorded: (Uuid, String, Option<String>) = sqlx::query_as(
        "SELECT repository_id,task_ref,lease FROM delivery_git_grants WHERE attempt_id=$1",
    )
    .bind(attempt)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(recorded, (repository, grant.task_ref.clone(), None));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let router = aq_server::build_worker_router(c.env.env.state.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let credential = format!("Authorization: Bearer {}", c.env.env.credential);
    // Grant headers from the client are ignored: the server sets its own.
    let attempt_header = format!("AQ-Attempt: {attempt}");
    let node = vec![
        credential.clone(),
        attempt_header.clone(),
        "AQ-Task-Ref: refs/heads/main".into(),
        format!("AQ-Base: {}", "0".repeat(40)),
        "AQ-Max-Push-Bytes: 999999999999".into(),
    ];
    let url = format!("{origin}{}", grant.endpoint);
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    let out = client(
        dir.path(),
        &node,
        &["clone", "-q", &url, work.to_str().unwrap()],
    )
    .await;
    assert!(out.status.success(), "clone: {out:?}");
    assert_eq!(git(&work, &["rev-parse", "HEAD"]), fake.base);
    std::fs::write(work.join("result.txt"), "the attempt's work\n").unwrap();
    git(&work, &["add", "result.txt"]);
    git(&work, &["commit", "-q", "-m", "work"]);
    let pushed = git(&work, &["rev-parse", "HEAD"]);
    let target = format!("HEAD:{}", grant.task_ref);
    let out = client(&work, &node, &["push", "-q", &url, &target]).await;
    assert!(out.status.success(), "push: {out:?}");
    assert_eq!(
        git(&fake.repository, &["rev-parse", &grant.task_ref]),
        pushed
    );
    let seen = fake.seen.lock().unwrap().clone();
    assert!(
        seen.len() >= 4,
        "clone and push reached aq-git: {}",
        seen.len()
    );
    for headers in &seen {
        let all = |name: &str| {
            (headers.get_all(name).iter())
                .map(|v| v.to_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(all("aq-task-ref"), [grant.task_ref.as_str()]);
        assert_eq!(all("aq-base"), [grant.base.as_str()]);
        assert_eq!(all("aq-repository"), [repository.to_string()]);
        assert_eq!(all("aq-max-push-bytes"), ["268435456"]);
        assert_eq!(all("aq-operation").len(), 1);
        assert!(all("authorization").is_empty() && all("aq-attempt").is_empty());
    }

    // Refused before aq-git: no credential, another node's credential, no or
    // another attempt, another repository.
    let other_token = other_node(&c).await;
    assert_eq!(
        get_grant(&c, &other_token, attempt).await.0,
        404,
        "another node's grant"
    );
    let other_node = format!("Authorization: Bearer {other_token}");
    let other = vec![other_node, attempt_header.clone()];
    let unknown = vec![
        credential.clone(),
        format!("AQ-Attempt: {}", Uuid::new_v4()),
    ];
    let unnamed = vec![credential.clone()];
    let elsewhere = format!("{origin}/v1/git/{}", Uuid::new_v4());
    for (headers, url, status) in [
        // 401: git asks for a username, which no prompt can give.
        (&vec![attempt_header.clone()], &url, "Username"),
        (&other, &url, "403"),
        (&unknown, &url, "403"),
        (&unnamed, &url, "400"),
        (&node, &elsewhere, "403"),
    ] {
        refused(
            &client(dir.path(), headers, &["ls-remote", url]).await,
            status,
        );
    }
    // A push of another ref, a deletion, or from another old head than the
    // lease (the grant's lease is "absent": the ref now exists).
    git(&work, &["commit", "-q", "--amend", "-m", "rework"]);
    let force = |target: String| {
        let (work, node, url) = (work.clone(), node.clone(), url.clone());
        async move { client(&work, &node, &["push", "-q", "--force", &url, &target]).await }
    };
    refused(&force("HEAD:refs/heads/main".into()).await, "403");
    refused(
        &force(format!("HEAD:refs/heads/aq/{}", Uuid::new_v4())).await,
        "403",
    );
    refused(&force(format!(":{}", grant.task_ref)).await, "403");
    refused(&force(target.clone()).await, "409");
    assert_eq!(
        git(&fake.repository, &["rev-parse", &grant.task_ref]),
        pushed
    );
    assert_eq!(
        fake.seen.lock().unwrap().len(),
        seen.len() + 4,
        "only advertisements"
    );

    // Larger than Settings `max_push_bytes`.
    let limits = aq_role_settings::delivery::DeliveryLimits {
        max_push_bytes: 64,
        ..Default::default()
    };
    let saved = aq_role_settings::save_delivery_limits(
        pool,
        c.env.actor,
        &limits,
        0,
        Uuid::new_v4(),
        Utc::now().timestamp_millis(),
    );
    saved.await.unwrap();
    refused(&force(target).await, "413");
    server.abort();
}
