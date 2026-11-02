//! Actual phase/refresh issuance + terminal + B1 owners. Synthetic proof DTOs,
//! source/private providers and task observations; no kernel/provider/run claim.
mod common;
#[allow(dead_code)]
#[path = "common/phase_terminal.rs"]
mod frozen_fixture;
#[allow(dead_code)]
#[path = "common/phase_refresh_terminal.rs"]
mod refresh_fixture;
mod frozen {
    use super::frozen_fixture as terminal;
    use aq_server::profiled_execution::phase_terminal as terminal_owner;
    const ORIGIN: &str = "phase_execution_v2";
    include!("phase_results/cases.rs");
    mod human_metadata {
        use super::*;
        include!("phase_results/human_results.rs");
    }
}
mod refresh {
    use super::refresh_fixture as terminal;
    use aq_server::profiled_execution::phase_refresh_terminal as terminal_owner;
    const ORIGIN: &str = "phase_refresh_execution_v1";
    include!("phase_results/cases.rs");
    mod human_metadata {
        use super::*;
        include!("phase_results/human_results.rs");
    }
}
