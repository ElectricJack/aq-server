//! Playbooks, project bindings, repository onboarding and real adapter pilots.
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

mod actual_codex_task;
mod actual_owner_setup;
// Only runs the unit tests of tests/support/codex_finite_provider.rs.
#[allow(clippy::module_inception)]
mod codex_finite_provider;
// Only runs the unit tests of tests/support/codex_runtime_recipe.rs.
#[allow(clippy::module_inception)]
mod codex_runtime_recipe;
mod playbook_activation;
mod playbook_authoring;
mod playbook_bridge;
mod playbook_coding;
mod playbook_contract;
mod playbook_findings_triggers;
mod playbook_host_adapters;
mod playbook_host_integration;
mod playbook_host_requests;
mod playbook_integration_startup;
mod playbook_isolation;
mod playbook_review_contract;
mod playbook_work_completion;
mod project_bindings;
mod real_adapter_pilots;
mod real_graph_fixture;
mod repository_onboarding;
