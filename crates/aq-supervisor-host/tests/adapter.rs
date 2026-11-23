use aq_codex_mapper::Paths;
use aq_session_protocol::LaunchManifest;
use aq_supervisor_host::adapter::native_recipe;
use std::path::Path;
#[test]
fn native_recipe_uses_scoped_route_stdin_and_no_provider_material() {
    let mut m: LaunchManifest = serde_json::from_str(include_str!(
        "../../../vendor/aq-session-host-0.1.7/tests/fixtures/manifest.json"
    ))
    .unwrap();
    m.harness.cli_version = aq_session_protocol::Text::new("0.166.0").unwrap();
    m.harness.harness = aq_session_protocol::Text::new("codex").unwrap();
    m.model.provider = aq_session_protocol::Text::new("openai").unwrap();
    m.model.model = aq_session_protocol::Text::new("fixture-model").unwrap();
    m.model.reasoning = aq_session_protocol::Text::new("high").unwrap();
    m.model.spending_budget = None;
    let recipe = recipe_for(&m, false).unwrap();
    assert_eq!(recipe.arguments.last().unwrap(), "-");
    assert!(
        recipe
            .arguments
            .iter()
            .any(|s| s.contains("http://127.0.0.1:12345/v1"))
    );
    assert!(recipe.arguments.iter().any(|s| s == "--ignore-user-config"));
    assert_eq!(
        recipe.environment["AQ_SUPERVISOR_CREDENTIAL_FILE"],
        "/homes/test/supervisor.credential"
    );
    assert!(recipe.arguments.iter().any(|s| s ==
        "shell_environment_policy.set.AQ_SUPERVISOR_CREDENTIAL_FILE=\"/homes/test/supervisor.credential\""));
    assert_eq!(
        recipe
            .arguments
            .iter()
            .rev()
            .find(|s| s.starts_with("sandbox_workspace_write.network_access="))
            .unwrap(),
        "sandbox_workspace_write.network_access=true"
    );
    assert!(
        !recipe
            .environment
            .keys()
            .any(|k| k.contains("KEY") || k.contains("TOKEN"))
    );
    assert!(recipe_for(&m, true).is_err());
}

fn recipe_for(
    m: &LaunchManifest,
    terminal_input_enabled: bool,
) -> anyhow::Result<aq_codex_mapper::Recipe> {
    native_recipe(
        aq_codex_mapper::Settings {
            harness: &m.harness,
            model: &m.model,
            terminal_input_enabled,
        },
        Paths {
            executable: Path::new("/runtime/codex"),
            workspace: Path::new("/workspace"),
            private_home: Path::new("/homes/test"),
        },
        12345,
    )
}

/// The `internal` mode's Codex runner, driven through one `tick` against a fake
/// native executable (a shell script) and a real test database. Runs under
/// the `test-support` identity: the current uid, no privilege drop, no chown.
#[path = "adapter/internal/mod.rs"]
mod internal;
