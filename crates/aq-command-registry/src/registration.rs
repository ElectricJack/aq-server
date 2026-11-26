//! Construction and validation of the command registry.
use super::*;
impl Registry {
    /// Every action kind becomes a mutation command reached by the owner
    /// through the action union. A catalog entry with the same name adds
    /// the other principals' routes and must agree on the grant.
    pub fn new(actions: &[ActionSpec]) -> Result<Self, RegistryError> {
        let mut commands = catalog();

        for action in actions {
            register_action(&mut commands, action)?;
        }
        for command in &mut commands {
            configure_routes(command);
        }
        commands.sort_by(|a, b| a.name.cmp(b.name));
        if let Some(pair) = commands
            .windows(2)
            .find(|pair| pair[0].name == pair[1].name)
        {
            return Err(RegistryError(format!("duplicate {}", pair[0].name)));
        }
        for command in &commands {
            validate(command, actions)?;
        }
        Ok(Self { commands })
    }
    pub fn commands(&self) -> &[CommandSpec] {
        &self.commands
    }
    pub fn get(&self, name: &str) -> Option<&CommandSpec> {
        self.commands.iter().find(|c| c.name == name)
    }
}

fn register_action(commands: &mut Vec<CommandSpec>, a: &ActionSpec) -> Result<(), RegistryError> {
    match commands.iter_mut().find(|c| c.name == a.kind) {
        Some(c) => {
            if (c.grant, c.kind, c.owner) != (Some(a.grant), Kind::Mutation, None) {
                return Err(RegistryError(format!(
                    "{}: catalog and ActionKind disagree",
                    a.kind
                )));
            }
            // The catalog's target is the agent-facing one (an agent
            // comments on its own task); the owner's action derives
            // its own subject.
            c.owner = Some(Route::action());
        }
        None => commands.push(CommandSpec {
            name: a.kind,
            kind: Kind::Mutation,
            grant: Some(a.grant),
            target: a.target,
            summary: action_summary(a.kind).unwrap_or(PLACEHOLDER_SUMMARY),
            owner: Some(Route::action()),
            supervisor: None,
            agent: None,
        }),
    }
    Ok(())
}

fn configure_routes(command: &mut CommandSpec) {
    if matches!(
        command.name,
        "task.edit"
            | "task.reprioritize"
            | "task.role.set"
            | "task.route.set"
            | "task.route.clear"
            | "task.dependency.add"
            | "task.dependency.remove"
    ) {
        command.supervisor = Some(Route::action());
    }
    if is_draft_action(command.name) {
        command.agent = Some(Route::post(
            "/v1/agent/task-actions",
            Body::Action,
            Params::Action,
        ));
    }
    // Every other owner command reaches supervisors through the owner
    // bridge. The leak is bounded: the registry is built once per process.
    #[cfg(feature = "supervisor_full_access")]
    if let (Some(owner), None) = (command.owner, command.supervisor)
        && owner.body != Body::Local
        && !matches!(
            command.name,
            "task.workspace.show"
                | "task.workspace.request"
                | "project.workspace.requests"
                | "project.workspace.decide"
        )
    {
        let path = format!("{OWNER_PREFIX}{}", owner.path);
        command.supervisor = Some(Route {
            path: Box::leak(path.into_boxed_str()),
            ..owner
        });
    }
}

fn validate(c: &CommandSpec, actions: &[ActionSpec]) -> Result<(), RegistryError> {
    if !valid_name(c.name) {
        return Err(RegistryError(format!("invalid name {}", c.name)));
    }
    let routes = [&c.owner, &c.supervisor, &c.agent];
    if routes.iter().all(|r| r.is_none()) {
        return Err(RegistryError(format!("{} has no route", c.name)));
    }
    if routes
        .iter()
        .copied()
        .flatten()
        .any(|r| r.params == Params::Action)
        && !actions.iter().any(|a| a.kind == c.name)
    {
        return Err(RegistryError(format!(
            "{} is not a registered action",
            c.name
        )));
    }
    Ok(())
}
