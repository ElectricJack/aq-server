use super::*;

// Generated flags (flags.rs): a command's params schema gives its flags, so
// `project show --project-id ID` is `run project.show --param project_id=ID`.
#[tokio::test]
async fn generated_flags_build_the_same_request_as_params() {
    let project = "00000000-0000-4000-8000-000000000010";
    let mut srv = server(vec![]).await;
    let _ = aqv2::execute(
        srv.client.clone(),
        &["project", "show", "--project-id", project],
    )
    .await;
    let flagged = srv.next().await;
    let _ = aqv2::execute(
        srv.client.clone(),
        &[
            "run",
            "project.show",
            "--param",
            &format!("project_id={project}"),
        ],
    )
    .await;
    let plain = srv.next().await;
    assert_eq!(flagged.method, "POST");
    assert_eq!(flagged.path, "/api/v1/commands/project.show");
    assert_eq!(flagged.body, json!({"params":{"project_id":project}}));
    assert_eq!((flagged.path, flagged.body), (plain.path, plain.body));

    // Values are typed by the schema: integers are numbers, `=` is accepted,
    // and a flag mixes with --param.
    let args = [
        "task",
        "list",
        "--project-id",
        project,
        "--limit=5",
        "--param",
        "q=launch notes",
    ];
    let _ = aqv2::execute(srv.client.clone(), &args).await;
    let list = srv.next().await;
    assert_eq!(list.path, "/api/v1/commands/task.list");
    assert_eq!(
        list.body,
        json!({"params":{"project_id":project,"limit":5,"q":"launch notes"}})
    );

    // A mutation keeps `run`'s own flags and its generated operation ID.
    let args = [
        "task",
        "cancel",
        "--project-id",
        project,
        "--expected-version",
        "7",
        "--subject",
        "task:00000000-0000-4000-8000-000000000020:7",
    ];
    let _ = aqv2::execute(srv.client.clone(), &args).await;
    let cancel = srv.next().await;
    assert_eq!(cancel.path, "/api/v1/commands/task.cancel");
    let op = cancel.body["operation_id"].clone();
    assert!(op.is_string());
    assert_eq!(
        cancel.body["params"],
        json!({"operation_id":op,"project_id":project,"expected_version":"7"})
    );
    assert_eq!(cancel.body["subject"]["revision"], "7");
    assert!(srv.seen.try_recv().is_err());
}

#[tokio::test]
async fn generated_flags_refuse_bad_input_before_sending() {
    let project = "00000000-0000-4000-8000-000000000010";
    let mut srv = server(vec![]).await;
    for (args, needle) in [
        (
            vec!["task", "list", "--project-id", project, "--limit", "many"],
            "an integer",
        ),
        (
            vec!["task", "list", "--project-id", project, "--limit"],
            "takes a value",
        ),
        (
            vec![
                "task",
                "list",
                "--project-id",
                project,
                "--limit",
                "1",
                "--limit",
                "2",
            ],
            "twice",
        ),
        (
            vec![
                "task",
                "list",
                "--project-id",
                project,
                "--param",
                &format!("project_id={project}"),
            ],
            "both",
        ),
        (
            vec!["question", "list", "--state", "pending"],
            "open, closed",
        ),
        (
            vec!["task", "list", "--project-id", project, "--bogus", "1"],
            "--help",
        ),
    ] {
        let error = aqv2::execute(srv.client.clone(), &args).await.unwrap_err();
        assert_eq!(error["code"], "invalid_arguments", "{args:?}");
        let message = error["message"].as_str().unwrap();
        assert!(message.contains(needle), "{args:?}: {message}");
    }
    assert!(srv.seen.try_recv().is_err());
}

#[tokio::test]
async fn generated_help_needs_no_request() {
    let mut srv = server(vec![]).await;
    let help = |v: Value| v["help"].as_str().unwrap().to_owned();
    let show = help(
        aqv2::execute(srv.client.clone(), &["task", "list", "--help"])
            .await
            .unwrap(),
    );
    for needle in [
        "Tasks in a project",
        "task.list (read, needs task.read)",
        "--project-id",
        "required",
        "--limit       integer 1..64, default 25",
        "--param KEY=VALUE",
    ] {
        assert!(show.contains(needle), "{needle}: {show}");
    }
    let cancel = help(
        aqv2::execute(srv.client.clone(), &["task", "cancel", "-h"])
            .await
            .unwrap(),
    );
    assert!(
        cancel.contains("task.cancel (mutation, needs task.cancel)")
            && cancel.contains("--task-id")
            && cancel.contains("--subject TYPE:ID"),
        "{cancel}"
    );
    assert!(
        !cancel.contains("--operation-id <"),
        "the operation ID is filled in: {cancel}"
    );
    // An agent-only command says so to the owner instead of listing flags.
    let wait = aqv2::flags::help("wait.register", "owner").unwrap();
    assert!(wait.contains("Not available to the owner"), "{wait}");
    // Tokens that name a group list its commands.
    let group = help(
        aqv2::execute(srv.client.clone(), &["project"])
            .await
            .unwrap(),
    );
    for needle in ["project list", "project show", "project revision show"] {
        assert!(group.contains(needle), "{group}");
    }
    assert!(srv.seen.try_recv().is_err());
}

#[test]
fn every_catalog_command_has_help_and_resolvable_flags() {
    let mut described = 0;
    for c in aqv2::catalog::commands() {
        let name = c["name"].as_str().unwrap();
        for principal in c["principals"].as_object().unwrap().keys() {
            if !matches!(principal.as_str(), "owner" | "agent") {
                continue;
            }
            let help = aqv2::flags::help(name, principal).unwrap();
            assert!(
                help.contains(name) && help.contains("Usage:"),
                "{name}: {help}"
            );
            let flags = aqv2::flags::flags(name, principal);
            let mut names: Vec<String> = flags.iter().map(|f| f.name()).collect();
            names.sort();
            names.dedup();
            assert_eq!(names.len(), flags.len(), "{name}: flag names are unique");
            if aqv2::flags::schema(name, principal).is_some() {
                described += 1;
            }
        }
    }
    // Every schema but the three lifecycle requests resolves in this binary.
    let total: usize = aqv2::catalog::commands()
        .iter()
        .map(|c| {
            c["principals"]
                .as_object()
                .unwrap()
                .keys()
                .filter(|p| matches!(p.as_str(), "owner" | "agent"))
                .count()
        })
        .sum();
    assert!(total - described <= 3, "{described} of {total} described");
}
