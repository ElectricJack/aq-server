//! Actual external Git owner across publication, scoped source reads and grant.
//! Runtime/material installation claims remain synthetic: no worker/model launch.
use super::*;
use aq_admission::control::{ControlReply, decode_reply};
use http_body_util::BodyExt;
use std::{path::Path, process::Stdio};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tower::ServiceExt;

async fn git(path: &Path, args: &[&str]) -> String {
    let output = tokio::process::Command::new(
        std::env::var("AQ_TEST_GIT_EXECUTABLE").expect("explicit Git executable required"),
    )
    .current_dir(path)
    .args(args)
    .env_clear()
    .env("PATH", "/usr/bin:/bin")
    .env("GIT_CONFIG_NOSYSTEM", "1")
    .env("GIT_CONFIG_GLOBAL", "/dev/null")
    .env("GIT_AUTHOR_NAME", "Fixture")
    .env("GIT_AUTHOR_EMAIL", "fixture@example.test")
    .env("GIT_COMMITTER_NAME", "Fixture")
    .env("GIT_COMMITTER_EMAIL", "fixture@example.test")
    .output()
    .await
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}
async fn rpc(socket: &Path, repo: Uuid, operation: &str, params: Value) -> Value {
    let operation_id = Uuid::new_v4();
    let body = json!({"protocol_version":1,"request_id":operation_id.to_string(),"repository_id":repo,"operation_id":operation_id,"expected_binding_generation":if operation=="repo.register" {0}else{1},"operation":operation,"params":params});
    let stream = tokio::net::UnixStream::connect(socket).await.unwrap();
    let mut reader = BufReader::new(stream);
    reader
        .get_mut()
        .write_all(format!("{body}\n").as_bytes())
        .await
        .unwrap();
    let mut line = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        reader.read_line(&mut line),
    )
    .await
    .unwrap()
    .unwrap();
    let response: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(response["ok"], true, "{response}");
    response["result"].clone()
}
struct Actual {
    prepared: Prepared,
    process: tokio::process::Child,
    socket: std::path::PathBuf,
    seed: std::path::PathBuf,
    base: String,
}
struct Unpublished {
    case: Case,
    process: tokio::process::Child,
    socket: std::path::PathBuf,
    seed: std::path::PathBuf,
    base: String,
}
async fn setup_actual_unpublished() -> Unpublished {
    setup_actual_with_prefix(false).await
}

async fn setup_actual_with_prefix(task_prefix: bool) -> Unpublished {
    let mut case = native_offer::setup().await;
    let dir = case.env.env._dir.path();
    let seed = dir.join("actual-seed");
    std::fs::create_dir(&seed).unwrap();
    git(&seed, &["init", "-b", "main"]).await;
    std::fs::write(seed.join("source"), b"actual source bytes\n").unwrap();
    git(&seed, &["add", "."]).await;
    git(&seed, &["commit", "-m", "source"]).await;
    let base = git(&seed, &["rev-parse", "HEAD"]).await;
    let socket = dir.join("actual-git.sock");
    let root = dir.join("actual-git");
    let mut process = tokio::process::Command::new(
        std::env::var("AQ_TEST_GIT_CANDIDATE").expect("explicit aq-git candidate required"),
    )
    .args([
        "--state-dir",
        root.to_str().unwrap(),
        "--git-executable",
        &std::env::var("AQ_TEST_GIT_EXECUTABLE").unwrap(),
        "--listen",
        socket.to_str().unwrap(),
        "serve",
    ])
    .kill_on_drop(true)
    .stdout(Stdio::null())
    .spawn()
    .unwrap();
    for _ in 0..200 {
        if socket.exists() {
            break;
        }
        assert!(process.try_wait().unwrap().is_none());
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(socket.exists());
    let repo = case.env.frozen.project.binding.repository_id;
    let mut binding = json!({"identity":"native-real-git-fixture","mode":"central","remote_url":seed,"allow_file_transport":true,"allowed_refs":["refs/heads/main"],"allowed_targets":["refs/heads/main"]});
    if task_prefix {
        binding["allowed_targets"] = json!([]);
        binding["allowed_target_prefixes"] = json!(["refs/heads/aq/"]);
    }
    rpc(&socket, repo, "repo.register", binding).await;
    rpc(&socket, repo, "repo.sync", json!({})).await;
    let oid = json!({"algorithm":"sha1","hex":base});
    let workspace = rpc(&socket, repo, "workspace.create", json!({"base":oid})).await;
    let checkpoint=rpc(&socket,repo,"workspace.checkpoint",json!({"workspace_id":workspace["workspace_id"],"expected_generation":1,"expected_head":oid,"quiescent":true,"mode":"head"})).await;
    let handle: Uuid = serde_json::from_value(checkpoint["retention_handle"].clone()).unwrap();
    let mut config = (*case.env.env.state.config).clone();
    let mut profiled = (**config.profiled_execution.as_ref().unwrap()).clone();
    profiled.revision += 1;
    profiled.projects[0].retention_handle = handle;
    profiled.projects[0].source = aq_admission::SourceSelection::Immutable {
        object: aq_admission::ObjectId {
            format: aq_admission::ObjectFormat::Sha1,
            hex: base.clone(),
        },
    };
    config.profiled_execution = Some(Arc::new(profiled));
    case.env.env.state.config = Arc::new(config);
    case.env.env.state.source_retention = Some(Arc::new(
        aq_server::source_retention::Runtime::new(
            aq_server::source_retention::SourceRetentionConfig {
                keep_per_task: 2,
                max_age: 30 * 86400,
                git_root: root,
                git_socket: socket.clone(),
                staging_root: dir.join("actual-staging"),
                max_staging_bytes: 4_194_304,
                max_bundle_bytes: 1_048_576,
                bulk_concurrency: 1,
                incomplete_ttl_secs: 60,
                git_deadline_ms: 30_000,
            },
        )
        .unwrap(),
    ));
    aq_server::profiled_execution::policies::install(&case.env.env.state)
        .await
        .unwrap();
    Unpublished {
        case,
        process,
        socket,
        seed,
        base,
    }
}
#[cfg(feature = "delivery_playbook")]
#[tokio::test]
#[ignore = "qualification requires an actual aq-git with followed-main retention"]
async fn unchanged_main_refreshes_at_completion_and_publishes_an_offer() {
    use aq_admission::{Label, SourceSelection};
    use aq_delivery::main_sync::{self, Scope};
    use aq_role_settings::delivery::DeliveryLimits;

    let mut f = setup_actual_with_prefix(true).await;
    let task = f.case.env.lineage.task_id;
    let project = f.case.env.lineage.project_id;
    let repo = f.case.env.frozen.project.binding.repository_id;
    let pool = &f.case.env.env.pool;
    let state = &mut f.case.env.env.state;
    state.clock = Arc::new(chrono::Utc::now);
    let config = Arc::make_mut(&mut state.config);
    config.delivery = Some(aq_delivery::github::DeliveryConfig {
        project_allowlist: [project].into(),
        task_allowlist: Some([task].into()),
        git_http_socket: Some(f.socket.with_extension("http")),
        ..Default::default()
    });
    let profiled = Arc::make_mut(config.profiled_execution.as_mut().unwrap());
    profiled.revision += 1;
    profiled.projects[0].source = SourceSelection::NamedRef {
        reference: Label::new("refs/heads/main").unwrap(),
    };
    aq_server::profiled_execution::policies::install(state)
        .await
        .unwrap();
    let now = || chrono::Utc::now().timestamp_millis() as u64;
    let limits = DeliveryLimits {
        main_freshness_ms: 1_000,
        ..Default::default()
    };
    let edit =
        aq_role_settings::Writer::new(pool, f.case.env.actor, now() as i64).edit(0, Uuid::new_v4());
    aq_role_settings::save_delivery_limits(edit, &limits)
        .await
        .unwrap();

    let scope = Scope::new(project, repo, 1, "refs/heads/main");
    let first = main_sync::MainFollower::new(pool, &f.socket, now)
        .current(&scope)
        .await
        .unwrap();
    assert_eq!(first.head, f.base);
    tokio::time::sleep(std::time::Duration::from_millis(limits.main_freshness_ms)).await;
    let poll_started_at_ms = now();
    assert!(!first.fresh(poll_started_at_ms, limits.main_freshness_ms));

    let (status, body) = native_offer::poll_offered(&f.case).await;
    // The installed baseline is expected to refuse. Close the disposable
    // daemon before asserting so the negative run drains its log pipes too.
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(f.process.id().unwrap() as i32),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    let stopped = tokio::time::timeout(std::time::Duration::from_secs(10), f.process.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(stopped.success());

    let renewed = main_sync::latest(pool, &scope).await.unwrap().unwrap();
    let page = match decode_reply(&serde_json::to_vec(&body).unwrap()).unwrap() {
        ControlReply::Poll(page) if status == 200 && page.offers.len() == 1 => page,
        _ => {
            let refusal = aq_role_settings::refusals::shown(pool, task).await;
            panic!(
                "no offer after unchanged main refresh: HTTP {status}, {body}; initial={first:?}; latest={renewed:?}; refusal={refusal:?}"
            );
        }
    };
    page.validate_for(&f.case.request).unwrap();
    let offer = &page.offers[0];
    assert_eq!(renewed.head, first.head);
    assert_eq!(renewed.retention_handle, first.retention_handle);
    assert!(
        renewed.synced_at_ms > poll_started_at_ms,
        "the real sync must complete after the offer poll begins"
    );
    let (grant_base, task_ref, observation): (String, String, Value) = sqlx::query_as(
        "SELECT g.base, g.task_ref, s.observation FROM delivery_git_grants g
         JOIN profiled_launches l USING (attempt_id)
         JOIN admission_source_observations s ON s.observation_id=l.source_observation_id
         WHERE g.attempt_id=$1",
    )
    .bind(offer.offered.lifecycle_binding.attempt_id)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(offer.offered.lifecycle_binding.task_id, task);
    assert_eq!(grant_base, f.base);
    assert_eq!(task_ref, aq_delivery::commands::task_branch(task));
    let observed_at = observation["observed_at_ms"].as_u64().unwrap();
    let observed = main_sync::ObservedHead {
        head: &grant_base,
        handle: first.retention_handle,
        observed_at_ms: observed_at,
    };
    assert!(renewed.proves(&scope, observed));
    println!(
        "followed-main offer proof base={grant_base} handle={} old_sync={} poll_start={poll_started_at_ms} new_sync={} source_observed={observed_at}",
        first.retention_handle, first.synced_at_ms, renewed.synced_at_ms
    );
}

#[cfg(feature = "delivery_playbook")]
#[tokio::test]
#[ignore = "qualification requires a delivery-enabled aq-git with source leases"]
async fn actual_git_publishes_only_the_submitted_commit_and_cleans_both_refs() {
    use aq_delivery::commands::{
        GitPort, GitTarget, RefDeletion, RefPublication, SocketGit, task_branch,
    };
    let mut f = setup_actual_with_prefix(true).await;
    let target = GitTarget {
        repository_id: f.case.env.frozen.project.binding.repository_id,
        binding_generation: 1,
    };
    let git_port = SocketGit::new(&f.socket);
    let retained = git_port
        .retain_ref(Uuid::new_v4(), &target, "refs/heads/main")
        .await
        .unwrap();
    assert_eq!(retained.head, f.base);
    let reference = task_branch(Uuid::new_v4());
    // Seed the fixture's central branch as if the worker had pushed the result.
    let pool = f
        .case
        .env
        .env
        ._dir
        .path()
        .join("actual-git/repositories")
        .join(target.repository_id.to_string())
        .join("objects.git");
    git(&pool, &["update-ref", &reference, &f.base]).await;
    let mismatch = git_port
        .publish_ref(
            Uuid::new_v4(),
            &target,
            &reference,
            None,
            Some(&"1".repeat(40)),
        )
        .await
        .unwrap();
    assert_eq!(mismatch, RefPublication::Moved);
    assert!(
        git(
            &f.seed,
            &["for-each-ref", "--format=%(objectname)", &reference]
        )
        .await
        .is_empty()
    );
    let op = Uuid::new_v4();
    let expected = RefPublication::Published {
        head: f.base.clone(),
    };
    assert_eq!(
        git_port
            .publish_ref(op, &target, &reference, None, Some(&f.base))
            .await
            .unwrap(),
        expected
    );
    assert_eq!(git(&f.seed, &["rev-parse", &reference]).await, f.base);
    assert_eq!(
        git_port
            .publish_ref(
                Uuid::new_v4(),
                &target,
                &reference,
                Some(&f.base),
                Some(&f.base)
            )
            .await
            .unwrap(),
        expected
    );
    assert_eq!(
        git_port
            .delete_ref(Uuid::new_v4(), &target, &reference, Some(&f.base))
            .await
            .unwrap(),
        RefDeletion::Deleted
    );
    assert_eq!(
        git_port
            .publish_ref(op, &target, &reference, None, Some(&f.base))
            .await
            .unwrap(),
        expected
    );
    assert_eq!(git_port.show_ref(&target, &reference).await.unwrap(), None);
    assert!(
        git(
            &f.seed,
            &["for-each-ref", "--format=%(objectname)", &reference]
        )
        .await
        .is_empty()
    );
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(f.process.id().unwrap() as i32),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    let status = tokio::time::timeout(std::time::Duration::from_secs(10), f.process.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success());
}

async fn setup_actual() -> Actual {
    let Unpublished {
        case,
        process,
        socket,
        seed,
        base,
    } = setup_actual_unpublished().await;
    let (status, body) = native_offer::poll_offered(&case).await;
    assert_eq!(status, 200, "{body}");
    let ControlReply::Poll(mut page) = decode_reply(&serde_json::to_vec(&body).unwrap()).unwrap()
    else {
        panic!("{body}")
    };
    page.validate_for(&case.request).unwrap();
    assert_eq!(page.offers.len(), 1);
    let mut prepared = setup_from(case, page.offers.remove(0)).await;
    // Actual-clock qualification: no post-Accept clock substitution or TTL stretch.
    prepared.case.env.env.state.clock = Arc::new(chrono::Utc::now);
    Actual {
        prepared,
        process,
        socket,
        seed,
        base,
    }
}
fn source_artifact<'a>(
    c: &'a Prepared,
    member: &str,
) -> &'a aq_admission::transport::ArtifactReceipt {
    c.request
        .artifacts
        .iter()
        .find(|r| {
            r.artifact
                .id
                .as_str()
                .starts_with(&format!("aq.source-{member}/"))
        })
        .unwrap()
}
async fn download(c: &Prepared, member: &str) -> Vec<u8> {
    let expected = source_artifact(c, member).bytes;
    assert!(expected > 0 && expected <= 1_048_576);
    let mut bytes = Vec::new();
    while (bytes.len() as u64) < expected {
        let response = aq_server::build_worker_router(c.case.env.env.state.clone())
            .oneshot(
                axum::http::Request::get(format!(
                    "/v1/worker/profiled/source/{}/{}?offset={}&limit=128",
                    c.request.binding.attempt_id,
                    member,
                    bytes.len()
                ))
                .header(
                    "authorization",
                    format!("Bearer {}", c.case.env.env.credential),
                )
                .header("aq-source-api-version", "1")
                .body(axum::body::Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["aq-source-api-version"], "1");
        let part = response.into_body().collect().await.unwrap().to_bytes();
        assert!(!part.is_empty());
        bytes.extend_from_slice(&part);
        assert!((bytes.len() as u64) <= expected);
    }
    bytes
}
#[tokio::test]
#[ignore = "qualification requires explicit actual aq-git and Git executable paths"]
async fn real_git_publication_download_and_initial_grant_share_exact_export() {
    let mut actual = setup_actual().await;
    let c = &actual.prepared;
    let manifest_bytes = download(c, "manifest").await;
    let bundle = download(c, "bundle").await;
    let manifest: Value = serde_json::from_slice(&manifest_bytes).unwrap();
    assert_eq!(manifest["base"]["hex"], actual.base);
    assert_eq!(manifest["source"]["hex"], actual.base);
    assert_eq!(manifest["bundle_size"], bundle.len());
    assert_eq!(manifest["bundle_sha256"], Digest::of(&bundle).as_str());
    for (name, bytes) in [("manifest", &manifest_bytes), ("bundle", &bundle)] {
        let receipt = source_artifact(c, name);
        assert_eq!(receipt.digest, Digest::of(bytes));
        assert_eq!(receipt.bytes, bytes.len() as u64);
    }
    let file = actual.seed.join("downloaded.bundle");
    std::fs::write(&file, &bundle).unwrap();
    git(&actual.seed, &["bundle", "verify", file.to_str().unwrap()]).await;
    let first = grant(c).await.unwrap();
    first
        .validate_for(&c.request, &c.plan, &c.offer, first.delivery.nonce)
        .unwrap();
    let next = grant(c).await.unwrap();
    assert_eq!(
        serde_json::to_value(&first.original).unwrap(),
        serde_json::to_value(&next.original).unwrap()
    );
    assert_eq!(first.versions, next.versions);
    assert_eq!(first.execution_deadline_ms, next.execution_deadline_ms);
    actual.process.kill().await.unwrap();
    actual.process.wait().await.unwrap();
}
#[tokio::test]
#[ignore = "qualification requires explicit actual aq-git and Git executable paths"]
async fn actual_released_export_refuses_grant_without_erasing_preparation() {
    let mut actual = setup_actual().await;
    let c = &actual.prepared;
    let manifest: Value = serde_json::from_slice(&download(c, "manifest").await).unwrap();
    let repo = c.case.env.frozen.project.binding.repository_id;
    let handle = &manifest["transfer_id"];
    let pin = rpc(
        &actual.socket,
        repo,
        "retention.inspect",
        json!({"handle_id":handle}),
    )
    .await;
    rpc(&actual.socket,repo,"retention.release",json!({"handle_id":handle,"expected_generation":pin["generation"],"refs":pin["refs"],"reason":"qualification explicit export retirement"})).await;
    assert!(grant(c).await.is_err());
    ungranted(c).await;
    actual.process.kill().await.unwrap();
    actual.process.wait().await.unwrap();
}

#[path = "actual_git/worker_assembly.rs"]
mod worker_assembly;

#[path = "actual_git/accepted_v2.rs"]
mod accepted_v2;

#[path = "actual_git/worker_refresh_assembly.rs"]
mod worker_refresh_assembly;
