//! Identity and operation lookup.
use super::*;

const fn whoami() -> CommandSpec {
    let mut whoami = read(
        "whoami",
        "",
        TargetRule::Own,
        "The calling principal: attempt, task, project, profile, grants and credential expiry",
    )
    .with_owner(Some(Route::local()));
    whoami.grant = None;
    whoami.supervisor = Some(Route::local());
    whoami.agent = Some(Route::local());
    whoami
}

const fn operation() -> CommandSpec {
    let mut operation = read(
        "operation.show",
        "",
        TargetRule::Own,
        "One operation by ID, with its current disposition (never resends it)",
    )
    .with_owner(Some(Route::get(
        "/api/v1/operations/{id}",
        &[("id", "operation_id")],
    )));
    operation.grant = None;
    operation.supervisor = Some(Route::get(
        "/api/v1/operations/{id}",
        &[("id", "operation_id")],
    ));
    operation.agent = Some(Route::get("/v1/agent/operations/{operation_id}", &[]));
    operation
}

pub(super) const COMMANDS: &[CommandSpec] = &[whoami(), operation()];
