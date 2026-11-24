use super::*;
// ---- the binary: modes, names, exit codes ----------------------------------

fn bin() -> std::process::Command {
    let mut c = std::process::Command::new(env!("CARGO_BIN_EXE_aqv2"));
    c.env_remove("AQ_CHANNEL");
    c
}
fn run(args: Vec<&'static str>, channel: String) -> tokio::task::JoinHandle<std::process::Output> {
    tokio::task::spawn_blocking(move || {
        bin()
            .env("AQ_CHANNEL", channel)
            .args(args)
            .output()
            .unwrap()
    })
}

async fn assert_grant_refusal(channel: &str) {
    let out = run(
        vec!["task", "create", "--title", "Unrelated top-level task"],
        channel.to_owned(),
    )
    .await
    .unwrap();
    assert_eq!(out.status.code(), Some(14));
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(
        err.contains("refused (grant_missing): task.create needs task.create.project"),
        "{err}"
    );
    assert!(
        err.contains("Use `aq question ask` to request it."),
        "{err}"
    );
    assert!(
        err.contains("held: task.create.subtask @ task:subtree (max_created_tasks=5, max_depth=2)"),
        "{err}"
    );
    let out = run(
        vec!["task", "create", "--title", "x", "--json"],
        channel.to_owned(),
    )
    .await
    .unwrap();
    assert_eq!(out.status.code(), Some(14));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["error"]["code"], "grant_missing");
    assert!(out.stderr.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_binary_talks_to_the_channel_named_by_aq_channel() {
    let mut fake = fake(|method, path, body| match path {
        "/api/v1/commands/task.create" => {
            (StatusCode::FORBIDDEN, fixture("refusal-grant-missing.json"))
        }
        _ => default_reply(method, path, body),
    })
    .await;
    let channel = format!("unix:{}", fake.socket.display());
    let out = run(vec!["whoami"], channel.clone()).await.unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.contains("principal: agent") && text.contains(&format!("task       {TASK}")),
        "{text}"
    );
    let out = run(vec!["--json", "whoami"], channel.clone())
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v, json!({"schema_version":1,"data":identity()}));

    assert_grant_refusal(&channel).await;

    // Channel mode sends no credential, so a token file or origin is refused.
    let out = run(
        vec!["--cf-access-token-file", "/x", "whoami"],
        channel.clone(),
    )
    .await
    .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let out = run(
        vec!["--origin", "https://aq.example", "--json", "whoami"],
        channel.clone(),
    )
    .await
    .unwrap();
    assert_eq!(out.status.code(), Some(2));
    // A missing socket is a closed channel: nothing was sent.
    let gone = format!("unix:{}/gone.sock", fake.socket.parent().unwrap().display());
    let out = run(vec!["--json", "whoami"], gone).await.unwrap();
    assert_eq!(out.status.code(), Some(13));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["error"]["code"], "channel_closed");
    let out = run(vec!["--json", "whoami"], "tcp://127.0.0.1:1".into())
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["error"]["code"], "configuration_error");
    // `--describe` reads the built-in catalog without a channel.
    let out = run(
        vec!["--json", "run", "task.create", "--describe"],
        "unix:/nonexistent/aq.sock".into(),
    )
    .await
    .unwrap();
    assert!(out.status.success());
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["data"]["grant"], "task.create.subtask");
    let seen = fake.drain();
    assert!(
        seen.iter()
            .all(|s| !s.headers.contains_key("authorization"))
    );
}
#[test]
fn installed_as_aq_the_binary_names_itself_aq() {
    let dir = tempfile::tempdir().unwrap();
    let aq = dir.path().join("aq");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_aqv2"), &aq).unwrap();
    let out = std::process::Command::new(&aq)
        .env_remove("AQ_CHANNEL")
        .arg("--help")
        .output()
        .unwrap();
    assert!(out.status.success());
    let help = String::from_utf8(out.stdout).unwrap();
    assert!(help.contains("Usage: aq [OPTIONS] <COMMAND>"), "{help}");
    for group in [
        "run",
        "commands",
        "whoami",
        "task",
        "knowledge",
        "comment",
        "question",
        "wait",
        "message",
    ] {
        assert!(help.contains(&format!("\n  {group} ")), "{group}");
    }
    let out = bin().arg("--help").output().unwrap();
    assert!(
        String::from_utf8(out.stdout)
            .unwrap()
            .contains("Usage: aqv2 [OPTIONS] <COMMAND>")
    );
    // Without a mode the error says how to choose one.
    let out = std::process::Command::new(&aq)
        .env_remove("AQ_CHANNEL")
        .args(["--json", "whoami"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("AQ_CHANNEL")
    );
}
