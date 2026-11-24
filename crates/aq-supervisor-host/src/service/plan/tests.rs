use super::*;
use aq_session_host::HostAdapter;
#[cfg(feature = "supervisor_full_access")]
#[test]
fn full_access_instructions_describe_the_toolchain_setup_task() {
    let root = PathBuf::from("/tmp/aq-plan-test");
    let config = ServiceConfig {
        database_url: "postgres://localhost/test".into(),
        conversation_id: Uuid::new_v4(),
        authority_id: Uuid::new_v4(),
        root: root.clone(),
        workspace: root.join("workspace"),
        homes: root.join("homes"),
        helper: PathBuf::from("/usr/local/bin/aq-session-host"),
        tmux: PathBuf::from("/usr/bin/tmux"),
        tmux_version: "tmux 3.3a".into(),
        native: Some(PathBuf::from("/opt/aq/bin/codex")),
        native_digest: Some(Digest::of(b"native")),
        image_digest: format!("sha256:{}", Digest::of(b"image").as_str()),
        secret_file: Some(root.join("subscription.json")),
        claude: None,
        server_origin: "http://server:8081".into(),
        automatic_turn_retries: default_turn_retries(),
    };
    let control = Control {
        actor: Uuid::new_v4(),
        enabled: true,
        revision: 1,
        profile: json!({}),
        owner: Some(Uuid::new_v4()),
        project: None,
        label: "Global supervisor".into(),
        role: None,
    };
    let text = instructions(&config, &control, "codex");
    #[cfg(feature = "project_environments")]
    for want in [
        "Set up the sandbox toolchain",
        ".aq/environments/<name>.md",
        "default: true",
        "aq environment try",
        "base environment",
        "allowed_environments before the recipe merges",
    ] {
        assert!(text.contains(want), "{want}");
    }
    #[cfg(not(feature = "project_environments"))]
    for gone in [".aq/environments", "aq environment try", "toolchain"] {
        assert!(!text.contains(gone), "{gone}");
    }
    #[cfg(feature = "project_environments")]
    {
        let scoped = Control {
            project: Some(Uuid::new_v4()),
            ..control
        };
        assert!(!instructions(&config, &scoped, "codex").contains(".aq/environments"));
    }
}
/// The owner's supervisor document replaces the project role text, with its
/// placeholders filled; without one the compiled text applies. The tool
/// manual is this build's either way.
#[cfg(feature = "supervisor_full_access")]
#[test]
fn a_project_supervisor_uses_the_saved_role_text_and_keeps_the_tool_manual() {
    let root = PathBuf::from("/tmp/aq-plan-test");
    let config = ServiceConfig {
        database_url: "postgres://localhost/test".into(),
        conversation_id: Uuid::new_v4(),
        authority_id: Uuid::new_v4(),
        root: root.clone(),
        workspace: root.join("workspace"),
        homes: root.join("homes"),
        helper: PathBuf::from("/usr/local/bin/aq-session-host"),
        tmux: PathBuf::from("/usr/bin/tmux"),
        tmux_version: "tmux 3.3a".into(),
        native: Some(PathBuf::from("/opt/aq/bin/codex")),
        native_digest: Some(Digest::of(b"native")),
        image_digest: format!("sha256:{}", Digest::of(b"image").as_str()),
        secret_file: Some(root.join("subscription.json")),
        claude: None,
        server_origin: "http://server:8081".into(),
        automatic_turn_retries: default_turn_retries(),
    };
    let project = Uuid::new_v4();
    let mut control = Control {
        actor: Uuid::new_v4(),
        enabled: true,
        revision: 1,
        profile: json!({}),
        owner: Some(Uuid::new_v4()),
        project: Some(project),
        label: "Example".into(),
        role: Some("Keep {label} ({project}) moving; mail {conversation}.".into()),
    };
    let manual = [
        ATTACHMENTS,
        MUTATION_REQUEST,
        "tool NAME with one JSON CommandRequest on stdin",
        "do not print credentials or read authentication files",
        "Your final answer is published automatically",
    ];
    let saved = instructions(&config, &control, "codex");
    let claude = instructions(&config, &control, "claude");
    assert_eq!(saved.lines().next(), claude.lines().next());
    assert!(claude.contains("mcp__aq__aq_command"));
    let conversation = config.conversation_id;
    assert!(saved.starts_with(&format!(
        "Keep Example ({project}) moving; mail {conversation}.\n"
    )));
    assert!(!saved.contains("keeping work moving"));
    for compiled in [None, Some(" \n".to_owned())] {
        control.role = compiled;
        let text = instructions(&config, &control, "codex");
        assert!(text.starts_with(&format!(
            "You are the engineer responsible for keeping work moving in project Example ({project})."
        )));
        for want in [
            "task.retry",
            "task.create",
            "task.dependency.add and \"resume_held\": true",
            "supervisor.escalate",
            "task.complete when its work is already on main",
            "task.work and task.history",
            "result.list and result.show",
            &format!("this conversation ({conversation})"),
        ] {
            assert!(text.contains(want), "{want}");
        }
        for placeholder in ["{label}", "{project}", "{conversation}"] {
            assert!(!text.contains(placeholder), "{placeholder}");
        }
        for want in manual {
            assert!(text.contains(want), "{want}");
        }
    }
    for want in manual {
        assert!(saved.contains(want), "{want}");
    }
    let global = instructions(
        &config,
        &Control {
            project: None,
            ..control
        },
        "codex",
    );
    assert!(global.contains(
        "Project supervisors escalate decisions to this conversation with supervisor.escalate"
    ));
}
#[test]
fn assembled_subscription_plans_satisfy_the_frozen_host_contract() {
    check_assembled_plan("codex", "openai", "medium", "codex-subscription-adapter");
    check_assembled_plan(
        "claude",
        "anthropic",
        "xhigh",
        "claude-subscription-adapter",
    );
}

fn check_assembled_plan(harness: &str, provider: &str, reasoning: &str, adapter: &str) {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().to_path_buf();
    let prior = example_plan();
    let mut config = ServiceConfig {
        database_url: "postgres://localhost/test".into(),
        conversation_id: prior.subject.conversation_id(),
        authority_id: Uuid::new_v4(),
        root: root.clone(),
        workspace: root.join("workspace"),
        homes: root.join("homes"),
        helper: PathBuf::from("/usr/local/bin/aq-session-host"),
        tmux: PathBuf::from("/usr/bin/tmux"),
        tmux_version: "tmux 3.3a".into(),
        native: Some(PathBuf::from("/opt/aq/bin/codex")),
        native_digest: Some(Digest::of(b"native")),
        image_digest: format!("sha256:{}", Digest::of(b"image").as_str()),
        secret_file: Some(root.join("subscription.json")),
        claude: None,
        server_origin: "http://server:8081".into(),
        automatic_turn_retries: default_turn_retries(),
    };
    let control = Control {
        actor: prior.subject.actor_id(),
        enabled: true,
        revision: 1,
        profile: json!({"provider":provider,"harness":harness,"model":"fixture-model","reasoning":reasoning,"snapshot_digest":Digest::of(b"snapshot"),"profile_id":"engineering","candidate_id":format!("{provider}:fixture-model:{reasoning}")}),
        owner: Some(Uuid::new_v4()),
        project: None,
        label: "Global supervisor".into(),
        role: None,
    };
    let (native, secret, digest) = if harness == "claude" {
        assert!(selection(&config, &control.profile).unwrap().is_none());
        let installation = NativeInstallation {
            native: PathBuf::from("/opt/claude/claude"),
            native_digest: Digest::of(b"Claude native"),
            secret_file: root.join("claude-token"),
        };
        let paths = (
            installation.native.clone(),
            installation.secret_file.clone(),
            installation.native_digest.clone(),
        );
        config.claude = Some(installation);
        config.native = None;
        config.native_digest = None;
        config.secret_file = None;
        let codex = json!({"provider":"openai","harness":"codex","model":"fixture-model","reasoning":"medium"});
        assert!(selection(&config, &codex).unwrap().is_none());
        paths
    } else {
        (
            config.native.clone().unwrap(),
            config.secret_file.clone().unwrap(),
            config.native_digest.clone().unwrap(),
        )
    };
    let config_path = root.join("service.json");
    write_service_config(&config_path, &config);
    let config = ServiceConfig::load(&config_path).unwrap();
    let selected = selection(&config, &control.profile).unwrap().unwrap();
    let turn = dispatch::Turn {
        batch: prior.batch,
        dispatch_id: Uuid::new_v4(),
        host_operation_id: Uuid::new_v4(),
        input: b"Actual immutable owner input".to_vec(),
    };
    let candidate = assemble(
        &config,
        &control,
        Generation {
            policy: &Digest::of(b"policy"),
            number: 1,
            session: Uuid::new_v4(),
        },
        turn,
        &selected,
    )
    .unwrap();
    assert_eq!(candidate.host.harness.harness.as_str(), harness);
    assert_eq!(
        candidate.host.harness.cli_version.as_str(),
        selected.cli_version
    );
    assert_eq!(
        candidate.host.harness.adapter.id.as_str(),
        format!("aq.central/{adapter}")
    );
    assert_eq!(candidate.model.provider.as_str(), provider);
    assert_eq!(candidate.model.model.as_str(), "fixture-model");
    assert_eq!(candidate.model.reasoning.as_str(), reasoning);
    assert_eq!(candidate.native, native);
    assert_eq!(candidate.native_digest, digest);
    assert_eq!(candidate.secret_file, secret);
    assert_eq!(
        candidate.central.profiles.catalog.digest,
        Digest::of(b"snapshot")
    );
    let prompt = &candidate.subscription.as_ref().unwrap().instructions;
    assert!(prompt.contains(ATTACHMENTS));
    assert_eq!(prompt.contains("mcp__aq__aq_command"), harness == "claude");
    assert_eq!(
        prompt.contains("mcp__aq__aq_attachment"),
        harness == "claude"
    );
    assert_eq!(
        prompt.contains("mcp__aq__repo_read"),
        harness == "claude"
            && cfg!(all(
                feature = "supervisor_full_access",
                feature = "delivery_playbook"
            ))
    );
    assert_scope_instructions(&config, &control, harness);
    assert_eq!(
        candidate.swarm_profile.as_ref().unwrap()["profile_id"],
        "engineering"
    );
    crate::validate_join(&candidate.central, &candidate.host).unwrap();
    install(&config, &candidate).unwrap();
    let (_, installed_digest) = Config::load(&config.turn_file()).unwrap();
    let adapter = NativeAdapter {
        config: config.turn_file(),
        digest: installed_digest,
        wrapper: std::env::current_exe().unwrap(),
    };
    let launch = adapter
        .prepare(&candidate.host, &candidate.workspace)
        .unwrap();
    assert_eq!(launch.adapter, candidate.host.harness.adapter);
    assert!(
        candidate
            .host
            .harness
            .required_capabilities
            .iter()
            .any(|c| c.id.as_str() == "aq.harness.confined-worktree")
    );
}

fn write_service_config(path: &Path, config: &ServiceConfig) {
    let bytes = serde_json::to_vec(config).unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap()
        .write_all(&bytes)
        .unwrap();
}

fn example_plan() -> CentralPlan {
    let examples: Value = serde_json::from_str(include_str!(
        "../../../../aq-central-authority/artifacts/examples.json"
    ))
    .unwrap();
    let aq_central_authority::Document::Plan(prior) =
        aq_central_authority::decode(&serde_json::to_vec(&examples["plan"]).unwrap()).unwrap()
    else {
        panic!("plan fixture")
    };
    prior
}

fn assert_scope_instructions(config: &ServiceConfig, control: &Control, harness: &str) {
    for scope in [None, Some(Uuid::new_v4())] {
        let scoped = Control {
            project: scope,
            ..control.clone()
        };
        assert!(instructions(config, &scoped, harness).contains(ATTACHMENTS));
    }
    #[cfg(feature = "supervisor_full_access")]
    {
        let text = instructions(config, control, harness);
        for want in [
            "Create tasks active unless the owner asks for a draft",
            "dashboard.swarm, session.list and attempt.show",
            "Do not merge, deploy or alter host services or credentials.",
            "resend it to recheck an owner command",
        ] {
            assert!(text.contains(want), "{want}");
        }
        let mut scoped = control.clone();
        scoped.project = Some(Uuid::new_v4());
        let scoped = instructions(config, &scoped, harness);
        assert!(scoped.contains("task.work and task.history"));
        assert!(!scoped.contains("dashboard.swarm"));
        assert_eq!(
            text.contains(
                "repo sync PROJECT_ID`, then read `/workspace/repos/<name>` with git and rg"
            ),
            cfg!(feature = "delivery_playbook") && harness == "codex"
        );
        for gone in [
            "Create draft tasks for proposals",
            "owner-only configuration",
        ] {
            assert!(!text.contains(gone), "{gone}");
        }
        let scoped = Control {
            project: Some(Uuid::new_v4()),
            ..control.clone()
        };
        let text = instructions(config, &scoped, harness);
        assert!(text.contains("You hold the owner's powers inside this project"));
        assert!(!text.contains("you may read and change any project"));
    }
}

#[test]
fn configured_claude_waits_for_a_usable_executable_and_protected_login() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let directory = tempfile::tempdir().unwrap();
    let executable = directory.path().join("claude");
    let login = directory.path().join("login");
    let selection = Selection {
        harness: "claude",
        cli_version: crate::adapter::CLAUDE_CLI_VERSION,
        adapter: "claude-subscription-adapter",
        provider: "anthropic",
        model: "fixture-model",
        reasoning: "high",
        native: &executable,
        // Readiness does not hash the executable.
        native_digest: &Digest::of(b"different executable"),
        secret_file: &login,
    };
    let executable_wait =
        Some("Waiting for a usable Claude Code executable in the supervisor image");
    let login_wait = Some("Waiting for a nonempty Claude subscription token in its protected file");
    assert_eq!(selection.unavailable(), executable_wait);
    std::fs::write(&executable, b"executable").unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(selection.unavailable(), executable_wait);
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(selection.unavailable(), login_wait);
    std::fs::write(&login, b"").unwrap();
    std::fs::set_permissions(&login, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(selection.unavailable(), login_wait);
    std::fs::write(&login, b"fixture token").unwrap();
    std::fs::set_permissions(&login, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(selection.unavailable(), login_wait);
    std::fs::set_permissions(&login, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(selection.unavailable(), None);
    let linked = directory.path().join("linked-login");
    symlink(&login, &linked).unwrap();
    let selection = Selection {
        secret_file: &linked,
        ..selection
    };
    assert_eq!(selection.unavailable(), login_wait);
}
