use super::*;
use crate::tests::{selected, settings};
fn paths() -> Paths<'static> {
    Paths {
        executable: Path::new("/opt/aq/codex"),
        workspace: Path::new("/var/lib/aq/git/worktree"),
        private_home: Path::new("/var/lib/aq/host/private-home"),
    }
}
fn channel() -> ResultChannel<'static> {
    ResultChannel {
        root: Path::new("/var/lib/aq/results/attempt"),
        client: Path::new("/opt/aq/aq-agent"),
    }
}
fn overrides(recipe: &Recipe) -> toml::Value {
    let values: Vec<_> = recipe
        .arguments
        .windows(2)
        .filter(|args| args[0] == "--config")
        .map(|args| args[1].as_str())
        .collect();
    toml::from_str(&values.join("\n")).unwrap()
}
#[test]
fn result_recipe_exposes_only_declared_channel_and_submission_child() {
    let (harness, model) = settings();
    let recipe = map_with_result_channel(selected(&harness, &model), paths(), channel()).unwrap();
    assert_eq!(recipe.environment.len(), 3);
    assert_eq!(
        recipe.environment["AQ_AGENT_RESULT_DIR"],
        "/var/lib/aq/results/attempt"
    );
    assert_eq!(
        recipe.environment["AQ_AGENT_RESULT_COMMAND"],
        "/opt/aq/aq-agent"
    );
    let config = overrides(&recipe);
    assert_eq!(
        config["sandbox_workspace_write"]["writable_roots"]
            .as_array()
            .unwrap(),
        &[toml::Value::String(
            "/var/lib/aq/results/attempt/submissions".into()
        )]
    );
    assert_eq!(
        config["shell_environment_policy"]["set"]["AQ_AGENT_RESULT_DIR"].as_str(),
        Some("/var/lib/aq/results/attempt")
    );
    assert_eq!(
        config["shell_environment_policy"]["set"]["AQ_AGENT_RESULT_COMMAND"].as_str(),
        Some("/opt/aq/aq-agent")
    );
    assert_eq!(
        config["shell_environment_policy"]["set"]["PATH"].as_str(),
        Some("/usr/bin:/bin")
    );
    assert_eq!(
        config["shell_environment_policy"]["inherit"].as_str(),
        Some("none")
    );
    assert_eq!(
        config["sandbox_workspace_write"]["network_access"].as_bool(),
        Some(false)
    );
    assert_eq!(recipe.arguments.last().map(String::as_str), Some("-"));
}
#[test]
fn removing_only_declared_delta_recovers_exact_original_recipe_bytes() {
    let (harness, model) = settings();
    let original = map(selected(&harness, &model), paths()).unwrap();
    let mut result =
        map_with_result_channel(selected(&harness, &model), paths(), channel()).unwrap();
    result.environment.remove("AQ_AGENT_RESULT_DIR");
    result.environment.remove("AQ_AGENT_RESULT_COMMAND");
    let end = result.arguments.len() - 1;
    let removed: Vec<_> = result.arguments.drain(end - 4..end).collect();
    assert_eq!(removed[0], "--config");
    assert!(removed[1].starts_with("shell_environment_policy.set.AQ_AGENT_RESULT_DIR="));
    assert_eq!(removed[2], "--config");
    assert!(removed[3].starts_with("shell_environment_policy.set.AQ_AGENT_RESULT_COMMAND="));
    for arg in &mut result.arguments {
        if arg.starts_with("sandbox_workspace_write.writable_roots=") {
            *arg = "sandbox_workspace_write.writable_roots=[]".into();
        }
    }
    assert_eq!(result.executable, original.executable);
    assert_eq!(result.arguments, original.arguments);
    assert_eq!(result.environment, original.environment);
}
#[test]
fn lexical_paths_cannot_smuggle_other_writable_or_private_roots() {
    let (harness, model) = settings();
    for root in [
        "relative",
        "/",
        "/var/lib/aq/git/worktree/result",
        "/var/lib/aq/git",
        "/var/lib/aq/host/private-home/result",
        "/var/lib/aq/results/../git",
        "/var//results",
        "/var/./results",
        "/var/results/",
    ] {
        assert!(
            map_with_result_channel(
                selected(&harness, &model),
                paths(),
                ResultChannel {
                    root: Path::new(root),
                    ..channel()
                }
            )
            .is_err(),
            "root {root}"
        );
    }
    for client in [
        "relative",
        "/var/lib/aq/results/attempt/client",
        "/var/lib/aq/git/worktree/client",
        "/var/lib/aq/host/private-home/client",
        "/opt/../client",
    ] {
        assert!(
            map_with_result_channel(
                selected(&harness, &model),
                paths(),
                ResultChannel {
                    client: Path::new(client),
                    ..channel()
                }
            )
            .is_err(),
            "client {client}"
        );
    }
}
#[test]
fn quoted_paths_are_literal_config_values_and_recipe_stays_bounded() {
    let (harness, model) = settings();
    let root = Path::new("/var/lib/aq/results/quoted \"root\"");
    let client = Path::new("/opt/aq/client \"literal\"");
    let recipe = map_with_result_channel(
        selected(&harness, &model),
        paths(),
        ResultChannel { root, client },
    )
    .unwrap();
    let config = overrides(&recipe);
    assert_eq!(
        config["shell_environment_policy"]["set"]["AQ_AGENT_RESULT_COMMAND"].as_str(),
        client.to_str()
    );
    assert_eq!(
        config["sandbox_workspace_write"]["writable_roots"][0].as_str(),
        root.join("submissions").to_str()
    );
    let oversized = format!("/var/{}", "x".repeat(2048));
    assert!(
        map_with_result_channel(
            selected(&harness, &model),
            paths(),
            ResultChannel {
                root: Path::new(&oversized),
                ..channel()
            }
        )
        .is_err()
    );
}
