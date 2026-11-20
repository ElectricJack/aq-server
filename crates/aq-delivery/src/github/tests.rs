use super::*;
use axum::extract::{Request, State};
use axum::http::{HeaderMap as AxHeaders, StatusCode as AxStatus};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Duration as Span, Utc};
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use reqwest::StatusCode;
use reqwest::header::{HeaderMap, HeaderValue};
use rsa::pkcs1::EncodeRsaPrivateKey;
use rsa::pkcs8::{EncodePublicKey, LineEnding};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};

const APP_ID: u64 = 4242;

mod commands;
mod pulls;

/// (private PKCS#1 PEM, public SPKI PEM), generated once per test process.
fn keys() -> &'static (String, String) {
    static KEYS: OnceLock<(String, String)> = OnceLock::new();
    KEYS.get_or_init(|| {
        let key = rsa::RsaPrivateKey::new(&mut rand::thread_rng(), 2048).expect("keygen");
        let private = key.to_pkcs1_pem(LineEnding::LF).expect("pem").to_string();
        let public = key
            .to_public_key()
            .to_public_key_pem(LineEnding::LF)
            .expect("public pem");
        (private, public)
    })
}
fn credentials() -> AppCredentials {
    AppCredentials::from_pem(APP_ID, keys().0.as_bytes()).expect("credentials")
}
fn verify(jwt: &str, validate_exp: bool) -> AppJwtClaims {
    let mut validation = Validation::new(Algorithm::RS256);
    validation.validate_exp = validate_exp;
    validation.required_spec_claims = ["exp", "iat", "iss"].map(String::from).into();
    let key = DecodingKey::from_rsa_pem(keys().1.as_bytes()).expect("decoding key");
    let data = jsonwebtoken::decode::<AppJwtClaims>(jwt, &key, &validation).expect("verifies");
    assert_eq!(data.header.alg, Algorithm::RS256);
    data.claims
}

#[derive(Debug)]
struct Seen {
    method: String,
    path: String,
    query: Option<String>,
    headers: AxHeaders,
    body: Value,
}
type Reply = (u16, Vec<(&'static str, String)>, Value);
#[derive(Clone, Default)]
struct Mock {
    seen: Arc<Mutex<Vec<Seen>>>,
    replies: Arc<Mutex<VecDeque<Reply>>>,
}
impl Mock {
    fn reply(&self, status: u16, headers: Vec<(&'static str, String)>, body: Value) {
        self.replies
            .lock()
            .unwrap()
            .push_back((status, headers, body));
    }
    fn token(&self, token: &str, expires_at: DateTime<Utc>) {
        self.reply(
            201,
            vec![],
            json!({"token": token, "expires_at": expires_at.to_rfc3339(), "permissions": {}}),
        );
    }
    fn seen(&self) -> std::sync::MutexGuard<'_, Vec<Seen>> {
        self.seen.lock().unwrap()
    }
}
async fn handle(State(mock): State<Mock>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, 1 << 20).await.unwrap();
    mock.seen.lock().unwrap().push(Seen {
        method: parts.method.to_string(),
        path: parts.uri.path().to_string(),
        query: parts.uri.query().map(str::to_string),
        headers: parts.headers,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    });
    let reply = mock.replies.lock().unwrap().pop_front();
    let Some((status, headers, body)) = reply else {
        return (AxStatus::INTERNAL_SERVER_ERROR, "no scripted reply").into_response();
    };
    let mut response = (AxStatus::from_u16(status).unwrap(), body.to_string()).into_response();
    for (name, value) in headers {
        response.headers_mut().insert(name, value.parse().unwrap());
    }
    response
}
/// A local GitHub stand-in and an App pointed at it, with installation 77
/// for owner `Acme`.
async fn app() -> (GitHubApp, Mock) {
    let mock = Mock::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = axum::Router::new()
        .fallback(handle)
        .with_state(mock.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let client = GitHubClient::new(
        &format!("http://{addr}/api/v3"),
        std::time::Duration::from_secs(5),
    )
    .unwrap();
    let installations = [Installation {
        owner: "Acme".into(),
        installation_id: 77,
    }];
    (GitHubApp::new(client, credentials(), &installations), mock)
}

#[test]
fn jwt_claims_and_signature_verify_with_the_app_key() {
    let creds = credentials();
    let now = Utc::now();
    let claims = verify(&creds.jwt_at(now).unwrap(), true);
    assert_eq!(claims.iss, APP_ID.to_string());
    assert_eq!(claims.iat, now.timestamp() - 60, "iat is 60 s in the past");
    assert!(
        claims.exp - now.timestamp() <= 600,
        "exp at most 10 min ahead"
    );
    assert!(claims.exp > now.timestamp());
    assert_eq!(creds.claims_at(now), claims);
}

#[test]
fn jwt_from_another_key_does_not_verify() {
    let other = rsa::RsaPrivateKey::new(&mut rand::thread_rng(), 2048).unwrap();
    let pem = other.to_pkcs1_pem(LineEnding::LF).unwrap();
    let jwt = AppCredentials::from_pem(APP_ID, pem.as_bytes())
        .unwrap()
        .jwt()
        .unwrap();
    let key = DecodingKey::from_rsa_pem(keys().1.as_bytes()).unwrap();
    assert!(
        jsonwebtoken::decode::<AppJwtClaims>(&jwt, &key, &Validation::new(Algorithm::RS256))
            .is_err()
    );
}

#[test]
fn bad_key_material_is_refused_without_panicking() {
    assert_eq!(
        AppCredentials::from_pem(APP_ID, b"not a key").unwrap_err(),
        GitHubApiError::Key("private key is not an RSA PEM key")
    );
    let missing = std::path::Path::new("/nonexistent/aq-github-app.pem");
    assert_eq!(
        AppCredentials::load(APP_ID, missing).unwrap_err().code(),
        "github_key_invalid"
    );
    assert!(format!("{:?}", credentials()).contains("4242"));
}

#[tokio::test]
async fn mints_a_token_for_one_repository_with_the_documented_request() {
    let (app, mock) = app().await;
    let expires = Utc::now() + Span::hours(1);
    mock.token("ghs_first", expires);
    let token = app.installation_token("acme", "widgets").await.unwrap();
    assert_eq!(token.secret(), "ghs_first");
    assert_eq!(token.installation_id, 77);
    assert_eq!(token.expires_at.timestamp(), expires.timestamp());
    assert!(!format!("{token:?}").contains("ghs_first"), "Debug redacts");

    let seen = mock.seen();
    assert_eq!(seen.len(), 1);
    let r = &seen[0];
    assert_eq!(r.method, "POST");
    assert_eq!(r.path, "/api/v3/app/installations/77/access_tokens");
    assert_eq!(r.body, json!({"repositories": ["widgets"]}));
    let header = |name: &str| r.headers.get(name).unwrap().to_str().unwrap().to_string();
    assert_eq!(header("accept"), "application/vnd.github+json");
    assert_eq!(header("x-github-api-version"), "2022-11-28");
    assert_eq!(header("user-agent"), "aq-server");
    assert_eq!(header("content-type"), "application/json");
    let jwt = header("authorization")
        .strip_prefix("Bearer ")
        .unwrap()
        .to_string();
    assert_eq!(verify(&jwt, true).iss, "4242");
}

#[tokio::test]
async fn installation_token_authenticates_api_calls() {
    let (app, mock) = app().await;
    mock.token("ghs_api", Utc::now() + Span::hours(1));
    mock.reply(200, vec![], json!({"full_name": "Acme/widgets"}));
    let token = app.installation_token("Acme", "widgets").await.unwrap();
    let repo: Value = app
        .client()
        .send(
            reqwest::Method::GET,
            "/repos/Acme/widgets",
            Auth::Installation(&token),
            None,
        )
        .await
        .unwrap();
    assert_eq!(repo["full_name"], "Acme/widgets");
    let seen = mock.seen();
    assert_eq!(seen[1].path, "/api/v3/repos/Acme/widgets");
    assert_eq!(seen[1].headers["authorization"], "Bearer ghs_api");
    assert_eq!(seen[1].headers["x-github-api-version"], "2022-11-28");
}

#[tokio::test]
async fn cached_token_is_reused_per_repository() {
    let (app, mock) = app().await;
    let expires = Utc::now() + Span::hours(1);
    mock.token("ghs_a", expires);
    mock.token("ghs_b", expires);
    let first = app.installation_token("Acme", "widgets").await.unwrap();
    let again = app.installation_token("ACME", "Widgets").await.unwrap();
    assert_eq!(first, again, "same repository reuses the cached token");
    assert_eq!(mock.seen().len(), 1);
    let other = app.installation_token("Acme", "gadgets").await.unwrap();
    assert_eq!(
        other.secret(),
        "ghs_b",
        "another repository gets its own token"
    );
    assert_eq!(mock.seen().len(), 2);
    assert_eq!(mock.seen()[1].body, json!({"repositories": ["gadgets"]}));
}

#[tokio::test]
async fn token_within_60s_of_expiry_is_refreshed() {
    let (app, mock) = app().await;
    mock.token("ghs_old", Utc::now() + Span::seconds(30));
    mock.token("ghs_new", Utc::now() + Span::hours(1));
    assert_eq!(
        app.installation_token("Acme", "widgets")
            .await
            .unwrap()
            .secret(),
        "ghs_old"
    );
    let refreshed = app.installation_token("Acme", "widgets").await.unwrap();
    assert_eq!(refreshed.secret(), "ghs_new");
    assert_eq!(mock.seen().len(), 2);
    // The refreshed token is now cached.
    app.installation_token("Acme", "widgets").await.unwrap();
    assert_eq!(mock.seen().len(), 2);
}

#[tokio::test]
async fn mint_failures_map_to_typed_errors_and_are_not_cached() {
    let (app, mock) = app().await;
    let reset = Utc::now().timestamp() + 900;
    mock.reply(
        403,
        vec![
            ("x-ratelimit-remaining", "0".into()),
            ("x-ratelimit-reset", reset.to_string()),
        ],
        json!({"message": "API rate limit exceeded"}),
    );
    mock.reply(404, vec![], json!({"message": "Not Found"}));
    mock.reply(
        422,
        vec![],
        json!({"message": "Validation Failed", "errors": [{"message": "repository not in installation"}]}),
    );
    mock.reply(429, vec![("retry-after", "30".into())], json!({}));
    mock.token("ghs_ok", Utc::now() + Span::hours(1));

    let err = app.installation_token("Acme", "widgets").await.unwrap_err();
    assert_eq!(
        err,
        GitHubApiError::Retryable {
            reset_at: DateTime::from_timestamp(reset, 0)
        }
    );
    assert_eq!(err.code(), "retryable");
    let err = app.installation_token("Acme", "widgets").await.unwrap_err();
    assert_eq!(err, GitHubApiError::NotFound);
    let err = app.installation_token("Acme", "widgets").await.unwrap_err();
    assert_eq!(
        err,
        GitHubApiError::Invalid {
            message: "Validation Failed; repository not in installation".into()
        }
    );
    let before = Utc::now();
    let GitHubApiError::Retryable { reset_at: Some(at) } =
        app.installation_token("Acme", "widgets").await.unwrap_err()
    else {
        panic!("429 is retryable with a reset time");
    };
    assert!(at >= before + Span::seconds(30) && at <= Utc::now() + Span::seconds(30));
    let ok = app.installation_token("Acme", "widgets").await.unwrap();
    assert_eq!(ok.secret(), "ghs_ok");
    assert_eq!(mock.seen().len(), 5);
}

#[test]
fn error_statuses_map_onto_github_api_errors() {
    let now = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
    let headers = |pairs: &[(&'static str, &str)]| {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    };
    let body = |m: &str| json!({ "message": m }).to_string().into_bytes();
    let map = |status: u16, h: &HeaderMap, b: &[u8]| {
        client::map_error(StatusCode::from_u16(status).unwrap(), h, b, now)
    };
    // Secondary limit: retry-after wins over x-ratelimit-reset.
    assert_eq!(
        map(
            403,
            &headers(&[("retry-after", "120"), ("x-ratelimit-reset", "1")]),
            &body("You have exceeded a secondary rate limit")
        ),
        GitHubApiError::Retryable {
            reset_at: Some(now + Span::seconds(120))
        }
    );
    // Secondary limit without any reset header.
    assert_eq!(
        map(
            403,
            &HeaderMap::new(),
            &body("You have exceeded a secondary rate limit")
        ),
        GitHubApiError::Retryable { reset_at: None }
    );
    // A permission 403 is not a rate limit.
    assert_eq!(
        map(
            403,
            &headers(&[("x-ratelimit-remaining", "4999")]),
            &body("Resource not accessible by integration")
        ),
        GitHubApiError::Status {
            status: 403,
            message: "Resource not accessible by integration".into()
        }
    );
    assert_eq!(
        map(429, &HeaderMap::new(), b""),
        GitHubApiError::Retryable { reset_at: None }
    );
    assert_eq!(map(404, &HeaderMap::new(), b""), GitHubApiError::NotFound);
    assert_eq!(
        map(422, &HeaderMap::new(), &body("Reference already exists")),
        GitHubApiError::Invalid {
            message: "Reference already exists".into()
        }
    );
    assert_eq!(
        map(401, &HeaderMap::new(), &body("Bad credentials")),
        GitHubApiError::Status {
            status: 401,
            message: "Bad credentials".into()
        }
    );
}

#[tokio::test]
async fn unconfigured_and_unknown_owner_are_refused() {
    let err = GitHubApp::from_config(None).err().unwrap();
    assert_eq!(err, GitHubApiError::Unconfigured);
    assert_eq!(err.code(), "github_unconfigured");
    let (app, mock) = app().await;
    let err = app
        .installation_token("other", "widgets")
        .await
        .unwrap_err();
    assert_eq!(err.code(), "github_no_installation");
    for (owner, repo) in [
        ("Acme", "../x"),
        ("Acme", ""),
        ("-bad", "widgets"),
        ("a/b", "c"),
    ] {
        assert_eq!(
            app.installation_token(owner, repo)
                .await
                .unwrap_err()
                .code(),
            "github_invalid_input"
        );
    }
    assert!(mock.seen().is_empty());
}

fn config(key: std::path::PathBuf) -> GitHubAppConfig {
    GitHubAppConfig {
        app_id: APP_ID,
        private_key_file: key,
        installations: vec![Installation {
            owner: "Acme".into(),
            installation_id: 77,
        }],
        api_base: default_api_base(),
        request_timeout_secs: default_request_timeout_secs(),
    }
}

#[test]
fn from_config_reads_the_key_file() {
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("app.pem");
    std::fs::write(&key, &keys().0).unwrap();
    let app = GitHubApp::from_config(Some(&config(key))).unwrap();
    assert_eq!(app.client().base().as_str(), "https://api.github.com/");
    assert_eq!(app.installation_id("acme").unwrap(), 77);
    let missing = config(dir.path().join("absent.pem"));
    assert_eq!(
        GitHubApp::from_config(Some(&missing)).err().unwrap().code(),
        "github_key_invalid"
    );
}

#[test]
fn config_validation() {
    let ok = config("/vault/github-app.pem".into());
    ok.validate().unwrap();
    let mut c = ok.clone();
    c.app_id = 0;
    assert!(c.validate().is_err());
    let mut c = ok.clone();
    c.private_key_file = "relative.pem".into();
    assert!(c.validate().is_err());
    let mut c = ok.clone();
    c.request_timeout_secs = 0;
    assert!(c.validate().is_err());
    for base in [
        "ftp://x",
        "https://user:pw@api.github.com",
        "https://api.github.com/?q=1",
        "not a url",
    ] {
        let mut c = ok.clone();
        c.api_base = base.into();
        assert!(c.validate().is_err(), "{base}");
    }
    let mut c = ok.clone();
    c.api_base = "https://ghe.example/api/v3".into();
    c.validate().unwrap();
    let mut c = ok.clone();
    c.installations.push(Installation {
        owner: "ACME".into(),
        installation_id: 78,
    });
    assert!(c.validate().is_err(), "duplicate owner");
    let mut c = ok.clone();
    c.installations[0].installation_id = 0;
    assert!(c.validate().is_err());
}

#[test]
fn delivery_section_deserializer_validates() {
    let parse = |v: Value| deserialize(v);
    assert_eq!(parse(json!({})).unwrap(), Some(DeliveryConfig::default()));
    let good = json!({"github": {"app_id": 1, "private_key_file": "/k.pem",
        "installations": [{"owner": "acme", "installation_id": 2}]}});
    let github = parse(good).unwrap().unwrap().github.unwrap();
    assert_eq!(github.api_base, "https://api.github.com");
    let bad = json!({"github": {"app_id": 1, "private_key_file": "k.pem", "installations": []}});
    let err = parse(bad).unwrap_err().to_string();
    assert!(err.starts_with("delivery.github: "), "{err}");
    assert!(
        parse(json!({"gitlab": {}})).is_err(),
        "unknown keys refused"
    );
    let socket = parse(json!({"git_http_socket": "/run/aq-git/git-http.sock"})).unwrap();
    assert_eq!(
        socket.unwrap().git_http_socket.unwrap(),
        PathBuf::from("/run/aq-git/git-http.sock")
    );
    for bad in ["git-http.sock", "/run/../git-http.sock"] {
        assert!(parse(json!({"git_http_socket": bad})).is_err(), "{bad}");
    }
}

#[test]
fn delivery_task_allowlist_narrows_enrolled_projects() {
    let project = Uuid::new_v4();
    let task = Uuid::new_v4();
    let parse = |tasks: Option<Vec<Uuid>>| {
        let mut config = json!({
            "project_allowlist": [project],
            "git_http_socket": "/run/aq-git/git-http.sock"
        });
        if let Some(tasks) = tasks {
            config["task_allowlist"] = json!(tasks);
        }
        deserialize(config).map(Option::unwrap)
    };
    assert!(parse(None).unwrap().enrolls(project, Uuid::new_v4()));
    let pilot = parse(Some(vec![task])).unwrap();
    assert!(pilot.enrolls(project, task));
    assert!(!pilot.enrolls(project, Uuid::new_v4()));
    assert!(!pilot.enrolls(Uuid::new_v4(), task));
    assert!(!parse(Some(vec![])).unwrap().has_enrollment());
    assert!(parse(Some(vec![Uuid::nil()])).is_err());
    assert!(deserialize(json!({"project_allowlist": [project]})).is_err());
}
