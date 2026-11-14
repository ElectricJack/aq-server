use std::{
    fs,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};
fn cli(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_aq-playbooks"))
        .args(args)
        .output()
        .unwrap()
}
#[test]
fn offline_cli_round_trip_and_invalid_input_exit() {
    let dir = std::env::temp_dir().join(format!(
        "aq-playbooks-cli-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let path = dir.to_str().unwrap();
    let output = cli(&["examples", "--output", path]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let proposal = dir.join("routing.proposal.json");
    let compiled = dir.join("compiled.json");
    let output = cli(&["validate", proposal.to_str().unwrap()]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let output = cli(&[
        "compile",
        proposal.to_str().unwrap(),
        "--output",
        compiled.to_str().unwrap(),
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read(&compiled).unwrap(),
        fs::read(dir.join("routing.artifact.json")).unwrap()
    );
    let output = cli(&[
        "diff",
        compiled.to_str().unwrap(),
        compiled.to_str().unwrap(),
    ]);
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["kind"], "identical");
    let output = cli(&[
        "simulate",
        dir.join("report.scenario.json").to_str().unwrap(),
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["complete"], true);
    // The finished runs have left the engine's State; the report's
    // inspection reads them from run history.
    let runs = value["inspection"]["runs"].as_array().unwrap();
    assert!(!runs.is_empty());
    assert!(runs.iter().all(|run| {
        ["completed", "failed", "cancelled"].contains(&run["status"].as_str().unwrap())
    }));
    assert!(
        !value["inspection"]["receipts"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&proposal).unwrap()).unwrap();
    value["proposal"]["playbook"]["entries"]["main"]["start"] = "nowhere".into();
    fs::write(&proposal, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(
        !cli(&["validate", proposal.to_str().unwrap()])
            .status
            .success()
    );
    fs::remove_dir_all(dir).unwrap();
}
#[test]
fn finite_catalog_and_required_explicit_store() {
    let out = cli(&["commands", "list"]);
    assert!(out.status.success());
    let names: Vec<String> = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(names.len(), 12);
    assert!(!cli(&["inspect"]).status.success());
    let out = cli(&["simulate", "--pilot", "routing"]);
    assert!(out.status.success());
}
