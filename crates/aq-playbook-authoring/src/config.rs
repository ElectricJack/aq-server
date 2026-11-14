//! `[playbook_authoring]`: every bound of an authoring run that is the
//! server's to choose. The bounds of a draft and of a case are the engine's.
use serde::Deserialize;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// How long an agent may work on one source, from submission.
    pub deadline_ms: u64,
    /// How many turns the agent's harness may take.
    pub max_turns: u32,
    /// Largest `out/outcome.json` read back.
    pub outcome_bytes: u64,
    /// Engine transitions all cases of one save may take together.
    pub case_transitions: usize,
    /// Wall time the server's own check of a save may take.
    pub check_deadline_ms: u64,
    /// Finished runs kept per source; older ones are deleted. The active run
    /// and the one a rollback would bring back are always kept.
    pub keep_finished_runs: u64,
    /// How many times in a row authoring starts again by itself because the
    /// facts changed while the agent worked.
    pub facts_restarts: u32,
    /// How often the server looks for finished or stranded runs.
    pub poll_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            deadline_ms: 1_800_000,
            max_turns: 200,
            outcome_bytes: 4_194_304,
            case_transitions: aq_playbooks_simulation::cases::CASES_TRANSITION_BUDGET,
            check_deadline_ms: 60_000,
            keep_finished_runs: 50,
            facts_restarts: 3,
            poll_ms: 2_000,
        }
    }
}

impl Config {
    /// Whether internal work would accept a request built from this.
    pub fn valid(&self) -> bool {
        aq_internal_work::AGENT_DEADLINE_MS.contains(&self.deadline_ms)
            && aq_internal_work::AGENT_MAX_TURNS.contains(&self.max_turns)
            && aq_internal_work::AGENT_MAX_OUTPUT_BYTES.contains(&self.outcome_bytes)
            && self.case_transitions > 0
            && self.check_deadline_ms > 0
            && self.keep_finished_runs > 0
            && self.poll_ms > 0
    }
}
