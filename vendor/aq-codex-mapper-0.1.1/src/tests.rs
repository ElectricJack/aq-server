use super::*;
use aq_session_protocol::{ArtifactRef, BudgetBasis, Capability, Digest, SpendingBudget, Text};

pub(super) fn settings() -> (HarnessSettings, ModelSettings) {
    let artifact = ArtifactRef {
        id: Text::new("independently-selected-artifact").unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(b"fixture metadata only"),
    };
    (
        HarnessSettings {
            harness: Text::new("codex").unwrap(),
            cli_version: Text::new(CODEX_CLI_VERSION).unwrap(),
            adapter: artifact.clone(),
            environment: artifact,
            required_capabilities: vec![Capability {
                id: Text::new("aq.harness.stop").unwrap(),
                version: 1,
            }],
        },
        ModelSettings {
            provider: Text::new("openai").unwrap(),
            model: Text::new("gpt-5.1-codex-mini").unwrap(),
            reasoning: Text::new("medium").unwrap(),
            max_output_tokens: 4096,
            spending_budget: None,
        },
    )
}
pub(super) fn selected<'a>(harness: &'a HarnessSettings, model: &'a ModelSettings) -> Settings<'a> {
    Settings {
        harness,
        model,
        terminal_input_enabled: false,
    }
}
#[test]
fn pure_recipe_does_not_require_remote_identity_or_existing_filesystem() {
    let (harness, model) = settings();
    let recipe = map(
        selected(&harness, &model),
        Paths {
            executable: Path::new("/aq-pure-mapper-fixture/executable"),
            workspace: Path::new("/aq-pure-mapper-fixture/workspace"),
            private_home: Path::new("/aq-pure-mapper-fixture/private-home"),
        },
    )
    .unwrap();
    assert_eq!(
        recipe.executable,
        Path::new("/aq-pure-mapper-fixture/executable")
    );
    assert_eq!(recipe.arguments.last().map(String::as_str), Some("-"));
    assert_eq!(recipe.environment.len(), 1);
    assert_eq!(
        recipe.environment["CODEX_HOME"],
        "/aq-pure-mapper-fixture/private-home"
    );
    assert!(
        map(
            selected(&harness, &model),
            Paths {
                executable: Path::new("relative"),
                workspace: Path::new("/work"),
                private_home: Path::new("/home"),
            }
        )
        .is_err()
    );
}
#[test]
fn unsupported_settings_cannot_map_a_recipe() {
    let (harness, model) = settings();
    let mut input = selected(&harness, &model);
    input.terminal_input_enabled = true;
    assert!(validate_settings(input).is_err());
    for value in ["0.165.0", "0.167.0"] {
        let mut changed = harness.clone();
        changed.cli_version = Text::new(value).unwrap();
        assert!(validate_settings(selected(&changed, &model)).is_err());
    }
    for value in ["aq.unknown", "aq.budget.task-cost"] {
        let mut changed = harness.clone();
        changed.required_capabilities[0].id = Text::new(value).unwrap();
        assert!(validate_settings(selected(&changed, &model)).is_err());
    }
    let mut changed = harness.clone();
    changed.required_capabilities[0].version = 2;
    assert!(validate_settings(selected(&changed, &model)).is_err());
    let mut changed = model.clone();
    changed.provider = Text::new("other-provider").unwrap();
    assert!(validate_settings(selected(&harness, &changed)).is_err());
    changed = model.clone();
    changed.reasoning = Text::new("unsupported").unwrap();
    assert!(validate_settings(selected(&harness, &changed)).is_err());
    changed = model;
    changed.spending_budget = Some(SpendingBudget {
        currency: Text::new("USD").unwrap(),
        basis: BudgetBasis::TaskCumulativeEstimatedCost,
        limit_microunits: 1,
    });
    assert!(validate_settings(selected(&harness, &changed)).is_err());
}
#[test]
fn scoped_endpoint_accepts_only_nonzero_port_and_emits_metadata() {
    assert!(LoopbackResponsesEndpoint::new(0).is_err());
    let overrides = responses_overrides(LoopbackResponsesEndpoint::new(32123).unwrap());
    let config: toml::Value = toml::from_str(&overrides.join("\n")).unwrap();
    assert_eq!(
        config["openai_base_url"].as_str(),
        Some("http://127.0.0.1:32123/v1")
    );
    assert_eq!(
        config["features"]["responses_websockets"].as_bool(),
        Some(false)
    );
    assert_eq!(
        config["features"]["responses_websockets_v2"].as_bool(),
        Some(false)
    );
}

#[test]
fn interactive_tui_uses_v1_model_prompt_mode_and_preserves_exec_recipe() {
    let (harness, model) = settings();
    let paths = || Paths {
        executable: Path::new("/installed/codex"),
        workspace: Path::new("/worktree"),
        private_home: Path::new("/private/home"),
    };
    let old = map(selected(&harness, &model), paths()).unwrap();
    assert_eq!(old.arguments[1], "exec");
    assert_eq!(old.arguments.last().unwrap(), "-");
    assert!(old.arguments.iter().any(|a| a == "--json"));
    assert!(!old.environment.contains_key("HOME"));
    let interactive = Settings {
        harness: &harness,
        model: &model,
        terminal_input_enabled: true,
    };
    let tui = map_interactive(interactive, paths()).unwrap();
    for forbidden in ["exec", "--json", "--ephemeral", "-"] {
        assert!(!tui.arguments.iter().any(|a| a == forbidden));
    }
    assert_eq!(tui.arguments.last().unwrap(), "--");
    assert!(
        tui.arguments
            .windows(2)
            .any(|a| a[0] == "-m" && a[1] == model.model.as_str())
    );
    assert!(
        tui.arguments
            .iter()
            .any(|a| a == "cli_auth_credentials_store=\"file\"")
    );
    assert_eq!(tui.environment["CODEX_HOME"], "/private/home");
    assert_eq!(tui.environment["HOME"], "/private/home");
    assert!(map(interactive, paths()).is_err());
    assert!(map_interactive(selected(&harness, &model), paths()).is_err());
}
#[test]
fn interactive_v1_composer_and_auth_dialog_rules_keep_no_escape_behavior() {
    assert_eq!(
        classify_current_viewport("status\n› "),
        InteractiveViewport::Ready
    );
    assert_eq!(
        classify_current_viewport("› Sign in with ChatGPT"),
        InteractiveViewport::AuthenticationRequired
    );
    assert_eq!(
        classify_current_viewport("Trust this folder?"),
        InteractiveViewport::TrustDirectory
    );
    assert_eq!(
        classify_current_viewport("working..."),
        InteractiveViewport::Waiting
    );
    assert_eq!(READY_DELAY_MS, 3000);
    assert_eq!(COMPOSER_CLEAR, &[0x15]);
    assert!(std::hint::black_box(SKIP_ESCAPE_BEFORE_ENTER));
}
