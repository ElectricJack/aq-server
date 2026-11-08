//! Profiled execution: artifacts, authority, results, source, started/stopped, replacement, retention.
//!
//! One integration test binary for this area: each former `tests/<name>.rs`
//! binary is a module here, so its tests are named `<name>::...`. Add new
//! tests to an existing module (or a new module listed below), never a new
//! test binary. See AGENTS.md "Build health".

#[path = "../common/mod.rs"]
mod common;

mod lifecycle_artifacts;
mod profile_catalog;
mod profile_inspection;
mod profiled_artifact_phases;
mod profiled_artifacts;
mod profiled_authority;
mod profiled_config;
mod profiled_context;
mod profiled_hot;
mod profiled_material;
mod profiled_results;
mod profiled_source;
mod profiled_source_exports;
mod profiled_started;
mod profiled_stopped;
mod replacement_handoff;
mod replacement_policy_limits;
mod replacement_release;
mod replacement_source;
mod source_retention;
mod stopped_dispositions;
