//! Real private Git export/import + phase/source/result SQL owners. Host proof,
//! material installation and Graph observations remain explicit synthetic facts.
#[path = "phase_source_retention/git.rs"]
mod actual_git;
#[path = "support/actual_git_owner.rs"]
mod actual_git_owner;
mod common;
#[allow(dead_code)]
#[path = "common/phase_terminal.rs"]
mod frozen_fixture;
#[allow(dead_code)]
#[path = "common/phase_refresh_terminal.rs"]
mod refresh_fixture;
mod frozen {
    const PROFILE: aq_admission_sandbox::sandbox::PhaseProfile =
        aq_admission_sandbox::sandbox::PhaseProfile::PhaseV2;
    use super::frozen_fixture as terminal;
    use aq_server::profiled_execution::phase_terminal as terminal_owner;
    include!("phase_source_retention/case.rs");
}
mod refresh {
    const PROFILE: aq_admission_sandbox::sandbox::PhaseProfile =
        aq_admission_sandbox::sandbox::PhaseProfile::PhaseRefreshV1;
    use super::refresh_fixture as terminal;
    use aq_server::profiled_execution::phase_refresh_terminal as terminal_owner;
    include!("phase_source_retention/case.rs");
}
