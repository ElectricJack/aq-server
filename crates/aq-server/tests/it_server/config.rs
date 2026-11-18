use aq_server::config::{Environment, load};
use std::path::Path;

fn vault_with(toml: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), toml).unwrap();
    dir
}

const VALID: &str = r#"
format_version = 1
environment = "development"

[server]
admin_bind = "0.0.0.0:8080"
worker_bind = "0.0.0.0:8081"
public_origin = "http://127.0.0.1:8080"

[auth]
mode = "dev"
dev_email = "owner@localhost"

[bootstrap]
enabled = false
"#;

#[test]
fn standalone_reviews_require_retained_private_keys_without_playbooks() {
    use std::os::unix::fs::PermissionsExt;
    let dir = vault_with(VALID);
    let key = dir.path().join("review-key");
    std::fs::write(&key, [32u8; 32]).unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    let text = format!(
        "{VALID}\n[reviews.protection]\nactive_key_id = \"fixture\"\n[[reviews.protection.keys]]\nid = \"fixture\"\nfile = {key:?}\n"
    );
    std::fs::write(dir.path().join("config.toml"), &text).unwrap();
    let config = load(dir.path()).unwrap();
    assert!(config.reviews.is_some());
    assert!(config.playbooks.is_none());
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(load(dir.path()).is_err());
    std::fs::remove_file(key).unwrap();
    assert!(load(dir.path()).is_err());
}

#[test]
fn enabled_profiled_execution_requires_terminal_keys_at_startup() {
    let policy = r#"
[profiled_execution]
revision = 1
qualified_runtimes = []
[[profiled_execution.projects]]
project_id = "00000000-0000-0000-0000-000000000001"
enabled = true
priority = 0
retention_handle = "00000000-0000-0000-0000-000000000002"
source = { kind = "named_ref", reference = "refs/heads/main" }
"#;
    let dir = vault_with(&format!("{VALID}\n{policy}"));
    let error = load(dir.path()).unwrap_err().to_string();
    assert!(
        error.contains("terminals") && error.contains("enabled projects"),
        "{error}"
    );
    // Disabled admission and a server without profiled execution remain valid.
    assert!(
        load(
            vault_with(&format!(
                "{VALID}\n{}",
                policy.replace("enabled = true", "enabled = false")
            ))
            .path()
        )
        .is_ok()
    );
    use std::os::unix::fs::PermissionsExt;
    let key = dir.path().join("terminal-key");
    std::fs::write(&key, [21u8; 32]).unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    let terminals = format!(
        "\n[terminals.protection]\nactive_key_id = \"fixture\"\n[[terminals.protection.keys]]\nid = \"fixture\"\nfile = {:?}\n",
        key
    );
    std::fs::write(
        dir.path().join("config.toml"),
        format!("{VALID}\n{policy}\n{terminals}"),
    )
    .unwrap();
    assert!(load(dir.path()).unwrap().terminals.is_some());
}

#[test]
fn loads_example_vault() {
    let cfg = load(Path::new("../../vault.example")).unwrap();
    assert_eq!(cfg.environment, Environment::Development);
    assert_eq!(cfg.enrollment.code_ttl_secs, 600);
    assert_eq!(cfg.workers.heartbeat_interval_secs, 15);
    assert_eq!(cfg.workers.offline_after_secs, 60);
}

#[test]
fn loads_minimal_valid_config_with_defaults() {
    let dir = vault_with(VALID);
    let cfg = load(dir.path()).unwrap();
    assert_eq!(cfg.enrollment.code_ttl_secs, 600);
}

#[test]
fn rejects_dev_auth_in_production() {
    let dir = vault_with(
        &VALID
            .replace("development", "production")
            .replace("http://", "https://"),
    );
    let err = load(dir.path()).unwrap_err().to_string();
    assert!(err.contains("auth.mode"), "{err}");
    assert!(err.contains("cannot be enabled in production"), "{err}");
}

#[test]
fn rejects_non_https_origin_in_production() {
    let toml = r#"
format_version = 1
environment = "production"
[server]
admin_bind = "0.0.0.0:8080"
worker_bind = "0.0.0.0:8081"
public_origin = "http://aq.example.com"
[auth]
mode = "cloudflare_access"
team_domain = "https://team.cloudflareaccess.com"
audience = "abc"
[bootstrap]
enabled = false
"#;
    let dir = vault_with(toml);
    let err = load(dir.path()).unwrap_err().to_string();
    assert!(err.contains("server.public_origin"), "{err}");
}

#[test]
fn rejects_non_https_team_domain() {
    let toml = r#"
format_version = 1
environment = "development"
[server]
admin_bind = "0.0.0.0:8080"
worker_bind = "0.0.0.0:8081"
public_origin = "http://127.0.0.1:8080"
[auth]
mode = "cloudflare_access"
team_domain = "team.cloudflareaccess.com"
audience = "abc"
[bootstrap]
enabled = false
"#;
    let dir = vault_with(toml);
    let err = load(dir.path()).unwrap_err().to_string();
    assert!(err.contains("auth.team_domain"), "{err}");
}

/// Spec 0014 removed two keys. A config that still has one is refused with
/// a message that names the key and says what removed it, whatever else the
/// section holds (the typed parse would only say "unknown field").
#[test]
fn a_key_removed_by_spec_0014_is_refused_by_name() {
    let dir = vault_with(&format!(
        "{VALID}\n[profiled_execution]\nrevision = 1\nqualified_runtimes = []\nprojects = []\ncontext_qualifications = []\n"
    ));
    let path = dir.path().join("config.toml").display().to_string();
    assert_eq!(
        load(dir.path()).unwrap_err().to_string(),
        format!(
            "invalid config {path}: profiled_execution.context_qualifications: was removed by \
             spec 0014 (roles and profiles): the server holds no context measurement; delete it"
        )
    );
    let dir = vault_with(&format!(
        "{VALID}\n[intelligence]\nrevision = 1\n\n[[intelligence.qualified_harnesses]]\nharness = \"codex\"\nmodels = []\n"
    ));
    let path = dir.path().join("config.toml").display().to_string();
    assert_eq!(
        load(dir.path()).unwrap_err().to_string(),
        format!(
            "invalid config {path}: intelligence.qualified_harnesses[].models: was removed by \
             spec 0014 (roles and profiles): any model an intelligence profile names is \
             allowed; delete it"
        )
    );
    // The same sections without the removed keys load.
    let dir = vault_with(&format!(
        "{VALID}\n[intelligence]\nrevision = 1\n\n[profiled_execution]\nrevision = 1\nqualified_runtimes = []\nprojects = []\n"
    ));
    let config = load(dir.path()).unwrap();
    assert!(config.profiled_execution.is_some() && config.intelligence.is_some());
}

#[test]
fn rejects_unknown_key() {
    let dir = vault_with(&VALID.replace("[auth]", "admni_bind = \"0.0.0.0:1\"\n\n[auth]"));
    let err = load(dir.path()).unwrap_err().to_string();
    assert!(err.contains("admni_bind"), "{err}");
    assert!(
        err.contains(&dir.path().join("config.toml").display().to_string()),
        "{err}"
    );
}

#[test]
fn rejects_missing_required_key() {
    let dir = vault_with(&VALID.replace("public_origin = \"http://127.0.0.1:8080\"\n", ""));
    let err = load(dir.path()).unwrap_err().to_string();
    assert!(err.contains("public_origin"), "{err}");
}

#[test]
fn rejects_future_format_version() {
    let dir = vault_with(&VALID.replace("format_version = 1", "format_version = 2"));
    let err = load(dir.path()).unwrap_err().to_string();
    assert!(err.contains("unsupported format_version 2"), "{err}");
    assert!(err.contains("format_version"), "{err}");
}

#[test]
fn rejects_offline_not_greater_than_heartbeat() {
    let dir = vault_with(&format!(
        "{VALID}\n[workers]\nheartbeat_interval_secs = 30\noffline_after_secs = 30\n"
    ));
    let err = load(dir.path()).unwrap_err().to_string();
    assert!(err.contains("workers.offline_after_secs"), "{err}");
}

#[test]
fn missing_file_names_path() {
    let dir = tempfile::tempdir().unwrap();
    let err = load(dir.path()).unwrap_err().to_string();
    assert!(
        err.contains(&dir.path().join("config.toml").display().to_string()),
        "{err}"
    );
}

#[test]
fn bootstrap_enabled_requires_email() {
    let dir = vault_with(&VALID.replace("enabled = false", "enabled = true"));
    let err = load(dir.path()).unwrap_err().to_string();
    assert!(err.contains("bootstrap.owner_email"), "{err}");
}

#[test]
fn rejects_zero_durations() {
    for (section, key) in [
        ("enrollment", "code_ttl_secs"),
        ("workers", "heartbeat_interval_secs"),
        ("workers", "offline_after_secs"),
    ] {
        let dir = vault_with(&format!("{VALID}\n[{section}]\n{key} = 0\n"));
        let err = load(dir.path()).unwrap_err().to_string();
        assert!(err.contains(key) && err.contains("at least 1"), "{err}");
    }
}

#[test]
fn updates_section_is_optional_strict_and_absolute() {
    let cfg = load(vault_with(VALID).path()).unwrap();
    assert!(cfg.updates.is_none());
    let ok = format!("{VALID}\n[updates]\nspool_dir = \"/updater\"\n");
    let cfg = load(vault_with(&ok).path()).unwrap();
    assert_eq!(cfg.updates.unwrap().spool_dir, Path::new("/updater"));
    let unknown = format!("{VALID}\n[updates]\nspool_dir = \"/u\"\nimage = \"x\"\n");
    assert!(load(vault_with(&unknown).path()).is_err());
    let rel = format!("{VALID}\n[updates]\nspool_dir = \"updater\"\n");
    let err = load(vault_with(&rel).path()).unwrap_err().to_string();
    assert!(err.contains("updates.spool_dir"), "{err}");
}

#[test]
fn internal_work_section_is_optional_strict_and_defaulted() {
    let cfg = load(vault_with(VALID).path()).unwrap();
    assert!(cfg.internal_work.is_none());
    let empty = format!("{VALID}\n[internal_work]\n");
    let section = load(vault_with(&empty).path())
        .unwrap()
        .internal_work
        .unwrap();
    assert!(section.api.is_empty() && section.enabled && section.poll_ms == 200);
    let full = format!(
        "{VALID}\n[internal_work]\npoll_ms = 50\nenabled = false\napi = [{{provider = \"openai\", endpoint = \"https://api.example/v1/responses\", secret_file = \"/run/key\"}}]\n"
    );
    let section = load(vault_with(&full).path())
        .unwrap()
        .internal_work
        .unwrap();
    assert_eq!((section.poll_ms, section.enabled), (50, false));
    assert_eq!(section.api[0].provider, "openai");
    let unknown = format!("{VALID}\n[internal_work]\nworkers = 2\n");
    assert!(load(vault_with(&unknown).path()).is_err());
    let nested = format!(
        "{VALID}\n[internal_work]\napi = [{{provider = \"o\", endpoint = \"e\", secret_file = \"/k\", model = \"m\"}}]\n"
    );
    assert!(load(vault_with(&nested).path()).is_err());
}

#[test]
fn playbook_authoring_section_is_optional_strict_and_bounded() {
    let cfg = load(vault_with(VALID).path()).unwrap();
    assert_eq!(cfg.playbook_authoring, Default::default());
    assert_eq!(cfg.playbook_authoring.keep_finished_runs, 50);
    let set =
        format!("{VALID}\n[playbook_authoring]\nkeep_finished_runs = 5\ndeadline_ms = 600000\n");
    let section = load(vault_with(&set).path()).unwrap().playbook_authoring;
    assert_eq!(
        (section.keep_finished_runs, section.deadline_ms),
        (5, 600_000)
    );
    assert_eq!(section.max_turns, 200);
    let unknown = format!("{VALID}\n[playbook_authoring]\nworkers = 2\n");
    assert!(load(vault_with(&unknown).path()).is_err());
    // A deadline internal work would refuse is refused at load.
    let long = format!("{VALID}\n[playbook_authoring]\ndeadline_ms = 1800001\n");
    let err = load(vault_with(&long).path()).unwrap_err().to_string();
    assert!(err.contains("playbook_authoring"), "{err}");
    let none = format!("{VALID}\n[playbook_authoring]\nkeep_finished_runs = 0\n");
    assert!(load(vault_with(&none).path()).is_err());
}

#[cfg(feature = "delivery_playbook")]
#[test]
fn delivery_github_section_is_optional_strict_and_validated() {
    let cfg = load(vault_with(VALID).path()).unwrap();
    assert!(cfg.delivery.is_none());
    // Absent section: the adapter is unavailable, not a panic.
    let err = aq_delivery::github::GitHubApp::from_config(None)
        .err()
        .unwrap();
    assert_eq!(err.code(), "github_unconfigured");
    let empty = format!("{VALID}\n[delivery]\n");
    assert!(
        load(vault_with(&empty).path())
            .unwrap()
            .delivery
            .unwrap()
            .github
            .is_none()
    );

    let section = r#"
[delivery.github]
app_id = 12345
private_key_file = "/vault/github-app.pem"
installations = [{ owner = "acme", installation_id = 678 }, { owner = "other-org", installation_id = 9 }]
"#;
    let github = load(vault_with(&format!("{VALID}\n{section}")).path())
        .unwrap()
        .delivery
        .unwrap()
        .github
        .unwrap();
    assert_eq!(github.app_id, 12345);
    assert_eq!(github.private_key_file, Path::new("/vault/github-app.pem"));
    assert_eq!(github.api_base, "https://api.github.com");
    assert_eq!(github.installations.len(), 2);
    assert_eq!(
        (
            github.installations[0].owner.as_str(),
            github.installations[0].installation_id
        ),
        ("acme", 678)
    );
    let base = format!("{VALID}\n{section}api_base = \"https://ghe.example/api/v3\"\n");
    let github = load(vault_with(&base).path())
        .unwrap()
        .delivery
        .unwrap()
        .github
        .unwrap();
    assert_eq!(github.api_base, "https://ghe.example/api/v3");

    let unknown = format!("{VALID}\n{section}token = \"x\"\n");
    assert!(load(vault_with(&unknown).path()).is_err());
    let relative = section.replace("/vault/github-app.pem", "github-app.pem");
    let err = load(vault_with(&format!("{VALID}\n{relative}")).path())
        .unwrap_err()
        .to_string();
    assert!(err.contains("delivery.github"), "{err}");
    let duplicate = section.replace("other-org", "ACME");
    assert!(load(vault_with(&format!("{VALID}\n{duplicate}")).path()).is_err());
}
