use super::*;
use aq_central_authority::{
    ArtifactRef, CentralPlan, CentralSubject, Digest, ProfilePins, SessionIdentity, Text,
};
use aq_session_host::{HostInput, HostPlan, HostStartup, HostSubject};
use aq_session_protocol::{HarnessSettings, ModelSettings};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
fn pin(config: &ServiceConfig, name: &str, value: &Value) -> Result<ArtifactRef> {
    let bytes = serde_json::to_vec(value)?;
    let digest = Digest::of(&bytes);
    let directory = config.root.join("descriptors");
    std::fs::create_dir_all(&directory)?;
    let path = directory.join(format!("{}-{}.json", name, digest.as_str()));
    if !path.exists() {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?
            .write_all(&bytes)?;
    }
    Ok(ArtifactRef {
        id: Text::new(format!("aq.central/{name}"))?,
        version: Text::new("1")?,
        digest,
    })
}
pub(super) fn assemble(
    config: &ServiceConfig,
    control: &Control,
    policy_digest: &Digest,
    generation: u64,
    session_id: Uuid,
    turn: dispatch::Turn,
) -> Result<Config> {
    let profile = &control.profile;
    let model = profile["model"]
        .as_str()
        .context("saved supervisor model")?;
    let effort = profile["reasoning"]
        .as_str()
        .context("saved supervisor effort")?;
    ensure!(
        profile["provider"] == "openai"
            && profile["harness"] == "codex"
            && ["low", "medium", "high"].contains(&effort),
        "installed central Codex settings are unavailable"
    );
    let scope = if let Some(project) = control.project {
        format!(
            "You supervise only project {} ({project}). The server restricts your commands to that project. Do not attempt to read or change other projects.",
            control.label
        )
    } else {
        "You are the global supervisor and may coordinate registered projects. Use project.list to discover project identifiers.".into()
    };
    let instructions = format!(
        "{scope}\nYou manage AQ work through authenticated tools. This private central workspace is not a project checkout: delegate implementation to worker tasks instead of editing files here.\nUse /usr/local/bin/aq-supervisor-host tool commands to discover commands, or tool commands NAME for one command's schema. To invoke one, run /usr/local/bin/aq-supervisor-host tool NAME with one JSON CommandRequest on stdin, e.g. {{\"params\":{{\"project_id\":\"UUID\"}}}} for task.list. Use project.list for project identifiers and revisions; task.list and task.show expose current task versions. Mutations also need a fresh operation_id, a subject and the current expected versions; inspect the command schema and current project/task before writing. Retain the same operation_id and exact body when checking an uncertain request; never invent success or replay a replacement.\nCreate draft tasks for proposals and QA, and active tasks only when the owner asks for execution. Include requirements, acceptance criteria and dependencies. Distinguish queued work, active attempts, retained results and actual completion. Workers should report completion/blockers to supervisor conversation {} through their AQ message tool. Respond when these messages arrive and inspect actual state. Do not claim you will monitor work without an admitted wait or future event.\nYour final answer is published automatically to this conversation with any confirmed task creation receipts. You may return ordinary concise text; do not print credentials or read authentication files. AQ credential paths are for the tool implementation only. Do not merge, deploy, alter host services, credentials, pools or owner-only configuration. Respect configured human review gates.\n",
        config.conversation_id
    );
    let agent = pin(
        config,
        "supervisor-agent",
        &json!({"instructions":instructions,"scope":control.project}),
    )?;
    let policy = ArtifactRef {
        id: Text::new("aq.central/reservation-policy")?,
        version: Text::new("1")?,
        digest: policy_digest.clone(),
    };
    let environment = pin(
        config,
        "environment",
        &json!({"image":config.image_digest,"native":config.native_digest,"native_uid":10001,"private_scope":config.conversation_id,"docker_socket":false}),
    )?;
    let adapter = pin(
        config,
        "codex-subscription-adapter",
        &json!({"version":1,"cli":"0.166.0","native":config.native_digest,"mode":"subscription-exec","final_reply":"generation-dispatch-bound"}),
    )?;
    let execution = pin(
        config,
        "execution",
        &json!({"cpu_millis":500,"memory_bytes":2147483648_u64,"process_limit":512,"wall_time_ms":3600000,"stop_grace_ms":10000}),
    )?;
    let selection = pin(
        config,
        "intelligence-selection",
        &json!({"control_revision":control.revision,"profile":profile}),
    )?;
    let composition = pin(
        config,
        "composition",
        &json!({"agent":agent,"execution":execution,"selection":selection,"policy":policy}),
    )?;
    let renderer = pin(
        config,
        "renderer",
        &json!({"version":1,"format":"immutable-supervisor-mail-and-history"}),
    )?;
    let startup_descriptor = pin(
        config,
        "startup",
        &json!({"renderer":renderer,"input_digest":Digest::of(&turn.input),"batch":turn.batch,"scope":control.project}),
    )?;
    let payload = ArtifactRef {
        id: Text::new("aq.central/startup-payload")?,
        version: Text::new("1")?,
        digest: Digest::of(&turn.input),
    };
    let batch = pin(config, "mail-batch", &json!(turn.batch))?;
    let batch = ArtifactRef {
        digest: turn.batch.digest()?,
        ..batch
    };
    let session = SessionIdentity::Central {
        authority_id: config.authority_id,
        session_id,
        generation,
    };
    let clock = crate::clock()?;
    let harness = HarnessSettings {
        harness: Text::new("codex")?,
        cli_version: Text::new("0.166.0")?,
        adapter: adapter.clone(),
        environment: environment.clone(),
        required_capabilities: serde_json::from_value(
            json!([{"id":"aq.harness.startup-context","version":1},{"id":"aq.harness.confined-worktree","version":1},{"id":"aq.harness.stop","version":1},{"id":"aq.terminal","version":1}]),
        )?,
    };
    let host = HostPlan {
        host_plan_version: 1,
        subject: HostSubject::CentralConversation {
            actor_id: control.actor,
            conversation_id: config.conversation_id,
            conversation_revision: 1,
        },
        session: session.clone(),
        policy: policy.clone(),
        execution: execution.clone(),
        input: HostInput {
            dispatch_id: turn.dispatch_id,
            host_operation_id: turn.host_operation_id,
            batch,
        },
        harness,
        resources: serde_json::from_value(
            json!({"cpu_millis":500,"memory_bytes":2147483648_u64,"process_limit":512,"wall_time_ms":3600000,"stop_grace_ms":10000}),
        )?,
        terminal: serde_json::from_value(
            json!({"host":"tmux","input_enabled":false,"max_frame_bytes":65536,"max_input_bytes":4096,"max_pending_input_bytes":32768,"replay_bytes":1048576}),
        )?,
        startup: HostStartup {
            descriptor: startup_descriptor.clone(),
            payload,
            bytes: turn.input.len() as u64,
        },
        machine_boot_id: clock.machine_boot_id,
        admitted_boot_ms: clock.boot_ms,
        deadline_boot_ms: clock.boot_ms + 3600000,
    };
    let host_reference = pin(config, "host-plan", &json!(host))?;
    let host_reference = ArtifactRef {
        digest: host.digest()?,
        ..host_reference
    };
    let catalog = ArtifactRef {
        id: Text::new("aq.owner/profile-catalog")?,
        version: Text::new("1")?,
        digest: serde_json::from_value(profile["catalog_digest"].clone())?,
    };
    let central = CentralPlan {
        schema_version: 1,
        subject: CentralSubject::CentralConversation {
            actor_id: control.actor,
            conversation_id: config.conversation_id,
            conversation_revision: 1,
            policy,
        },
        session,
        generation,
        profiles: ProfilePins {
            catalog,
            agent,
            intelligence: selection.clone(),
            execution,
            composition,
            selection,
            host_plan: host_reference,
            adapter,
            environment,
            renderer,
            startup: startup_descriptor,
        },
        batch: turn.batch,
        dispatch_id: turn.dispatch_id,
        host_operation_id: turn.host_operation_id,
        wall_time_ms: 3600000,
        max_permit_ms: 120000,
    };
    let model = ModelSettings {
        provider: Text::new("openai")?,
        model: Text::new(model)?,
        reasoning: Text::new(effort)?,
        max_output_tokens: 32768,
        spending_budget: None,
    };
    let resource_admission = pin(
        config,
        "container-admission",
        &json!({"image":config.image_digest,"conversation_id":config.conversation_id,"cpu_millis":500,"memory_bytes":2147483648_u64,"process_limit":512}),
    )?;
    let startup_file = config
        .root
        .join(format!("input-{generation}-{}.bin", central.dispatch_id));
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&startup_file)?
        .write_all(&turn.input)?;
    let result = Config {
        database_url: config.database_url.clone(),
        root: config.root.clone(),
        workspace: config.workspace.clone(),
        helper: config.helper.clone(),
        tmux: config.tmux.clone(),
        tmux_version: config.tmux_version.clone(),
        native: config.native.clone(),
        native_digest: config.native_digest.clone(),
        homes: config.homes.clone(),
        secret_file: config.secret_file.clone(),
        model,
        central,
        host,
        startup_file,
        resource_admission,
        swarm_profile: Some(profile.clone()),
        subscription: Some(crate::config::Subscription {
            server_origin: config.server_origin.clone(),
            instructions,
            control_revision: control.revision,
            options: serde_json::from_value(profile.get("options").cloned().unwrap_or(json!({})))?,
        }),
    };
    crate::validate_join(&result.central, &result.host)?;
    Ok(result)
}
pub(super) fn install(config: &ServiceConfig, turn: &Config) -> Result<()> {
    let path = config.turn_file();
    ensure!(!path.exists(), "current supervisor turn is retained");
    let bytes = serde_json::to_vec(turn)?;
    ensure!(bytes.len() <= 65536, "installed supervisor config bound");
    let temp = config.root.join(format!("turn-{}.tmp", Uuid::new_v4()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    std::fs::rename(temp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn assembled_subscription_plan_satisfies_the_frozen_host_contract() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_path_buf();
        let examples: Value = serde_json::from_str(include_str!(
            "../../../aq-central-authority/artifacts/examples.json"
        ))
        .unwrap();
        let aq_central_authority::Document::Plan(prior) =
            aq_central_authority::decode(&serde_json::to_vec(&examples["plan"]).unwrap()).unwrap()
        else {
            panic!("plan fixture")
        };
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
        };
        let control = Control {
            actor: prior.subject.actor_id(),
            enabled: true,
            revision: 1,
            profile: json!({"provider":"openai","harness":"codex","model":"fixture-model","reasoning":"medium","catalog_digest":Digest::of(b"catalog"),"profile_id":"engineering","profile_version":1,"reasoning_configuration_reference":"medium"}),
            owner: Some(Uuid::new_v4()),
            project: None,
            label: "Global supervisor".into(),
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
            &Digest::of(b"policy"),
            1,
            Uuid::new_v4(),
            turn,
        )
        .unwrap();
        candidate.host.digest().unwrap();
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
}
