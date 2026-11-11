//! Server binary, configuration, auth, enrollment, execution, readiness and updates.
//!
//! One integration test binary for this area: each former `tests/<name>.rs`
//! binary is a module here, so its tests are named `<name>::...`. Add new
//! tests to an existing module (or a new module listed below), never a new
//! test binary. See AGENTS.md "Build health".

#[path = "../common/mod.rs"]
mod common;

mod authz;
mod cli;
mod commands;
mod config;
mod enrollment;
mod execution;
mod health;
mod identity;
mod manual_agents;
mod readiness;
mod updates;
