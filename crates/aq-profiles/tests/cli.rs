mod common;
use common::*;
use std::process::Command;
fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_aq-profile"))
}
fn catalog() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/catalog")
}
#[test]
fn profile_cli_uses_distinct_binary_name() {
    let output = binary().arg("--help").output().unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("Usage: aq-profile <COMMAND>"), "{help}");
}
#[test]
fn requested_aq_profile_commands_work_without_db_and_separate_selection_pending() {
    let output = binary()
        .args(["profile", "validate", "--catalog"])
        .arg(catalog())
        .env_remove("DATABASE_URL")
        .output()
        .unwrap();
    assert!(output.status.success());
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(v["catalog_valid"], true);
    assert_eq!(v["selection_pending"], true);
    let c = seed();
    let r = request(&c);
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("request.json");
    std::fs::write(&path, serde_json::to_vec(&r).unwrap()).unwrap();
    for command in ["validate", "explain"] {
        let output = binary()
            .args(["profile", command, "--catalog"])
            .arg(catalog())
            .arg("--request")
            .arg(&path)
            .arg("--graph-access-profile-digest")
            .arg(access())
            .env_remove("DATABASE_URL")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(v["composition_valid"], true);
        assert_eq!(v["explanation"]["valid"], false);
        assert!(v["explanation"]["selection"]["selected_candidate_id"].is_null());
    }
    let mut r = r;
    r.available_adapters.clear();
    std::fs::write(&path, serde_json::to_vec(&r).unwrap()).unwrap();
    let output = binary()
        .args(["profile", "explain", "--catalog"])
        .arg(catalog())
        .arg("--request")
        .arg(&path)
        .arg("--graph-access-profile-digest")
        .arg(access())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()["composition_valid"],
        false
    );
}

#[test]
fn canonical_artifact_export_is_not_an_activation() {
    let temp = tempfile::tempdir().unwrap();
    let out = temp.path().join("catalog.json");
    let output = binary()
        .args(["profile", "validate", "--catalog"])
        .arg(catalog())
        .arg("--artifact-out")
        .arg(&out)
        .output()
        .unwrap();
    assert!(output.status.success());
    let validated =
        aq_profiles::ValidatedCatalog::from_artifact(&std::fs::read(out).unwrap()).unwrap();
    assert_eq!(validated.digest(), seed().digest());
    assert!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout)
            .unwrap()
            .get("active_digest")
            .is_none()
    );
}
#[cfg(unix)]
#[test]
fn request_fifo_refuses_without_waiting_for_a_writer() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("request.json");
    nix::unistd::mkfifo(&path, nix::sys::stat::Mode::S_IRUSR).unwrap();
    let output = binary()
        .args(["profile", "explain", "--catalog"])
        .arg(catalog())
        .arg("--request")
        .arg(path)
        .arg("--graph-access-profile-digest")
        .arg(access())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
}
