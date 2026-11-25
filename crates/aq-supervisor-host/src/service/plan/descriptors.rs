use super::*;

pub(super) struct Pins {
    agent: ArtifactRef,
    policy: ArtifactRef,
    environment: ArtifactRef,
    adapter: ArtifactRef,
    execution: ArtifactRef,
    selection: ArtifactRef,
    composition: ArtifactRef,
    renderer: ArtifactRef,
    startup_descriptor: ArtifactRef,
}

impl Pins {
    pub(super) fn new(context: &PlanContext<'_>, instructions: &str) -> Result<Self> {
        let (config, control, turn) = (context.config, context.control, context.turn);
        let profile = &control.profile;
        let selected = context.selection;
        let agent = pin(
            config,
            "supervisor-agent",
            &json!({"instructions":instructions,"scope":control.project}),
        )?;
        let policy = ArtifactRef {
            id: Text::new("aq.central/reservation-policy")?,
            version: Text::new("1")?,
            digest: context.generation.policy.clone(),
        };
        let environment = pin(
            config,
            "environment",
            &json!({"image":config.image_digest,"native":selected.native_digest,"native_uid":10001,"private_scope":config.conversation_id,"docker_socket":false}),
        )?;
        let adapter = pin(
            config,
            selected.adapter,
            &json!({"version":1,"cli":selected.cli_version,"native":selected.native_digest,"mode":"subscription-exec","final_reply":"generation-dispatch-bound"}),
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
        Ok(Self {
            agent,
            policy,
            environment,
            adapter,
            execution,
            selection,
            composition,
            renderer,
            startup_descriptor,
        })
    }

    pub(super) fn host(&self, context: &PlanContext<'_>) -> Result<HostPlan> {
        let (config, control, turn) = (context.config, context.control, context.turn);
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
        let clock = crate::clock()?;
        let harness = HarnessSettings {
            harness: Text::new(context.selection.harness)?,
            cli_version: Text::new(context.selection.cli_version)?,
            adapter: self.adapter.clone(),
            environment: self.environment.clone(),
            required_capabilities: serde_json::from_value(
                json!([{"id":"aq.harness.startup-context","version":1},{"id":"aq.harness.confined-worktree","version":1},{"id":"aq.harness.stop","version":1},{"id":"aq.terminal","version":1}]),
            )?,
        };
        Ok(HostPlan {
            host_plan_version: 1,
            subject: HostSubject::CentralConversation {
                actor_id: control.actor,
                conversation_id: config.conversation_id,
                conversation_revision: 1,
            },
            session: context.session(),
            policy: self.policy.clone(),
            execution: self.execution.clone(),
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
                descriptor: self.startup_descriptor.clone(),
                payload,
                bytes: turn.input.len() as u64,
            },
            machine_boot_id: clock.machine_boot_id,
            admitted_boot_ms: clock.boot_ms,
            deadline_boot_ms: clock.boot_ms + 3600000,
        })
    }

    pub(super) fn central(self, context: &PlanContext<'_>, host: &HostPlan) -> Result<CentralPlan> {
        let (config, control, turn) = (context.config, context.control, context.turn);
        let Self {
            agent,
            policy,
            environment,
            adapter,
            execution,
            selection,
            composition,
            renderer,
            startup_descriptor,
        } = self;
        let host_reference = pin(config, "host-plan", &json!(host))?;
        let host_reference = ArtifactRef {
            digest: host.digest()?,
            ..host_reference
        };
        // The role snapshot the saved profile was chosen from. The pin keeps the
        // field name `catalog` of the stored central plan contract.
        let catalog = ArtifactRef {
            id: Text::new("aq.owner/role-snapshot")?,
            version: Text::new("1")?,
            digest: serde_json::from_value(control.profile["snapshot_digest"].clone())
                .context("saved supervisor profile names no role snapshot; choose it again")?,
        };
        Ok(CentralPlan {
            schema_version: 1,
            subject: CentralSubject::CentralConversation {
                actor_id: control.actor,
                conversation_id: config.conversation_id,
                conversation_revision: 1,
                policy,
            },
            generation: context.generation.number,
            session: context.session(),
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
            batch: turn.batch.clone(),
            dispatch_id: turn.dispatch_id,
            host_operation_id: turn.host_operation_id,
            wall_time_ms: 3600000,
            max_permit_ms: 120000,
        })
    }
}
