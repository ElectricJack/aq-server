//! Store, schema migrations, migration catalog and row retention.
//!
//! One integration test binary for this area: each former `tests/<name>.rs`
//! binary is a module here, so its tests are named `<name>::...`. Add new
//! tests to an existing module (or a new module listed below), never a new
//! test binary. See AGENTS.md "Build health".

// Some tests/support and tests/common fixture files are path-included by more
// than one module (each module was its own test binary before 2026-11-05).
#![allow(clippy::duplicate_mod)]

#[path = "../common/mod.rs"]
mod common;

mod host9_routing_migration;
mod migration_catalog;
mod playbook_authoring_migration;
mod playbook_rolling_records_migration;
mod presence_rolling_records_migration;
mod rate_limit_removal_migration;
mod result_origin_migration;
mod roles_and_profiles_migration;
mod routing_policies_migration;
mod row_retention;
mod schema26_27;
mod schema28_closure;
mod schema29_presence;
mod schema30_preparation;
mod schema31_presence_barrier;
mod schema32_phase_execution;
mod schema33_phase_terminal;
mod schema34_human_graph;
mod schema35_supervisor;
mod schema36_media;
mod schema41_compute;
mod schema43_cancellation;
mod sealed_history_live;
mod settlement_migration;
mod store;
mod task_holds_migration;

mod task_git_authors;
