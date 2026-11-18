//! Spec 0016 §5.2 (feature `delivery_playbook`): the mounted poll carries each
//! offer's git grant, and a git client using its token clones from and pushes
//! to the task ref through `/v1/git`. `git http-backend` stands in for aq-git's
//! git socket (`aq_git_transfer::fake`) and records the headers it receives.
use crate::common::native_offer::{poll_offered, setup};
use aq_admission::{
    control::ControlReply,
    git_grant::{self, decode_poll_reply},
};
use aq_git_transfer::{GitHttpConfig, Loaded};
use std::{path::Path, sync::Arc};
use uuid::Uuid;

async fn git(dir: &Path, home: &Path, args: &[&str]) -> (bool, String) {
    let out = tokio::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "aq")
        .env("GIT_AUTHOR_EMAIL", "aq@example.invalid")
        .env("GIT_COMMITTER_NAME", "aq")
        .env("GIT_COMMITTER_EMAIL", "aq@example.invalid")
        .output()
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).trim().to_owned()
        + &String::from_utf8_lossy(&out.stderr);
    (out.status.success(), text)
}
async fn ok(dir: &Path, home: &Path, args: &[&str]) -> String {
    let (success, text) = git(dir, home, args).await;
    assert!(success, "git {args:?}: {text}");
    text.lines().next().unwrap_or("").to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offer_carries_a_git_grant_and_its_token_clones_and_pushes_only_the_task_ref() {
    let mut c = setup().await;
    let repository = c.env.frozen.project.binding.repository_id;
    let task = c.env.lineage.task_id;
    let task_ref = git_grant::task_ref(task);
    let dir = tempfile::tempdir().unwrap();
    let (root, home, work) = (
        dir.path().join("repos"),
        dir.path().join("home"),
        dir.path().join("work"),
    );
    for d in [&root, &home, &work] {
        std::fs::create_dir_all(d).unwrap();
    }
    // Central aq-git holds main and an earlier attempt's task branch.
    let bare = root.join(repository.to_string());
    let bare = bare.to_str().unwrap();
    ok(&root, &home, &["init", "-q", "--bare", "-b", "main", bare]).await;
    ok(&work, &home, &["init", "-q", "-b", "main"]).await;
    std::fs::write(work.join("README"), "base\n").unwrap();
    ok(&work, &home, &["add", "README"]).await;
    ok(&work, &home, &["commit", "-q", "-m", "base"]).await;
    let earlier = ok(&work, &home, &["rev-parse", "HEAD"]).await;
    ok(
        &work,
        &home,
        &["push", "-q", bare, "main", &format!("main:{task_ref}")],
    )
    .await;

    let socket = dir.path().join("git-http.sock");
    let fake = aq_git_transfer::fake::serve(&socket, &root);
    let key = [0x42u8; 32];
    let mut config = (*c.env.env.state.config).clone();
    config.delegated_permits = Some(Arc::new(aq_server::delegated_permits::CredentialKey::new(
        key,
    )));
    let loaded = Loaded::new(GitHttpConfig {
        socket,
        transfer_timeout_ms: 60_000,
        max_push_bytes: 16 * 1024 * 1024,
        concurrency: 2,
    });
    config.git_http = Some(Arc::new(loaded.unwrap()));
    c.env.env.state.config = Arc::new(config);

    let (status, body) = poll_offered(&c).await;
    assert_eq!(status, 200, "{body}");
    let (reply, grants) = decode_poll_reply(&serde_json::to_vec(&body).unwrap()).unwrap();
    let ControlReply::Poll(page) = reply else {
        panic!("{body}")
    };
    page.validate_for(&c.request).unwrap();
    let attempt = page.offers[0].offered.offer.binding.attempt_id;
    assert_eq!(grants.len(), 1, "{body}");
    assert_eq!(grants[0].attempt_id, attempt);
    let grant = grants[0].grant.clone();
    assert_eq!(grant.endpoint, git_grant::endpoint(repository));
    assert_eq!(grant.repository_id, repository);
    assert_eq!(grant.task_ref, task_ref);
    assert_eq!(grant.base.hex, "a".repeat(40));
    assert_eq!(
        grant.lease.as_ref().map(|l| l.hex.as_str()),
        Some(earlier.as_str())
    );
    // The stored offer keeps the grant's facts, never its token.
    let stored: serde_json::Value = sqlx::query_scalar(
        "SELECT graph_task_observation FROM profiled_launches WHERE attempt_id=$1",
    )
    .bind(attempt)
    .fetch_one(&c.env.env.pool)
    .await
    .unwrap();
    assert_eq!(stored["git"]["task_ref"], task_ref.as_str());
    assert!(!stored.to_string().contains(grant.token.expose()));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let url = format!("{origin}{}", grant.endpoint);
    let app = aq_server::build_worker_router(c.env.env.state.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let auth = format!(
        "http.extraHeader=Authorization: Bearer {}",
        grant.token.expose()
    );
    // Grant headers from the client are ignored.
    let forged = "http.extraHeader=AQ-Task-Ref: refs/heads/main";
    let clone = dir.path().join("clone");
    let args = ["-c", &auth, "-c", forged, "clone", "-q", &url];
    ok(
        dir.path(),
        &home,
        &[&args[..], &[clone.to_str().unwrap()]].concat(),
    )
    .await;
    std::fs::write(clone.join("README"), "changed\n").unwrap();
    ok(&clone, &home, &["commit", "-q", "-am", "change"]).await;
    let head = ok(&clone, &home, &["rev-parse", "HEAD"]).await;
    let refspec = format!("HEAD:{task_ref}");
    ok(
        &clone,
        &home,
        &["-c", &auth, "-c", forged, "push", "-q", &url, &refspec],
    )
    .await;
    let pushed = ok(&root, &home, &["--git-dir", bare, "rev-parse", &task_ref]).await;
    assert_eq!(pushed, head);
    assert_eq!(
        ok(&root, &home, &["--git-dir", bare, "rev-parse", "main"]).await,
        earlier
    );
    for (path, headers) in fake.seen.lock().unwrap().iter() {
        let h = |n: &str| headers.get(n).map(|v| v.to_str().unwrap().to_owned());
        if path.contains("service=git-upload-pack") && h("git-protocol").is_none() {
            continue; // The offer's own lease read, before the attempt existed.
        }
        assert_eq!(
            h("aq-task-ref").as_deref(),
            Some(task_ref.as_str()),
            "{path}"
        );
        assert_eq!(h("aq-task-ref-lease").as_deref(), Some(earlier.as_str()));
        assert_eq!(h("aq-repository"), Some(repository.to_string()));
        assert_eq!(h("aq-operation"), Some(grant.operation_id.to_string()));
        assert_eq!(headers.get_all("aq-task-ref").iter().count(), 1);
        assert!(headers.get("authorization").is_none());
    }

    // Refused: a token for another attempt, a token for another repository,
    // this token on another repository's endpoint, and no token at all.
    let mac = move |a: Uuid, r: Uuid| {
        aq_server::delegated_permits::CredentialKey::new(key).git_grant_mac(a, r)
    };
    let mac: aq_git_transfer::Mac = Arc::new(mac);
    let other = Uuid::new_v4();
    let other_attempt = aq_git_transfer::token(&mac, Uuid::new_v4(), repository).unwrap();
    let other_repository = aq_git_transfer::token(&mac, attempt, other).unwrap();
    let http = reqwest::Client::new();
    let status = |endpoint: String, token: Option<String>| {
        let http = http.clone();
        let target = format!("{origin}{endpoint}/info/refs?service=git-upload-pack");
        async move {
            let mut request = http.get(target);
            if let Some(t) = token {
                request = request.bearer_auth(t);
            }
            request.send().await.unwrap().status().as_u16()
        }
    };
    let own = Some(grant.token.expose().to_owned());
    assert_eq!(status(grant.endpoint.clone(), own.clone()).await, 200);
    let other_attempt = Some(other_attempt.expose().to_owned());
    assert_eq!(status(grant.endpoint.clone(), other_attempt).await, 403);
    let other_repository = Some(other_repository.expose().to_owned());
    assert_eq!(status(grant.endpoint.clone(), other_repository).await, 401);
    assert_eq!(status(git_grant::endpoint(other), own).await, 401);
    assert_eq!(status(grant.endpoint.clone(), None).await, 401);
    server.abort();
}
