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
    // The worked authoring workspaces (tests/authoring_examples.rs runs the
    // tools in them).
    for example in ["escalate-on-judgment", "notify-when-ready"] {
        assert!(dir.join(example).join("draft.json").is_file(), "{example}");
    }
    // `validate`, `compile` and `diff` took a whole CompileInput; the
    // authoring tools (`check`, `seal`, `compare`, tests/authoring_cli.rs)
    // replace them. `simulate` ran raw scenarios (`try` runs cases now),
    // and `surface` and `interfaces` described the synthetic pilot surface
    // and callable entries, which an authoring source is not offered. All
    // of them are refused.
    let draft = dir.join("notify-when-ready/draft.json");
    let draft = draft.to_str().unwrap();
    for args in [
        &["validate", draft][..],
        &["compile", draft, "--output", "x.json"],
        &["diff", draft, draft],
        &["simulate", "--pilot", "coding"],
        &["surface"],
        &["interfaces", draft, "list"],
    ] {
        let out = cli(args);
        assert!(!out.status.success(), "{args:?}");
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("unrecognized subcommand"),
            "{args:?}"
        );
    }
    fs::remove_dir_all(dir).unwrap();
}
#[test]
fn finite_catalog_and_required_explicit_store() {
    let out = cli(&["commands", "list"]);
    assert!(out.status.success());
    let names: Vec<String> = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(names.len(), 12);
    assert!(!cli(&["inspect"]).status.success());
}
