//! Spec 0016 §5.2 (feature `delivery_playbook`): an offer of a project that
//! follows `main` carries a git grant, and a git client holding the node
//! credential and the grant's token clones and pushes the task ref through
//! the worker route `/v1/git`. aq-git's git socket is a fake served by
//! `git http-backend`, which records the headers each request reached it with.
use crate::common::native_offer::{Case, poll_offered, setup_source};
use aq_admission::{
    Label, SourceSelection,
    control::{ControlReply, decode_reply},
    startup::NativeOffer,
};
use aq_delivery::git_grant::{
    self,
    fake::{FakeGitHttp, git},
};
use hmac::Mac;
use std::sync::Arc;
use uuid::Uuid;

const MAIN: &str = "refs/heads/main";
const KEY: [u8; 32] = [7; 32];

/// A `main` the fixture's synthetic aq-git retained a moment ago, so the offer
/// starts from it without asking aq-git to sync.
async fn synced_main(c: &Case) {
    let binding = &c.env.frozen.project.binding;
    let config = c.env.env.state.config.profiled_execution.clone().unwrap();
    let now = (c.env.env.state.clock)().timestamp_millis();
    sqlx::query(
        "INSERT INTO profiled_main_syncs(project_id,repository_id,binding_generation,reference,\
         retention_handle,head,synced_at_ms,recorded_at_ms) VALUES($1,$2,1,$3,$4,$5,$6,$6)",
    )
    .bind(binding.project_id)
    .bind(binding.repository_id)
    .bind(MAIN)
    .bind(config.projects[0].retention_handle)
    .bind("a".repeat(40))
    .bind(now)
    .execute(&c.env.env.pool)
    .await
    .unwrap();
}
fn token_for(attempt: Uuid, node: Uuid, repository: Uuid) -> String {
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(&KEY).unwrap();
    mac.update(&git_grant::token_message(attempt, node, repository));
    git_grant::token(attempt, &mac.finalize().into_bytes().into())
}
/// `git -c <each header> args…` against the served worker router, off the
/// test's runtime thread.
async fn client(dir: &std::path::Path, headers: &[String], args: &[&str]) -> std::process::Output {
    let mut command = tokio::process::Command::new("git");
    command.arg("-C").arg(dir);
    for header in headers {
        command.arg("-c").arg(format!("http.extraHeader={header}"));
    }
    (command.args(args))
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .await
        .unwrap()
}

#[tokio::test]
async fn offer_carries_a_git_grant_whose_token_clones_and_pushes_only_the_task_ref() {
    let reference = Label::new(MAIN).unwrap();
    let mut c = setup_source(Some(SourceSelection::NamedRef { reference })).await;
    synced_main(&c).await;
    let repository = c.env.frozen.project.binding.repository_id;
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeGitHttp::start(dir.path(), repository, MAIN).await;
    let mut config = (*c.env.env.state.config).clone();
    config.delivery = Some(aq_delivery::github::DeliveryConfig {
        git_http_socket: Some(fake.socket.clone()),
        ..Default::default()
    });
    config.delegated_permits = Some(Arc::new(aq_server::delegated_permits::CredentialKey::new(
        KEY,
    )));
    c.env.env.state.config = Arc::new(config);

    // The offer carries the grant, with the token only in the delivered copy.
    let (status, body) = poll_offered(&c).await;
    assert_eq!(status, 200, "{body}");
    let ControlReply::Poll(mut page) = decode_reply(&serde_json::to_vec(&body).unwrap()).unwrap()
    else {
        panic!("{body}")
    };
    assert_eq!(page.offers.len(), 1, "{body}");
    let offer: NativeOffer = page.offers.remove(0);
    offer.validate().unwrap();
    let binding = &offer.offered.offer.binding;
    let (attempt, task) = (
        binding.attempt_id,
        offer.offered.offer.manifest.bindings.task_id,
    );
    let grant = offer.git.clone().expect("the offer carries a git grant");
    assert_eq!(grant.endpoint, format!("/v1/git/{repository}"));
    assert_eq!(grant.repository_id, repository);
    assert_eq!(grant.base, "a".repeat(40));
    assert_eq!(grant.task_ref, format!("refs/heads/aq/{task}"));
    assert_eq!(grant.lease, None, "the task ref does not exist yet");
    assert_eq!(grant.main_ref, MAIN);
    assert_eq!(grant.operation_id, git_grant::push_operation(attempt));
    let node = c.request.identity.node_id;
    let token = grant
        .token
        .clone()
        .expect("the delivered grant has its token");
    assert_eq!(token, token_for(attempt, node, repository));
    let stored: serde_json::Value = sqlx::query_scalar(
        "SELECT graph_task_observation#>'{native_offer,git}' FROM profiled_launches WHERE attempt_id=$1",
    )
    .bind(attempt)
    .fetch_one(&c.env.env.pool)
    .await
    .unwrap();
    assert_eq!(stored["task_ref"], grant.task_ref);
    assert!(
        stored.get("token").is_none(),
        "the token is never stored: {stored}"
    );
    let recorded: (Uuid, String, Option<String>) = sqlx::query_as(
        "SELECT node_id,task_ref,lease FROM delivery_git_grants WHERE attempt_id=$1",
    )
    .bind(attempt)
    .fetch_one(&c.env.env.pool)
    .await
    .unwrap();
    assert_eq!(recorded, (node, grant.task_ref.clone(), None));

    // The worker listener, served for a real git client.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let router = aq_server::build_worker_router(c.env.env.state.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let credential = format!("Authorization: Bearer {}", c.env.env.credential);
    let granted = vec![
        credential.clone(),
        format!("AQ-Git-Token: {token}"),
        // Grant headers from the client are ignored: the server sets its own.
        format!("AQ-Task-Ref: {MAIN}"),
        format!("AQ-Base: {}", "0".repeat(40)),
    ];
    let url = format!("{origin}{}", grant.endpoint);
    let work = dir.path().join("work");
    let out = client(
        dir.path(),
        &granted,
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
    let out = client(&work, &granted, &["push", "-q", "origin", &target]).await;
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
        assert_eq!(all("aq-main-ref"), [MAIN]);
        assert_eq!(all("aq-repository"), [repository.to_string()]);
        assert_eq!(all("aq-operation"), [grant.operation_id.to_string()]);
        assert!(all("authorization").is_empty() && all("aq-git-token").is_empty());
    }

    // Refused: another attempt's token (valid MAC, no live grant), this token
    // on another repository, and this token without the node credential.
    let other = Uuid::new_v4();
    let elsewhere = format!("{origin}/v1/git/{other}");
    let other_attempt = vec![
        credential.clone(),
        format!("AQ-Git-Token: {}", token_for(other, node, repository)),
    ];
    let no_node = vec![format!("AQ-Git-Token: {token}")];
    for (headers, url, status) in [
        (&other_attempt, &url, Some("403")),
        (&granted, &elsewhere, Some("403")),
        // 401: git asks for a password, which no prompt can give.
        (&no_node, &url, None),
    ] {
        let out = client(dir.path(), headers, &["ls-remote", url]).await;
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{url}: {stderr}");
        assert!(status.is_none_or(|s| stderr.contains(s)), "{url}: {stderr}");
    }
    assert_eq!(
        fake.seen.lock().unwrap().len(),
        seen.len(),
        "no refusal reached aq-git"
    );
    server.abort();
}
