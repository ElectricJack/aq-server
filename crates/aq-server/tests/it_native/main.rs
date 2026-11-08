//! Native publication, preparation, grants, permits, presence and closure.
//!
//! One integration test binary for this area: each former `tests/<name>.rs`
//! binary is a module here, so its tests are named `<name>::...`. Add new
//! tests to an existing module (or a new module listed below), never a new
//! test binary. See AGENTS.md "Build health".

#[path = "../common/mod.rs"]
mod common;

mod delegated_permit_runtime;
mod delegated_permits;
mod granted_closure;
mod native_accepted_preparation;
mod native_cancellation;
mod native_definitions;
mod native_expiry;
mod native_initial_grant;
mod native_inventory;
mod native_material_issuance;
mod native_offers;
mod native_preparation;
mod native_proposal_scope;
mod native_qualification_renewal;
mod native_reconciliation;
mod native_registration;
mod native_result_startup;
// Only runs the unit tests of tests/support/native_runner_transport.rs.
#[allow(clippy::module_inception)]
mod native_runner_transport;
mod native_source;
mod never_granted;
mod permit_fixture_conformance;
mod scoped_presence_inventory;
mod subscription_refresh;
