use super::*;

fn valid_bounded(value: &str, limit: usize) -> bool {
    !value.is_empty() && value.len() <= limit && !value.contains('\0')
}

pub(super) fn valid(value: &str) -> bool {
    valid_bounded(value, 512)
}

fn valid_page(after: Option<&str>, limit: usize, max: usize) -> bool {
    (1..=max).contains(&limit) && after.is_none_or(valid)
}

fn valid_inspection(query: &runner::InspectionQuery) -> bool {
    query.validate().is_ok() && query.limit <= 32
}

fn authorized(binding: &str, target: &str, authority: &Authority) -> bool {
    valid(binding) && valid(target) && valid_authority(authority)
}

fn valid_authority(authority: &Authority) -> bool {
    valid(&authority.actor)
        && valid(&authority.scope)
        && authority.capabilities.len() <= 64
        && authority
            .capabilities
            .iter()
            .all(|capability| valid(capability))
}

impl Request {
    pub(super) fn validate(&self) -> Result<(), Error> {
        let ok = match self {
            Self::AdmitOnce { request, .. } => {
                valid(&request.key)
                    && authorized(&request.binding, &request.entry, &request.authority)
            }
            Self::Admission { key } => valid_bounded(key, 256),
            Self::Binding { binding } => valid(binding),
            Self::Event {
                authenticated_producer,
                ..
            } => valid(authenticated_producer),
            Self::Fanout {
                event_key, limit, ..
            } => valid_page(Some(event_key), *limit, 64),

            Self::EventStatus { event_key } => valid(event_key),
            Self::Inspect { query } => valid_inspection(query),
            Self::Activate {
                binding,
                artifact,
                authority: a,
            }
            | Self::ActivateAt {
                binding,
                artifact,
                authority: a,
                ..
            } => authorized(binding, artifact, a),
            Self::ConfigureBinding {
                binding,
                artifact,
                authority: a,
                schedules,
                ..
            } => {
                authorized(binding, artifact, a)
                    && model::schedules::BindingSchedule::validate_all(schedules).is_ok()
            }
            Self::DeactivateBinding {
                binding, revision, ..
            } => valid(binding) && *revision > 0,

            Self::Admit {
                binding,
                entry,
                authority: a,
                ..
            } => authorized(binding, entry, a),
            Self::Runs { after, limit } => valid_page(after.as_deref(), *limit, 100),
            Self::Cancel { run, .. } => valid(run),
            Self::ReconcileDelivery {
                intents,
                scan_after,
                ..
            } => {
                intents.len() <= 4
                    && intents.iter().all(|id| valid(id))
                    && scan_after.as_deref().is_none_or(valid)
            }
            Self::Install { .. }
            | Self::Recover { .. }
            | Self::CancelEffects { .. }
            | Self::CollectIntervalEvents { .. }
            | Self::Tick { .. }
            | Self::Stop => true,
        };
        ok.then_some(()).ok_or(Error::Invalid)
    }
}

fn supported_command(name: &str) -> bool {
    // Base host commands through catalog 2.4.0.
    const COMMANDS: [&str; 17] = [
        "work.read",
        "work.route",
        "work.unroutable",
        "judgment.assess",
        "context.read",
        "work.ensure",
        "work.change",
        "work.activate",
        "judgment.request",
        "decision.request",
        "check.request",
        "candidate.prepare",
        "candidate.publish",
        "message.send",
        "findings.reconcile",
        "operation.get",
        "operation.cancel",
    ];
    // Host catalog 2.3.0 (Epic 0, spec 0016): the ten delivery commands, only
    // with the cargo feature `delivery_playbook`.
    const DELIVERY: [&str; 10] = [
        "work.complete",
        "work.return",
        "work.hold",
        "supervisor.notify",
        "main.sync",
        "branch.publish",
        "pull_request.open",
        "pull_request.await_checks",
        "pull_request.merge",
        "pull_request.close",
    ];
    let delivery = cfg!(feature = "delivery_playbook");
    // Host catalog 2.5.0 (spec 0017), with both features.
    let environments = delivery && cfg!(feature = "project_environments");
    COMMANDS.contains(&name)
        || (delivery && DELIVERY.contains(&name))
        || (environments && ["environment.build", "environment.admit"].contains(&name))
}

fn valid_command(name: &str, command: &CommandContract) -> bool {
    let supported = supported_command(name) || plugins::extension_command(name, command);
    let recoverable = matches!(
        command.recovery,
        model::Recovery::Observation
            | model::Recovery::KeyedIdempotent
            | model::Recovery::Reconcilable
    );
    supported && name == command.name && recoverable
}

fn catalog_size_valid(registry: &Registry) -> bool {
    let base_event_count = registry
        .events
        .keys()
        .filter(|name| plugins::command_plugin(name).is_none())
        .count();
    let has_extensions = registry
        .commands
        .keys()
        .any(|name| plugins::command_plugin(name).is_some())
        || registry.events.len() != base_event_count;
    let byte_limit = if has_extensions { 1_048_576 } else { 262_144 };
    base_event_count <= 16 && bounded_json(registry, byte_limit)
}

pub(super) fn validate_registry(registry: &Registry) -> Result<(), Error> {
    if !catalog_size_valid(registry) {
        return Err(Error::Invalid);
    }
    let commands_valid = registry
        .commands
        .iter()
        .all(|(name, c)| valid_command(name, c));
    if !commands_valid {
        return Err(Error::Invalid);
    }
    let events_valid = registry.events.iter().all(|(name, c)| name == &c.name);
    events_valid.then_some(()).ok_or(Error::Invalid)
}
