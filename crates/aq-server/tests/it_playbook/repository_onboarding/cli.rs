use super::*;

fn cli_vault(o: &Onboarding, github: &str) -> tempfile::TempDir {
    let vault = tempfile::tempdir().unwrap();
    let fixture = o._dir.path();
    let staging = fixture.join("staging");
    let git = fixture.join("git");
    let socket = fixture.join("aq-git.sock");
    let config = include_str!("../../../../../vault.example/config.toml");
    std::fs::write(
        vault.path().join("config.toml"),
        format!(
            "{config}
[source_retention]
keep_per_task = 2
max_age = 2592000
staging_root = {staging:?}
git_root = {git:?}
git_socket = {socket:?}
max_staging_bytes = 1048576
max_bundle_bytes = 1048576
bulk_concurrency = 1
incomplete_ttl_secs = 60
git_deadline_ms = 30000

{github}"
        ),
    )
    .unwrap();
    vault
}

async fn cli_adopt(
    o: &Onboarding,
    vault: &std::path::Path,
    request: &Value,
) -> std::process::Output {
    let mut db_url = reqwest::Url::parse(&std::env::var("DATABASE_URL").unwrap()).unwrap();
    db_url.set_path(o.env._db.options().get_database().unwrap());
    let mut file = tempfile::NamedTempFile::new_in(vault).unwrap();
    serde_json::to_writer(&mut file, request).unwrap();
    tokio::process::Command::new(env!("CARGO_BIN_EXE_aq-server"))
        .kill_on_drop(true)
        .env_remove("AQ_DEPLOYMENT")
        .env("DATABASE_URL", db_url.as_str())
        .arg("adopt-repository")
        .arg("--vault")
        .arg(vault)
        .args(["--owner-email", "owner@localhost", "--request"])
        .arg(file.path())
        .output()
        .await
        .unwrap()
}

fn cli_json(output: std::process::Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[tokio::test]
async fn concurrent_owner_cli_processes_and_http_replay_the_same_operation() {
    let o = onboarding().await;
    bind(&o.env).await;
    o.fake.lock().unwrap().remote = "https://github.com/acme/widgets.git".into();
    let vault = cli_vault(
        &o,
        &format!("[onboarding]\ngithub_api_url={:?}", o.api_base),
    );
    let op = Uuid::new_v4();
    let request = adopt_request(op);
    let arrived = Arc::new(Notify::new());
    let proceed = Arc::new(Notify::new());
    o.fake.lock().unwrap().rules_gate = Some((arrived.clone(), proceed.clone()));
    let retry = async {
        tokio::time::timeout(std::time::Duration::from_secs(5), arrived.notified())
            .await
            .expect("first CLI reached GitHub");
        let result = cli_adopt(&o, vault.path(), &request).await;
        proceed.notify_one();
        result
    };
    let (left, right) = tokio::join!(cli_adopt(&o, vault.path(), &request), retry);
    let answer = cli_json(left);
    assert_eq!(
        (answer["operation_id"].clone(), cli_json(right)),
        (json!(op), answer.clone())
    );
    assert_eq!(
        (adopt(&o, request).await, rows(&o).await),
        ((StatusCode::OK, answer), 1)
    );
}

#[tokio::test]
async fn competing_adoption_operations_cannot_both_record_the_same_generation() {
    let o = onboarding().await;
    bind(&o.env).await;
    let arrived = Arc::new(Notify::new());
    let proceed = Arc::new(Notify::new());
    {
        let mut f = o.fake.lock().unwrap();
        f.remote = "https://github.com/acme/widgets.git".into();
        f.rules_gate = Some((arrived.clone(), proceed.clone()));
    }
    let winner = Uuid::new_v4();
    let competing = async {
        tokio::time::timeout(std::time::Duration::from_secs(5), arrived.notified())
            .await
            .expect("adoption reached GitHub");
        let result = adopt(&o, adopt_request(winner)).await;
        proceed.notify_one();
        result
    };
    let (first, second) = tokio::join!(adopt(&o, adopt_request(Uuid::new_v4())), competing);
    assert_eq!(
        (first.0, second.0),
        (StatusCode::CONFLICT, StatusCode::OK),
        "{first:?} {second:?}"
    );
    assert_eq!(
        (second.1["operation_id"].clone(), rows(&o).await),
        (json!(winner), 1)
    );
}

#[cfg(feature = "delivery_playbook")]
#[tokio::test]
async fn owner_cli_uses_the_configured_repository_scoped_github_app() {
    use rsa::pkcs1::{EncodeRsaPrivateKey, LineEnding};
    let o = onboarding().await;
    bind(&o.env).await;
    o.fake.lock().unwrap().remote = "https://github.com/acme/widgets.git".into();
    let key = o._dir.path().join("app.pem");
    let rsa = rsa::RsaPrivateKey::new(&mut rand::thread_rng(), 2048).unwrap();
    std::fs::write(&key, rsa.to_pkcs1_pem(LineEnding::LF).unwrap().as_bytes()).unwrap();
    let vault = cli_vault(
        &o,
        &format!(
            r#"
[delivery.github]
app_id = 4242
private_key_file = {key:?}
api_base = {api:?}
[[delivery.github.installations]]
owner = "acme"
installation_id = 42
"#,
            api = o.api_base
        ),
    );
    let answer = cli_json(cli_adopt(&o, vault.path(), &adopt_request(Uuid::new_v4())).await);
    assert_eq!(answer["default_branch"], "trunk");
    assert_eq!(
        o.fake.lock().unwrap().metadata_auth,
        vec![Some(format!("Bearer {APP_TOKEN}"))]
    );
    assert!(!answer.to_string().contains(APP_TOKEN));
}
