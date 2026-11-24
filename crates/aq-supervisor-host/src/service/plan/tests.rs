use super::*;
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
        native: PathBuf::from("/opt/aq/bin/codex"),
        native_digest: Digest::of(b"native"),
        image_digest: format!("sha256:{}", Digest::of(b"image").as_str()),
        secret_file: root.join("subscription.json"),
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
    let text = instructions(&config, &control);
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
        assert!(!instructions(&config, &scoped).contains(".aq/environments"));
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
        native: PathBuf::from("/opt/aq/bin/codex"),
        native_digest: Digest::of(b"native"),
        image_digest: format!("sha256:{}", Digest::of(b"image").as_str()),
        secret_file: root.join("subscription.json"),
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
    let saved = instructions(&config, &control);
    let conversation = config.conversation_id;
    assert!(saved.starts_with(&format!(
        "Keep Example ({project}) moving; mail {conversation}.\n"
    )));
    assert!(!saved.contains("keeping work moving"));
    for compiled in [None, Some(" \n".to_owned())] {
        control.role = compiled;
        let text = instructions(&config, &control);
        assert!(text.starts_with(&format!(
            "You are the engineer responsible for keeping work moving in project Example ({project})."
        )));
        for want in [
            "task.retry",
            "task.create",
            "task.dependency.add and \"resume_held\": true",
            "supervisor.escalate",
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
    );
    assert!(global.contains(
        "Project supervisors escalate decisions to this conversation with supervisor.escalate"
    ));
}
#[test]
fn assembled_subscription_plan_satisfies_the_frozen_host_contract() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().to_path_buf();
    let prior = example_plan();
    let config = ServiceConfig {
        database_url: "postgres://localhost/test".into(),
        conversation_id: prior.subject.conversation_id(),
        authority_id: Uuid::new_v4(),
        root: root.clone(),
        workspace: root.join("workspace"),
        homes: root.join("homes"),
        helper: PathBuf::from("/usr/local/bin/aq-session-host"),
        tmux: PathBuf::from("/usr/bin/tmux"),
        tmux_version: "tmux 3.3a".into(),
        native: PathBuf::from("/opt/aq/bin/codex"),
        native_digest: Digest::of(b"native"),
        image_digest: format!("sha256:{}", Digest::of(b"image").as_str()),
        secret_file: root.join("subscription.json"),
        server_origin: "http://server:8081".into(),
        automatic_turn_retries: default_turn_retries(),
    };
    let control = Control {
        actor: prior.subject.actor_id(),
        enabled: true,
        revision: 1,
        profile: json!({"provider":"openai","harness":"codex","model":"fixture-model","reasoning":"medium","snapshot_digest":Digest::of(b"snapshot"),"profile_id":"engineering","candidate_id":"openai:fixture-model:medium"}),
        owner: Some(Uuid::new_v4()),
        project: None,
        label: "Global supervisor".into(),
        role: None,
    };
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
    )
    .unwrap();
    candidate.host.digest().unwrap();
    assert_scope_instructions(&config, &control);
    assert_eq!(
        candidate.swarm_profile.as_ref().unwrap()["profile_id"],
        "engineering"
    );
    crate::validate_join(&candidate.central, &candidate.host).unwrap();
    assert!(
        candidate
            .host
            .harness
            .required_capabilities
            .iter()
            .any(|c| c.id.as_str() == "aq.harness.confined-worktree")
    );
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

fn assert_scope_instructions(config: &ServiceConfig, control: &Control) {
    for scope in [None, Some(Uuid::new_v4())] {
        let scoped = Control {
            project: scope,
            ..control.clone()
        };
        assert!(instructions(config, &scoped).contains(ATTACHMENTS));
    }
    #[cfg(feature = "supervisor_full_access")]
    {
        let text = instructions(config, control);
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
        let scoped = instructions(config, &scoped);
        assert!(scoped.contains("task.work and task.history"));
        assert!(!scoped.contains("dashboard.swarm"));
        assert_eq!(
            text.contains(
                "repo sync PROJECT_ID`, then read `/workspace/repos/<name>` with git and rg"
            ),
            cfg!(feature = "delivery_playbook")
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
        let text = instructions(config, &scoped);
        assert!(text.contains("You hold the owner's powers inside this project"));
        assert!(!text.contains("you may read and change any project"));
    }
}
