//! A real git client clones from and pushes to a task ref through the proxy,
//! against `git http-backend` standing in for aq-git's git socket.
use crate::{
    GitHttpConfig, GrantSource, Loaded, Mac, OfferFacts, Proxy, StoredGrant, fake, offer_grant,
    routes, token,
};
use aq_admission::{ObjectFormat, ObjectId, git_grant};
use axum::{body::Body, http::Request};
use std::{
    collections::BTreeMap,
    future::Future,
    path::Path,
    pin::Pin,
    sync::{Arc, Mutex},
};
use tower::ServiceExt;
use uuid::Uuid;

#[derive(Default)]
struct Grants(Mutex<BTreeMap<Uuid, StoredGrant>>);
impl GrantSource for Grants {
    fn live_grant(
        &self,
        attempt_id: Uuid,
    ) -> Pin<Box<dyn Future<Output = Result<Option<StoredGrant>, String>> + Send + '_>> {
        let found = self.0.lock().unwrap().get(&attempt_id).cloned();
        Box::pin(async move { Ok(found) })
    }
}
fn mac() -> Mac {
    Arc::new(|a: Uuid, r: Uuid| {
        let mut out = [0x5au8; 32];
        for (i, b) in a.as_bytes().iter().chain(r.as_bytes()).enumerate() {
            out[i % 32] = out[i % 32].wrapping_mul(31) ^ b;
        }
        Some(out)
    })
}
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
async fn a_git_client_clones_and_pushes_the_task_ref_with_server_set_grant_headers() {
    let dir = tempfile::tempdir().unwrap();
    let (root, home, work) = (
        dir.path().join("repos"),
        dir.path().join("home"),
        dir.path().join("work"),
    );
    for d in [&root, &home, &work] {
        std::fs::create_dir_all(d).unwrap();
    }
    let (repository, task, attempt) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let bare = root.join(repository.to_string());
    let bare = bare.to_str().unwrap();
    ok(&root, &home, &["init", "-q", "--bare", "-b", "main", bare]).await;
    ok(&work, &home, &["init", "-q", "-b", "main"]).await;
    std::fs::write(work.join("README"), "base\n").unwrap();
    ok(&work, &home, &["add", "README"]).await;
    ok(&work, &home, &["commit", "-q", "-m", "base"]).await;
    ok(&work, &home, &["push", "-q", bare, "main"]).await;
    let base = ok(&work, &home, &["rev-parse", "HEAD"]).await;

    let socket = dir.path().join("git-http.sock");
    let fake = fake::serve(&socket, &root);
    let loaded = Arc::new(
        Loaded::new(GitHttpConfig {
            socket,
            transfer_timeout_ms: 60_000,
            max_push_bytes: 16 * 1024 * 1024,
            concurrency: 4,
        })
        .unwrap(),
    );
    let mac = mac();
    let facts = |attempt_id| OfferFacts {
        attempt_id,
        task_id: task,
        repository_id: repository,
        base: ObjectId {
            format: ObjectFormat::Sha1,
            hex: base.clone(),
        },
        main_ref: Some("refs/heads/main".into()),
    };
    // The task branch does not exist yet: the grant's lease is absent.
    let grant = offer_grant(&loaded, &mac, facts(attempt)).await.unwrap();
    assert_eq!(grant.lease, None);
    assert_eq!(grant.task_ref, git_grant::task_ref(task));
    let grants = Arc::new(Grants::default());
    grants.0.lock().unwrap().insert(attempt, grant.clone());
    let app = routes::<()>(Proxy {
        loaded: loaded.clone(),
        mac: mac.clone(),
        grants: grants.clone(),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}{}",
        listener.local_addr().unwrap(),
        git_grant::endpoint(repository)
    );
    let server = tokio::spawn({
        let app = app.clone();
        async move { axum::serve(listener, app).await.unwrap() }
    });

    let wire = grant.wire(&mac, attempt).unwrap();
    let auth = format!(
        "http.extraHeader=Authorization: Bearer {}",
        wire.token.expose()
    );
    // A client's own grant headers are ignored.
    let forged = "http.extraHeader=AQ-Task-Ref: refs/heads/main";
    let clone = dir.path().join("clone");
    let clone_path = clone.to_str().unwrap();
    ok(
        dir.path(),
        &home,
        &["-c", &auth, "-c", forged, "clone", "-q", &url, clone_path],
    )
    .await;
    assert_eq!(ok(&clone, &home, &["rev-parse", "HEAD"]).await, base);
    std::fs::write(clone.join("README"), "changed\n").unwrap();
    ok(&clone, &home, &["commit", "-q", "-am", "change"]).await;
    let head = ok(&clone, &home, &["rev-parse", "HEAD"]).await;
    let refspec = format!("HEAD:{}", wire.task_ref);
    ok(
        &clone,
        &home,
        &["-c", &auth, "-c", forged, "push", "-q", &url, &refspec],
    )
    .await;
    assert_eq!(
        ok(
            &root,
            &home,
            &["--git-dir", bare, "rev-parse", &wire.task_ref]
        )
        .await,
        head
    );
    {
        let seen = fake.seen.lock().unwrap();
        assert!(seen.iter().any(|(p, _)| p.ends_with("/git-receive-pack")));
        for (path, headers) in seen.iter() {
            assert!(path.starts_with(&format!("/{repository}/")), "{path}");
            let h = |n: &str| headers.get(n).map(|v| v.to_str().unwrap().to_owned());
            assert_eq!(h("aq-task-ref"), Some(git_grant::task_ref(task)), "{path}");
            assert_eq!(h("aq-repository"), Some(repository.to_string()));
            assert_eq!(h("aq-base"), Some(base.clone()));
            assert_eq!(h("aq-main-ref").as_deref(), Some("refs/heads/main"));
            assert_eq!(h("aq-max-push-bytes").as_deref(), Some("16777216"));
            assert_eq!(h("aq-operation"), Some(grant.operation_id.to_string()));
            assert_eq!(headers.get_all("aq-task-ref").iter().count(), 1);
            assert!(headers.get("authorization").is_none());
        }
    }
    // The next attempt's lease is the branch's head.
    let next = offer_grant(&loaded, &mac, facts(Uuid::new_v4()))
        .await
        .unwrap();
    assert_eq!(next.lease.map(|l| l.hex), Some(head));

    // Refused: another repository's token, an attempt without a live grant,
    // no token, and a route git's smart HTTP does not use.
    let status = |token: Option<String>, path: &str| {
        let app = app.clone();
        let path = format!("{}/{path}", git_grant::endpoint(repository));
        async move {
            let mut request = Request::get(path);
            if let Some(t) = token {
                request = request.header("authorization", format!("Bearer {t}"));
            }
            let response = app
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            response.status().as_u16()
        }
    };
    let refs = "info/refs?service=git-upload-pack";
    let other_repository = token(&mac, attempt, Uuid::new_v4()).unwrap();
    let other_attempt = token(&mac, Uuid::new_v4(), repository).unwrap();
    assert_eq!(
        status(Some(other_repository.expose().into()), refs).await,
        401
    );
    assert_eq!(status(Some(other_attempt.expose().into()), refs).await, 403);
    assert_eq!(status(None, refs).await, 401);
    assert_eq!(status(Some(wire.token.expose().into()), refs).await, 200);
    assert_eq!(status(Some(wire.token.expose().into()), "HEAD").await, 404);
    let (cloned, _) = git(
        dir.path(),
        &home,
        &[
            "-c",
            &format!(
                "http.extraHeader=Authorization: Bearer {}",
                other_attempt.expose()
            ),
            "ls-remote",
            &url,
        ],
    )
    .await;
    assert!(!cloned, "a token for another attempt is refused");
    server.abort();
}
