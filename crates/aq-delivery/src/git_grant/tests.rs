use super::fake::{FakeGitHttp, git};
use super::*;
use axum::{
    Router,
    extract::{Request as AxRequest, State},
    http::StatusCode,
    response::IntoResponse,
};
use std::sync::Arc;

fn grant(repository: Uuid, base: &str, task: Uuid, attempt: Uuid) -> GitGrant {
    GitGrant {
        endpoint: endpoint(repository),
        repository_id: repository,
        base: base.to_owned(),
        task_ref: task_ref(task),
        lease: None,
        main_ref: "refs/heads/main".into(),
        operation_id: push_operation(attempt),
        token: None,
    }
}
/// Stands in for the server's key: any keyed digest of the message.
fn mac(attempt: Uuid, node: Uuid, repository: Uuid) -> [u8; 32] {
    use sha2::Digest;
    let mut message = b"test key\0".to_vec();
    message.extend(token_message(attempt, node, repository));
    sha2::Sha256::digest(message).into()
}

#[test]
fn a_token_names_its_attempt_and_any_change_is_refused() {
    let (attempt, node, repository) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let issued = token(attempt, &mac(attempt, node, repository));
    assert_eq!(issued.len(), 100);
    let (named, presented) = parse_token(&issued).unwrap();
    assert_eq!(named, attempt);
    assert!(same(&presented, &mac(attempt, node, repository)));
    // Another node, repository or attempt has another MAC.
    assert!(!same(&presented, &mac(attempt, Uuid::new_v4(), repository)));
    assert!(!same(&presented, &mac(attempt, node, Uuid::new_v4())));
    assert!(!same(&presented, &mac(Uuid::new_v4(), node, repository)));
    for bad in [
        String::new(),
        issued[4..].to_owned(),
        issued.to_uppercase(),
        format!("{issued}0"),
        issued.replace("aqg_", "aqp_"),
    ] {
        assert!(parse_token(&bad).is_none(), "{bad}");
    }
}

#[test]
fn grant_headers_come_from_the_grant_and_the_lease_only_when_present() {
    let (repository, task, attempt) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let mut g = grant(repository, &"a".repeat(40), task, attempt);
    g.validate().unwrap();
    let headers = grant_headers(&g, 7);
    assert!(headers.iter().all(|(n, _)| *n != "aq-task-ref-lease"));
    assert!(headers.contains(&("aq-task-ref", format!("refs/heads/aq/{task}"))));
    assert!(headers.contains(&("aq-max-push-bytes", "7".into())));
    assert!(headers.contains(&("aq-operation", push_operation(attempt).to_string())));
    g.lease = Some("b".repeat(40));
    assert!(grant_headers(&g, 7).contains(&("aq-task-ref-lease", "b".repeat(40))));
    assert_ne!(push_operation(attempt), push_operation(task));
}

#[test]
fn the_advertised_task_ref_is_the_lease() {
    let line = |s: &str| format!("{:04x}{s}", s.len() + 4);
    let (main, task) = ("1".repeat(40), "2".repeat(40));
    let body = [
        line("# service=git-upload-pack\n"),
        "0000".into(),
        line(&format!(
            "{main} refs/heads/main\0multi_ack side-band-64k\n"
        )),
        line(&format!("{task} refs/heads/aq/t\n")),
        "0000".into(),
    ]
    .concat();
    assert_eq!(
        advertised(body.as_bytes(), "refs/heads/aq/t").unwrap(),
        Some(task)
    );
    assert_eq!(
        advertised(body.as_bytes(), "refs/heads/aq/other").unwrap(),
        None
    );
    assert!(advertised(b"zz", "refs/heads/aq/t").is_err());
}

struct Proxy {
    socket: std::path::PathBuf,
    node: Uuid,
    grants: Vec<(Uuid, GitGrant)>,
}
/// The server route in miniature: token, then the recorded grant, then forward.
async fn proxy(State(p): State<Arc<Proxy>>, request: AxRequest) -> axum::response::Response {
    let presented = (request.headers().get(TOKEN_HEADER))
        .and_then(|v| v.to_str().ok())
        .and_then(parse_token);
    let Some((attempt, presented)) = presented else {
        return StatusCode::FORBIDDEN.into_response();
    };
    let repository = request.uri().path()[ROUTE_PREFIX.len()..][..36].to_owned();
    let Some((_, grant)) =
        (p.grants.iter()).find(|(a, g)| *a == attempt && g.repository_id.to_string() == repository)
    else {
        return StatusCode::FORBIDDEN.into_response();
    };
    if !same(&presented, &mac(attempt, p.node, grant.repository_id)) {
        return StatusCode::FORBIDDEN.into_response();
    }
    match forward(&p.socket, grant, &DeliveryLimits::default(), request).await {
        Ok(response) => response.map(axum::body::Body::new).into_response(),
        Err(_) => StatusCode::BAD_GATEWAY.into_response(),
    }
}

#[tokio::test]
async fn a_git_client_clones_and_pushes_the_task_ref_through_the_proxy() {
    let dir = tempfile::tempdir().unwrap();
    let (repository, task, attempt, node) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    let fake = FakeGitHttp::start(dir.path(), repository, "refs/heads/main").await;
    let g = grant(repository, &fake.base, task, attempt);
    let limits = DeliveryLimits::default();
    assert_eq!(
        observe_lease(&fake.socket, &g, &limits).await.unwrap(),
        None
    );
    let state = Arc::new(Proxy {
        socket: fake.socket.clone(),
        node,
        grants: vec![(attempt, g.clone())],
    });
    let app = Router::new()
        .route("/v1/git/{repository}/{*path}", axum::routing::any(proxy))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let issued = token(attempt, &mac(attempt, node, repository));
    let url = format!("{origin}{}", g.endpoint);
    let work = dir.path().join("work");
    let token_header = format!("http.extraHeader=AQ-Git-Token: {issued}");
    // A client that names its own grant is ignored: the server's grant counts.
    let forged = "http.extraHeader=AQ-Task-Ref: refs/heads/main";
    let lease = "http.extraHeader=AQ-Task-Ref-Lease: 0000000000000000000000000000000000000000";
    let client = |dir: &std::path::Path, args: &[&str]| {
        let mut all = vec!["-c", &token_header, "-c", forged, "-c", lease];
        all.extend_from_slice(args);
        let dir = dir.to_owned();
        let all: Vec<String> = all.into_iter().map(String::from).collect();
        tokio::task::spawn_blocking(move || {
            let all: Vec<&str> = all.iter().map(String::as_str).collect();
            git(&dir, &all)
        })
    };
    client(dir.path(), &["clone", "-q", &url, work.to_str().unwrap()])
        .await
        .unwrap();
    assert_eq!(git(&work, &["rev-parse", "HEAD"]), fake.base);
    std::fs::write(work.join("result.txt"), "the attempt's work\n").unwrap();
    git(&work, &["add", "result.txt"]);
    git(&work, &["commit", "-q", "-m", "work"]);
    let pushed = git(&work, &["rev-parse", "HEAD"]);
    let target = format!("HEAD:{}", g.task_ref);
    client(&work, &["push", "-q", "origin", &target])
        .await
        .unwrap();
    assert_eq!(git(&fake.repository, &["rev-parse", &g.task_ref]), pushed);
    assert_eq!(
        observe_lease(&fake.socket, &g, &limits).await.unwrap(),
        Some(pushed)
    );
    let seen = fake.seen.lock().unwrap().clone();
    assert!(seen.len() >= 5, "clone and push requests: {}", seen.len());
    for headers in &seen {
        let all = |name: &str| {
            (headers.get_all(name).iter())
                .map(|v| v.to_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(all("aq-task-ref"), [g.task_ref.as_str()]);
        assert_eq!(all("aq-repository"), [repository.to_string()]);
        assert_eq!(all("aq-base"), [fake.base.as_str()]);
        assert!(all("aq-task-ref-lease").is_empty());
        assert!(all(TOKEN_HEADER).is_empty() && all("authorization").is_empty());
    }
    // Another attempt's token, or this token for another repository, is refused.
    let other_attempt = token(Uuid::new_v4(), &mac(attempt, node, repository));
    let other_repository = format!("{origin}{}", endpoint(Uuid::new_v4()));
    for (presented, url) in [(&other_attempt, &url), (&issued, &other_repository)] {
        let header = format!("http.extraHeader=AQ-Git-Token: {presented}");
        let out = tokio::process::Command::new("git")
            .args(["-c", &header, "ls-remote", url])
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .await
            .unwrap();
        assert!(!out.status.success(), "{presented} at {url}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("403"),
            "{out:?}"
        );
    }
}

#[tokio::test]
async fn forwarding_outside_the_granted_repository_is_refused_before_aq_git() {
    let (repository, task, attempt) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let g = grant(repository, &"a".repeat(40), task, attempt);
    let request = Request::get(format!("/v1/git/{}/info/refs", Uuid::new_v4()))
        .body(http_body_util::Empty::<Bytes>::new())
        .unwrap();
    let missing = std::path::Path::new("/nonexistent/aq-git.sock");
    let refused = forward(missing, &g, &DeliveryLimits::default(), request).await;
    assert!(matches!(refused, Err(DeliveryError::InvalidInput(_))));
}
