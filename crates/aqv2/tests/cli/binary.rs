use super::*;

#[test]
fn binary_json_errors_and_help_are_usable() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_aqv2"))
        .env_remove("AQ_CHANNEL")
        .args(["--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    for group in [
        "tasks",
        "decisions",
        "messages",
        "chat",
        "workers",
        "projects",
        "operations",
    ] {
        assert!(help.contains(group), "{group}");
    }
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_aqv2"))
        .env_remove("AQ_CHANNEL")
        .args([
            "--origin",
            "https://aq.example",
            "--cf-access-token-file",
            "/nonexistent",
            "--json",
            "workers",
            "enroll",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(error["schema_version"], 1);
    assert_eq!(error["error"]["code"], "unsupported");
    assert!(output.stderr.is_empty());
}

#[test]
fn binary_reports_parse_and_auth_errors_as_json() {
    for args in [
        vec!["--json", "tasks", "show", "not-a-uuid"],
        vec!["--json", "projects", "list"],
        vec![
            "--json",
            "--origin",
            "http://example.test",
            "--cf-access-token-file",
            "/missing",
            "projects",
            "list",
        ],
        vec![
            "--json",
            "--origin",
            "https://example.test",
            "--cf-access-token-file",
            "/missing",
            "projects",
            "list",
        ],
    ] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_aqv2"))
            .env_remove("AQ_CHANNEL")
            .args(args)
            .output()
            .unwrap();
        assert!(!output.status.success());
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["schema_version"], 1);
        assert!(value["error"]["code"].is_string());
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn review_regression_positional_json_text_keeps_human_output() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_aqv2"))
        .env_remove("AQ_CHANNEL")
        .args(["messages", "send", "--", "--json"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(!output.stderr.is_empty());
}

#[tokio::test]
async fn local_owner_uses_loopback_without_identity_headers() {
    let mut srv = server(vec![(StatusCode::OK, json!({"items": []}))]).await;
    let origin = srv.origin.clone();
    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_aqv2"))
            .env_remove("AQ_CHANNEL")
            .args(["--local", "--origin", &origin, "--json", "projects", "list"])
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["data"], json!({"items": []}));
    let seen = srv.next().await;
    assert_eq!(seen.method, "GET");
    assert_eq!(seen.path, "/api/v1/projects");
    for header in ["cf-access-jwt-assertion", "x-aq-dev-email", "authorization"] {
        assert!(!seen.headers.contains_key(header), "{header}");
    }
}

#[test]
fn local_owner_refuses_remote_origins_and_other_auth_modes() {
    let binary = env!("CARGO_BIN_EXE_aqv2");
    for origin in ["http://example.test", "https://127.0.0.1:28080"] {
        let output = std::process::Command::new(binary)
            .env_remove("AQ_CHANNEL")
            .args(["--local", "--origin", origin, "projects", "list"])
            .output()
            .unwrap();
        assert!(!output.status.success(), "{origin}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("configuration_error"),
            "{origin}"
        );
    }

    let output = std::process::Command::new(binary)
        .env("AQ_CHANNEL", "unix:/tmp/unused-aq-test.sock")
        .args([
            "--local",
            "--origin",
            "http://127.0.0.1:28080",
            "projects",
            "list",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--local is only available on the host")
    );

    let output = std::process::Command::new(binary)
        .env_remove("AQ_CHANNEL")
        .args([
            "--local",
            "--origin",
            "http://127.0.0.1:28080",
            "--cf-access-token-file",
            "/unused",
            "projects",
            "list",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot be used with"));
}
