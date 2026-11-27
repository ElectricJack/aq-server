//! Spec 0016 §5.2: an offer carries a git
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
use std::{
    collections::BTreeSet,
    path::Path,
    sync::{Arc, atomic::Ordering},
};
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
fn configure(
    c: &mut Case,
    fake: &FakeGitHttp,
    projects: impl IntoIterator<Item = Uuid>,
    task_allowlist: Option<BTreeSet<Uuid>>,
) {
    let mut config = (*c.env.env.state.config).clone();
    config.delivery = Some(aq_delivery::github::DeliveryConfig {
        git_http_socket: Some(fake.socket.clone()),
        project_allowlist: projects.into_iter().collect(),
        task_allowlist,
        ..Default::default()
    });
    c.env.env.state.config = Arc::new(config);
}

/// The offer of `c` with the server serving git through `fake`.
pub(super) async fn granted_offer(c: &mut Case, fake: &FakeGitHttp) -> (NativeOffer, GitGrant) {
    configure(c, fake, [c.env.lineage.project_id], None);
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

async fn check_grant(pool: &sqlx::PgPool, offer: &NativeOffer, grant: &GitGrant, repository: Uuid) {
    let binding = &offer.offered.lifecycle_binding;
    let (attempt, task) = (binding.attempt_id, binding.task_id);
    assert_eq!(grant.endpoint, format!("/v1/git/{repository}"));
    assert_eq!(grant.repository_id, repository);
    assert_eq!(grant.base, "a".repeat(40));
    assert_eq!(grant.task_ref, format!("refs/heads/aq/{task}"));
    assert_eq!(grant.lease, None, "the task ref does not exist yet");
    assert!(!grant.update_from_main, "the task was never returned");
    let recorded: (Uuid, String, Option<String>) = sqlx::query_as(
        "SELECT repository_id,task_ref,lease FROM delivery_git_grants WHERE attempt_id=$1",
    )
    .bind(attempt)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(recorded, (repository, grant.task_ref.clone(), None));
}
struct PushedWork {
    directory: std::path::PathBuf,
    head: String,
    forwarded_requests: usize,
}
struct NodeGit {
    origin: String,
    credential: String,
    attempt_header: String,
    node: Vec<String>,
    url: String,
}
impl NodeGit {
    fn new(c: &Case, origin: String, attempt: Uuid, grant: &GitGrant) -> Self {
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
        Self {
            origin,
            credential,
            attempt_header,
            node,
            url,
        }
    }
    async fn clone_and_push(&self, dir: &Path, fake: &FakeGitHttp, grant: &GitGrant) -> PushedWork {
        let work = dir.join("work");
        let out = client(
            dir,
            &self.node,
            &["clone", "-q", &self.url, work.to_str().unwrap()],
        )
        .await;
        assert!(out.status.success(), "clone: {out:?}");
        assert_eq!(git(&work, &["rev-parse", "HEAD"]), fake.base);
        std::fs::write(work.join("result.txt"), "the attempt's work\n").unwrap();
        git(&work, &["add", "result.txt"]);
        git(&work, &["commit", "-q", "-m", "work"]);
        let pushed = git(&work, &["rev-parse", "HEAD"]);
        let target = format!("HEAD:{}", grant.task_ref);
        let out = client(&work, &self.node, &["push", "-q", &self.url, &target]).await;
        assert!(out.status.success(), "push: {out:?}");
        assert_eq!(
            git(&fake.repository, &["rev-parse", &grant.task_ref]),
            pushed
        );
        PushedWork {
            directory: work,
            head: pushed,
            forwarded_requests: check_forwarded(fake, grant),
        }
    }
    async fn check_access(&self, c: &Case, attempt: Uuid, dir: &Path) {
        // Refused before aq-git: no credential, another node's credential, no or
        // another attempt, another repository.
        let other_token = other_node(c).await;
        assert_eq!(
            get_grant(c, &other_token, attempt).await.0,
            403,
            "another node's grant"
        );
        let other_node = format!("Authorization: Bearer {other_token}");
        let other = vec![other_node, self.attempt_header.clone()];
        let unknown = vec![
            self.credential.clone(),
            format!("AQ-Attempt: {}", Uuid::new_v4()),
        ];
        let unnamed = vec![self.credential.clone()];
        let elsewhere = format!("{}/v1/git/{}", self.origin, Uuid::new_v4());
        for (headers, url, status) in [
            // 401: git asks for a username, which no prompt can give.
            (&vec![self.attempt_header.clone()], &self.url, "Username"),
            (&other, &self.url, "403"),
            (&unknown, &self.url, "403"),
            (&unnamed, &self.url, "400"),
            (&self.node, &elsewhere, "403"),
        ] {
            refused(&client(dir, headers, &["ls-remote", url]).await, status);
        }
    }
    async fn check_ref_protection(
        &self,
        pushed: &PushedWork,
        grant: &GitGrant,
        fake: &FakeGitHttp,
    ) {
        let work = pushed.directory.as_path();
        // A push of another ref, a deletion, or from another old head than the
        // lease (the grant's lease is "absent": the ref now exists).
        git(work, &["commit", "-q", "--amend", "-m", "rework"]);
        let force = |target: String| {
            let (work, node, url) = (work.to_owned(), self.node.clone(), self.url.clone());
            async move { client(&work, &node, &["push", "-q", "--force", &url, &target]).await }
        };
        refused(&force("HEAD:refs/heads/main".into()).await, "403");
        refused(
            &force(format!("HEAD:refs/heads/aq/{}", Uuid::new_v4())).await,
            "403",
        );
        refused(&force(format!(":{}", grant.task_ref)).await, "403");
        refused(&force(format!("HEAD:{}", grant.task_ref)).await, "409");
        assert_eq!(
            git(&fake.repository, &["rev-parse", &grant.task_ref]),
            pushed.head
        );
        assert_eq!(
            fake.seen.lock().unwrap().len(),
            pushed.forwarded_requests + 4,
            "only advertisements"
        );
    }
    async fn check_limit(&self, c: &Case, work: &Path, grant: &GitGrant) {
        let target = format!("HEAD:{}", grant.task_ref);
        // Larger than Settings `max_push_bytes`.
        let limits = aq_role_settings::delivery::DeliveryLimits {
            max_push_bytes: 64,
            ..Default::default()
        };
        let saved = aq_role_settings::save_delivery_limits(
            aq_role_settings::Writer::new(
                &c.env.env.pool,
                c.env.actor,
                Utc::now().timestamp_millis(),
            )
            .edit(0, Uuid::new_v4()),
            &limits,
        );
        saved.await.unwrap();
        refused(
            &client(
                work,
                &self.node,
                &["push", "-q", "--force", &self.url, &target],
            )
            .await,
            "413",
        );
    }
}
fn check_forwarded(fake: &FakeGitHttp, grant: &GitGrant) -> usize {
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
        assert_eq!(all("aq-repository"), [grant.repository_id.to_string()]);
        assert_eq!(all("aq-max-push-bytes"), ["268435456"]);
        assert_eq!(all("aq-operation").len(), 1);
        assert!(all("authorization").is_empty() && all("aq-attempt").is_empty());
    }

    seen.len()
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
    let attempt = offer.offered.lifecycle_binding.attempt_id;
    check_grant(&c.env.env.pool, &offer, &grant, repository).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let router = aq_server::build_worker_router(c.env.env.state.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let node = NodeGit::new(&c, origin, attempt, &grant);
    let dir = tempfile::tempdir().unwrap();
    let pushed = node.clone_and_push(dir.path(), &fake, &grant).await;
    node.check_access(&c, attempt, dir.path()).await;
    node.check_ref_protection(&pushed, &grant, &fake).await;
    node.check_limit(&c, &pushed.directory, &grant).await;
    server.abort();
}

#[tokio::test]
async fn one_task_in_two_projects_gets_delivery_and_all_three_offers_keep_their_mode() {
    let subscriber = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("aq_refusal=debug,aq_server::profiled_execution=debug")
        .finish();
    let _trace = tracing::subscriber::set_default(subscriber);

    use super::node_slots::offered;
    let lineage = aq_selection::TaskLineage {
        project_id: Uuid::new_v4(),
        workspace_id: Uuid::new_v4(),
        task_id: Uuid::new_v4(),
    };
    let mut c = crate::common::native_offer::setup_projects(Some(lineage.clone())).await;
    let primary = c.env.lineage.project_id;
    let pilot = c.env.lineage.task_id;
    let peer = c.second_task;
    let repository = c.env.frozen.project.binding.repository_id;
    let fake = enable_pilot(&mut c, repository, &lineage).await;
    let a = offered(&c, 1).await.offered.lifecycle_binding;
    let second = offered(&c, 2).await.offered.lifecycle_binding;
    let third = offered(&c, 3).await.offered.lifecycle_binding;
    let (b, x) = if second.project_id == primary {
        (&second, &third)
    } else {
        (&third, &second)
    };
    assert_eq!((a.project_id, a.task_id), (primary, pilot));
    assert_eq!((b.project_id, b.task_id), (primary, peer));
    assert_eq!(
        (x.project_id, x.task_id),
        (lineage.project_id, lineage.task_id)
    );

    let credential = c.env.env.credential.clone();
    let original = get_grant(&c, &credential, a.attempt_id).await;
    assert_eq!(original.0, 200);
    assert_eq!(original.1["task_ref"], format!("refs/heads/aq/{pilot}"));
    assert_eq!(get_grant(&c, &credential, b.attempt_id).await.0, 404);
    assert_eq!(get_grant(&c, &credential, x.attempt_id).await.0, 404);
    assert_eq!(get_grant(&c, "invalid", a.attempt_id).await.0, 401);

    // The retained named-ref observation and outcome use the same selector.
    let pool = &c.env.env.pool;
    for (binding, expected_review, source_kind) in [
        (&a, "policy_actor", "main_sync"),
        (b, "human_owner", "reference_observation"),
        (x, "human_owner", "reference_observation"),
    ] {
        check_offer_mode(pool, binding.attempt_id, expected_review, source_kind).await;
    }
    // Rebuild the router with reversed enrollment, as after restarting with
    // updated config. Redelivery and discovery still agree with the old rows.
    configure(
        &mut c,
        &fake,
        [primary, lineage.project_id],
        Some([peer, lineage.task_id].into()),
    );
    c.env.env.state.store =
        aq_server::store::Store::connect_with((*c.env.env.pool.connect_options()).clone())
            .await
            .unwrap();
    for (slot, previous) in [(1, &a), (2, &second), (3, &third)] {
        assert_eq!(offered(&c, slot).await.offered.lifecycle_binding, *previous);
    }
    assert_eq!(get_grant(&c, &credential, a.attempt_id).await, original);
    assert_eq!(get_grant(&c, &credential, b.attempt_id).await.0, 404);
    assert_eq!(get_grant(&c, &credential, x.attempt_id).await.0, 404);
}

async fn check_offer_mode(
    pool: &sqlx::PgPool,
    attempt: Uuid,
    expected_review: &str,
    source_kind: &str,
) {
    let (outcome, source): (Vec<u8>, Vec<u8>) = sqlx::query_as(
            "SELECT o.body,s.body FROM profiled_launches l
             JOIN admission_artifacts o ON o.descriptor->'artifact'=l.execution_spec->'outcome_artifact'
             JOIN admission_artifacts s ON s.descriptor->'artifact'=l.execution_spec->'source_artifact'
             WHERE l.attempt_id=$1 LIMIT 1",
        )
        .bind(attempt)
        .fetch_one(pool)
        .await
        .unwrap();
    let decode = |bytes: &[u8]| serde_json::from_slice::<serde_json::Value>(bytes).unwrap();
    assert_eq!(decode(&outcome)["review"], expected_review);
    let source = decode(&source);
    assert!(source["evidence"].get(source_kind).is_some(), "{source}");
}

async fn enable_pilot(
    c: &mut Case,
    repository: Uuid,
    lineage: &aq_selection::TaskLineage,
) -> FakeGitHttp {
    let git_root = &c
        .env
        .env
        .state
        .source_retention
        .as_ref()
        .unwrap()
        .config
        .git_root;
    let fake = FakeGitHttp::start(git_root, repository, "refs/heads/main").await;
    let pilot = c.env.lineage.task_id;
    let primary = c.env.lineage.project_id;
    // The task list cannot opt in a task outside the project list.
    configure(c, &fake, [primary], Some([pilot, lineage.task_id].into()));
    c.mode.store(7, Ordering::SeqCst);
    sqlx::query("UPDATE worker_settings SET max_agents=3 WHERE kind='node' AND id=$1")
        .bind(c.env.env.node.to_string())
        .execute(&c.env.env.pool)
        .await
        .unwrap();
    super::node_slots::register(c, 3, None).await;
    fake
}

/// Spec 0017 §6.2 (with `project_environments`): an attempt's
/// `refs/heads/aq-try/<attempt>`, the checkpoint `aq environment try` builds.
#[cfg(feature = "project_environments")]
mod try_ref {
    use super::*;

    /// A clone of `c`'s granted repository through `/v1/git`, served by the
    /// worker router on a local port.
    struct Cloned {
        fake: FakeGitHttp,
        attempt: Uuid,
        task_ref: String,
        try_ref: String,
        url: String,
        node: Vec<String>,
        work: std::path::PathBuf,
        _dir: tempfile::TempDir,
        server: tokio::task::JoinHandle<()>,
    }
    impl Drop for Cloned {
        fn drop(&mut self) {
            self.server.abort();
        }
    }
    async fn cloned(c: &mut Case) -> Cloned {
        let repository = c.env.frozen.project.binding.repository_id;
        let git_root = (c.env.env.state.source_retention.as_ref().unwrap().config)
            .git_root
            .clone();
        let fake = FakeGitHttp::start(&git_root, repository, "refs/heads/main").await;
        let (offer, grant) = granted_offer(c, &fake).await;
        let attempt = offer.offered.lifecycle_binding.attempt_id;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let router = aq_server::build_worker_router(c.env.env.state.clone());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let node = vec![
            format!("Authorization: Bearer {}", c.env.env.credential),
            format!("AQ-Attempt: {attempt}"),
            // A client's grant header is never forwarded.
            format!("AQ-Try-Ref: refs/heads/aq-try/{}", Uuid::new_v4()),
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
        Cloned {
            fake,
            attempt,
            task_ref: grant.task_ref,
            try_ref: format!("refs/heads/aq-try/{attempt}"),
            url,
            node,
            work,
            _dir: dir,
            server,
        }
    }
    impl Cloned {
        /// A new commit on the clone's base, so two of them are siblings.
        fn commit(&self, text: &str) -> String {
            git(&self.work, &["checkout", "-q", "--detach", &self.fake.base]);
            std::fs::write(self.work.join("try.txt"), text).unwrap();
            git(&self.work, &["add", "try.txt"]);
            git(&self.work, &["commit", "-q", "-m", text]);
            git(&self.work, &["rev-parse", "HEAD"])
        }
        async fn push(&self, refspecs: &[&str]) -> std::process::Output {
            let mut args = vec!["push", "--porcelain", "--no-verify", "--force", &self.url];
            args.extend_from_slice(refspecs);
            client(&self.work, &self.node, &args).await
        }
        fn head(&self, reference: &str) -> Option<String> {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&self.fake.repository)
                .args(["rev-parse", "--verify", "-q", reference])
                .output()
                .unwrap();
            out.status
                .success()
                .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        }
        fn pushes(&self) -> usize {
            let seen = self.fake.seen.lock().unwrap();
            (seen.iter())
                .filter(|h| {
                    h.get("content-type")
                        .is_some_and(|v| v == "application/x-git-receive-pack-request")
                })
                .count()
        }
    }
    /// The owner cancels the offered attempt, which releases its task.
    async fn end(c: &Case, attempt: Uuid) {
        let state = &c.env.env.state;
        let cancel =
            crate::common::native_cancellation::cancel(state, attempt, Uuid::new_v4(), "ended");
        let (status, body) = cancel.await;
        assert_eq!(status, 200, "{body}");
        let held: bool =
            sqlx::query_scalar("SELECT holds_task FROM execution_attempts WHERE attempt_id=$1")
                .bind(attempt)
                .fetch_one(&c.env.env.pool)
                .await
                .unwrap();
        assert!(!held, "the cancelled attempt no longer holds its task");
    }
    async fn recorded(c: &Case, attempt: Uuid) -> Option<(Uuid, i64)> {
        sqlx::query_as(
            "SELECT repository_id,binding_generation FROM environment_try_refs WHERE attempt_id=$1",
        )
        .bind(attempt)
        .fetch_optional(&c.env.env.pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn an_attempt_can_force_push_its_own_try_ref() {
        let mut c = setup().await;
        let t = cloned(&mut c).await;
        assert_eq!(recorded(&c, t.attempt).await, None, "nothing pushed yet");
        // Created from nothing, then moved to a sibling (a forced update).
        let first = t.commit("first try");
        let out = t.push(&[&format!("HEAD:{}", t.try_ref)]).await;
        assert!(out.status.success(), "create: {out:?}");
        assert_eq!(t.head(&t.try_ref).as_deref(), Some(first.as_str()));
        let second = t.commit("second try");
        let out = t.push(&[&format!("HEAD:{}", t.try_ref)]).await;
        assert!(out.status.success(), "force update: {out:?}");
        assert_eq!(t.head(&t.try_ref).as_deref(), Some(second.as_str()));
        assert_eq!(t.head(&t.task_ref), None, "the task ref is untouched");
        let repository = c.env.frozen.project.binding.repository_id;
        assert_eq!(recorded(&c, t.attempt).await, Some((repository, 1)));
        // Every request reached aq-git with the server's own try ref and the
        // unchanged task ref, never the client's header.
        let seen = t.fake.seen.lock().unwrap().clone();
        for headers in &seen {
            let all = |name: &str| {
                (headers.get_all(name).iter())
                    .map(|v| v.to_str().unwrap().to_owned())
                    .collect::<Vec<_>>()
            };
            assert_eq!(all("aq-try-ref"), [t.try_ref.as_str()]);
            assert_eq!(all("aq-task-ref"), [t.task_ref.as_str()]);
        }
        // The task ref keeps its lease rule after try pushes: the grant's lease
        // is "absent", so its first push lands.
        let out = t.push(&[&format!("HEAD:{}", t.task_ref)]).await;
        assert!(out.status.success(), "task push: {out:?}");
    }

    #[tokio::test]
    async fn an_attempt_cannot_push_another_attempts_try_ref_or_relax_the_task_ref() {
        let mut c = setup().await;
        let t = cloned(&mut c).await;
        let mine = t.commit("mine");
        let out = t.push(&[&format!("HEAD:{}", t.task_ref)]).await;
        assert!(out.status.success(), "task push: {out:?}");
        let before = t.pushes();
        let other = Uuid::new_v4();
        let sibling = t.commit("sibling");
        for refspecs in [
            // Another attempt's try ref, a ref below or beside this one's.
            vec![format!("HEAD:refs/heads/aq-try/{other}")],
            vec![format!("HEAD:{}/x", t.try_ref)],
            vec![format!("HEAD:refs/heads/aq-try/{}", t.attempt.simple())],
            // Two refs at once.
            vec![
                format!("HEAD:{}", t.try_ref),
                format!("HEAD:refs/heads/aq-try/{other}"),
            ],
        ] {
            let refspecs: Vec<&str> = refspecs.iter().map(String::as_str).collect();
            refused(&t.push(&refspecs).await, "403");
        }
        // The task ref still needs its lease: a forced sibling over the
        // pushed head (old = that head, lease = absent) is stale.
        refused(&t.push(&[&format!("HEAD:{}", t.task_ref)]).await, "409");
        refused(&t.push(&[&format!(":{}", t.task_ref)]).await, "403");
        // The attempt's own try ref may not be deleted by a push.
        let out = t.push(&[&format!("HEAD:{}", t.try_ref)]).await;
        assert!(out.status.success(), "own try ref: {out:?}");
        refused(&t.push(&[&format!(":{}", t.try_ref)]).await, "403");
        assert_eq!(t.head(&t.try_ref).as_deref(), Some(sibling.as_str()));
        assert_eq!(t.head(&t.task_ref).as_deref(), Some(mine.as_str()));
        assert_eq!(t.head(&format!("refs/heads/aq-try/{other}")), None);
        assert_eq!(
            t.pushes(),
            before + 1,
            "only the own try push reached aq-git"
        );
        assert_eq!(recorded(&c, other).await, None);
        // Once the attempt no longer holds its task, no push is served.
        end(&c, t.attempt).await;
        t.commit("late");
        refused(&t.push(&[&format!("HEAD:{}", t.try_ref)]).await, "403");
    }

    #[tokio::test]
    async fn the_try_ref_is_deleted_when_the_attempt_ends() {
        let mut c = setup().await;
        let t = cloned(&mut c).await;
        t.commit("try");
        let out = t.push(&[&format!("HEAD:{}", t.try_ref)]).await;
        assert!(out.status.success(), "push: {out:?}");
        let retention = c.env.env.state.source_retention.clone().unwrap();
        let (pool, socket) = (&c.env.env.pool, &retention.config.git_socket);
        // A live attempt keeps its try ref.
        aq_environments::git::sweep(pool, socket).await;
        assert!(t.head(&t.try_ref).is_some());
        assert!(recorded(&c, t.attempt).await.is_some());
        end(&c, t.attempt).await;
        aq_environments::git::sweep(pool, socket).await;
        assert_eq!(t.head(&t.try_ref), None, "deleted once the attempt ended");
        // The record outlives a push that was in flight at the end
        // (`git_transfer_timeout_ms`); deleting an absent ref is fine.
        assert!(recorded(&c, t.attempt).await.is_some());
        aq_environments::git::sweep(pool, socket).await;
        sqlx::query(
            "UPDATE environment_try_refs SET pushed_at=now()-interval '1 day' WHERE attempt_id=$1",
        )
        .bind(t.attempt)
        .execute(pool)
        .await
        .unwrap();
        aq_environments::git::sweep(pool, socket).await;
        assert_eq!(recorded(&c, t.attempt).await, None);
        // A deletion aq-git refuses for good (the binding changed) is not
        // retried forever: the record goes, even inside the in-flight window.
        let gone = Uuid::new_v4();
        sqlx::query("INSERT INTO environment_try_refs(attempt_id,repository_id,binding_generation) VALUES($1,$2,2)")
            .bind(gone)
            .bind(c.env.frozen.project.binding.repository_id)
            .execute(pool)
            .await
            .unwrap();
        aq_environments::git::sweep(pool, socket).await;
        assert_eq!(recorded(&c, gone).await, None);
        assert_eq!(t.head(&t.task_ref), None);
        assert_eq!(
            t.head("refs/heads/main").as_deref(),
            Some(t.fake.base.as_str())
        );
    }
}
