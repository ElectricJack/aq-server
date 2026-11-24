use super::*;
use aq_central_authority::{Digest, Document, MailBatch, decode};

fn service_config(root: PathBuf) -> ServiceConfig {
    std::fs::create_dir(&root).unwrap();
    ServiceConfig {
        database_url: "postgres://localhost/test".into(),
        conversation_id: Uuid::new_v4(),
        authority_id: Uuid::new_v4(),
        workspace: root.join("workspace"),
        homes: root.join("homes"),
        helper: PathBuf::from("/usr/local/bin/aq-session-host"),
        tmux: PathBuf::from("/usr/bin/tmux"),
        tmux_version: "tmux 3.3a".into(),
        native: PathBuf::from("/opt/aq/bin/codex"),
        native_digest: Digest::of(b"codex fixture binary"),
        image_digest: format!("sha256:{}", Digest::of(b"image").as_str()),
        secret_file: root.join("openai.json"),
        claude: Some(NativeInstallation {
            native: PathBuf::from("/opt/aq/bin/claude"),
            native_digest: Digest::of(b"claude fixture binary"),
            secret_file: root.join("anthropic.json"),
        }),
        root,
        server_origin: "http://server:8081".into(),
        automatic_turn_retries: default_turn_retries(),
    }
}

fn initial_control(project: Option<Uuid>) -> Control {
    Control {
        actor: Uuid::new_v4(),
        enabled: true,
        revision: 1,
        profile: json!({
            "profile_id": "engineering",
            "candidate_id": "openai:fixture-model:medium",
            "provider": "openai",
            "harness": "codex",
            "model": "fixture-model",
            "reasoning": "medium",
            "snapshot_digest": Digest::of(b"initial catalog"),
        }),
        owner: Some(Uuid::new_v4()),
        project,
        label: "Supervisor fixture".into(),
    }
}

fn mail_batch() -> MailBatch {
    let examples: Value = serde_json::from_str(include_str!(
        "../../../../aq-central-authority/artifacts/examples.json"
    ))
    .unwrap();
    let Document::Plan(example) = decode(&serde_json::to_vec(&examples["plan"]).unwrap()).unwrap()
    else {
        panic!("expected central plan fixture");
    };
    example.batch
}

fn assemble(
    config: &ServiceConfig,
    control: &Control,
    generation: u64,
    batch: MailBatch,
    input: &[u8],
) -> Config {
    let selection = plan::selection(config, &control.profile).unwrap().unwrap();
    plan::assemble(
        config,
        control,
        plan::Generation {
            policy: &Digest::of(b"reservation policy"),
            number: generation,
            session: Uuid::new_v4(),
        },
        dispatch::Turn {
            batch,
            input: input.to_vec(),
            dispatch_id: Uuid::new_v4(),
            host_operation_id: Uuid::new_v4(),
        },
        &selection,
    )
    .unwrap()
}

#[tokio::test]
async fn provider_switch_preserves_recovered_turn_and_changes_only_its_successor() {
    let tmp = tempfile::tempdir().unwrap();
    let global = service_config(tmp.path().join("global"));
    let project = service_config(tmp.path().join("project"));
    let global_control = initial_control(None);
    let mut project_control = initial_control(Some(Uuid::new_v4()));
    let pool = PgPool::connect_lazy(&global.database_url).unwrap();
    let batch = mail_batch();
    let global_turn = assemble(&global, &global_control, 1, batch.clone(), b"global input");
    let prior = assemble(
        &project,
        &project_control,
        1,
        batch.clone(),
        b"retained project input",
    );
    plan::install(&global, &global_turn).unwrap();
    plan::install(&project, &prior).unwrap();

    project_control.revision += 1;
    project_control.profile = json!({
        "profile_id": "engineering",
        "candidate_id": "anthropic:claude-opus-4-6:high",
        "provider": "anthropic",
        "harness": "claude",
        "model": "claude-opus-4-6",
        "reasoning": "high",
        "snapshot_digest": Digest::of(b"updated catalog"),
    });

    let successor = assemble(&project, &project_control, 2, batch, b"new project input");
    assert!(plan::install(&project, &successor).is_err());
    let (recovered, _) = installed(&project, &pool).unwrap().unwrap();
    assert_eq!(recovered.central, prior.central);
    assert_eq!(
        recovered.host.digest().unwrap(),
        prior.host.digest().unwrap()
    );
    assert_eq!(recovered.swarm_profile, prior.swarm_profile);
    assert_eq!(recovered.model.provider.as_str(), "openai");
    assert_eq!(recovered.native_digest, project.native_digest);
    assert_eq!(recovered.secret_file, project.secret_file);
    assert_eq!(
        protected_read(&recovered.startup_file, 1024).unwrap(),
        b"retained project input"
    );

    let archive = project
        .root
        .join(format!("turn-1-{}.json", prior.central.dispatch_id));
    archive_turn(&project, &recovered).unwrap();
    plan::install(&project, &successor).unwrap();
    let (next, _) = installed(&project, &pool).unwrap().unwrap();
    let (archived, _) = Config::load(&archive).unwrap();
    assert_eq!(archived.central, prior.central);
    assert_eq!(archived.native_digest, prior.native_digest);
    assert_eq!(next.central.generation, 2);
    assert_ne!(next.central.session, prior.central.session);
    assert_ne!(next.central.dispatch_id, prior.central.dispatch_id);
    assert_eq!(next.central.subject, prior.central.subject);
    assert_eq!(next.central.subject.actor_id(), project_control.actor);
    assert_eq!(
        next.central.subject.conversation_id(),
        project.conversation_id
    );
    assert_eq!(next.model.provider.as_str(), "anthropic");
    assert_eq!(next.model.model.as_str(), "claude-opus-4-6");
    assert_eq!(next.subscription.as_ref().unwrap().control_revision, 2);
    assert_eq!(next.swarm_profile.as_ref().unwrap()["harness"], "claude");
    let claude = project.claude.as_ref().unwrap();
    assert_eq!(next.native, claude.native);
    assert_eq!(next.native_digest, claude.native_digest);
    assert_eq!(next.secret_file, claude.secret_file);

    let (other, _) = installed(&global, &pool).unwrap().unwrap();
    assert_eq!(other.central, global_turn.central);
    assert_eq!(
        other.central.subject.conversation_id(),
        global.conversation_id
    );
    assert_eq!(other.swarm_profile.as_ref().unwrap()["harness"], "codex");
    assert_eq!(other.model.provider.as_str(), "openai");
    assert_eq!(other.subscription.as_ref().unwrap().control_revision, 1);
}
