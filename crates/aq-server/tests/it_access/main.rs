//! Agent access, messaging, graph mutations, intelligence, worker API and observations.
//!
//! One integration test binary for this area: each former `tests/<name>.rs`
//! binary is a module here, so its tests are named `<name>::...`. Add new
//! tests to an existing module (or a new module listed below), never a new
//! test binary. See AGENTS.md "Build health".

#[path = "../common/mod.rs"]
mod common;

mod agent_access;
mod agent_commands;
mod agent_route_grants;
mod agent_task_comments;
mod graph_mutations;
mod image_labels;
mod intelligence;
mod machine_store;
mod messaging;
mod observation_owner_peer;
mod observation_transport;
mod observations;
mod scheduling_reads;
mod stall_ladder;
mod worker_api;
mod worker_auth_context;
