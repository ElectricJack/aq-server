//! Dashboard, human work, supervisor, sessions, terminal relay and notifications.
//!
//! One integration test binary for this area: each former `tests/<name>.rs`
//! binary is a module here, so its tests are named `<name>::...`. Add new
//! tests to an existing module (or a new module listed below), never a new
//! test binary. See AGENTS.md "Build health".

#[path = "../common/mod.rs"]
mod common;

mod central_sessions;
mod dashboard_actions;
mod dashboard_aggregation;
mod dashboard_assets;
mod dashboard_errors;
mod dashboard_mounts;
mod dashboard_notifications;
mod dashboard_read;
mod dashboard_search;
mod dashboard_swarm;
mod dashboard_task_knowledge;
mod dashboard_usage;
mod dashboard_worker_actions;
mod human_graph;
mod human_task_comments;
mod human_task_create;
mod human_work;
mod notification_discord_live;
mod notification_policy;
mod notification_store;
mod session_inspection;
mod supervisor_conversation;
mod supervisor_turns;
mod terminal_output_poll;
mod terminal_store;
