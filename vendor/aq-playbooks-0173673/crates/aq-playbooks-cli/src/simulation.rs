use aq_playbooks_model::{Artifact, CommandContract, EventEnvelope};
use aq_playbooks_runner::{Authority, DispatchResult, Engine, Host, Intent, MemoryStorage, State};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Start {
    pub binding: String,
    pub entry: String,
    pub input: Value,
    pub authority: Authority,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Tick {
        at_ms: i64,
    },
    Event {
        at_ms: i64,
        producer: String,
        event: Box<EventEnvelope>,
    },
    Cancel {
        at_ms: i64,
        start: usize,
    },
    Recover {
        at_ms: i64,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub artifacts: Vec<Artifact>,
    pub bindings: BTreeMap<String, String>,
    pub authority: Authority,
    pub starts: Vec<Start>,
    pub mocks: BTreeMap<String, Vec<DispatchResult>>,
    pub actions: Vec<Action>,
    pub max_transitions: usize,
}
#[derive(Default)]
pub struct ScriptedHost {
    pub responses: BTreeMap<String, VecDeque<DispatchResult>>,
    pub invocations: Vec<String>,
}
impl Host for ScriptedHost {
    fn authorize(&self, a: &Authority, c: &CommandContract) -> bool {
        c.capabilities.is_subset(&a.capabilities) && c.scopes.contains(&a.scope)
    }
    fn dispatch(&mut self, i: &Intent) -> DispatchResult {
        self.invocations.push(i.id.clone());
        self.responses
            .get_mut(&i.command.name)
            .and_then(VecDeque::pop_front)
            .unwrap_or_else(|| DispatchResult::Unknown {
                reason: format!("no mock response for {}", i.command.name),
            })
    }
    fn reconcile(&mut self, i: &Intent) -> DispatchResult {
        self.dispatch(i)
    }
}
#[derive(Serialize)]
pub struct SimulationReport {
    pub complete: bool,
    pub coverage: String,
    pub transitions: usize,
    pub roots: Vec<String>,
    pub inspection: Value,
    pub invocations: Vec<String>,
}

pub fn simulate(scenario: &Scenario) -> anyhow::Result<SimulationReport> {
    anyhow::ensure!(
        scenario.max_transitions > 0 && scenario.max_transitions <= 100_000,
        "scenario transition limit must be 1..100000"
    );
    anyhow::ensure!(
        scenario.actions.len() <= 10_000 && scenario.starts.len() <= 100,
        "scenario action/start bound exceeded"
    );
    let engine = Engine::new(
        MemoryStorage::default(),
        "simulation",
        "simulation-incarnation",
    )?;
    for a in &scenario.artifacts {
        engine.install(a.clone())?;
    }
    for (name, digest) in &scenario.bindings {
        engine.activate(name, digest, scenario.authority.clone())?;
    }
    let roots = scenario
        .starts
        .iter()
        .map(|s| {
            engine.admit(
                &s.binding,
                &s.entry,
                s.input.clone(),
                s.authority.clone(),
                0,
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut host = ScriptedHost {
        responses: scenario
            .mocks
            .iter()
            .map(|(k, v)| (k.clone(), v.clone().into()))
            .collect(),
        invocations: vec![],
    };
    let mut used = engine.drive(&mut host, 0, scenario.max_transitions)?;
    let mut clock = 0;
    for action in &scenario.actions {
        if used >= scenario.max_transitions {
            break;
        }
        let at = match action {
            Action::Tick { at_ms }
            | Action::Event { at_ms, .. }
            | Action::Cancel { at_ms, .. }
            | Action::Recover { at_ms } => *at_ms,
        };
        anyhow::ensure!(at >= clock, "simulation clock cannot move backwards");
        clock = at;
        match action {
            Action::Tick { .. } => {}
            Action::Event {
                producer, event, ..
            } => {
                let accepted = engine.admit_event(producer, *event.clone(), clock)?;
                engine.fanout(&accepted.key, 100, clock)?;
            }
            Action::Cancel { start, .. } => {
                engine.cancel(
                    roots
                        .get(*start)
                        .ok_or_else(|| anyhow::anyhow!("unknown start index"))?,
                    clock,
                )?;
            }
            Action::Recover { .. } => {
                engine.recover(&mut host, clock)?;
            }
        }
        used += engine.drive(&mut host, clock, scenario.max_transitions - used)?;
    }
    let state = engine.snapshot()?;
    let complete = state.runs.values().all(|r| r.status.terminal());
    Ok(SimulationReport {
        complete,
        coverage: if !complete && used >= scenario.max_transitions {
            "incomplete: transition limit reached"
        } else if !complete {
            "incomplete: unresolved waits, effects, or missing mocks"
        } else {
            "scripted scenario only; not exhaustive exploration"
        }
        .into(),
        transitions: used,
        roots,
        inspection: inspect(&state, None),
        invocations: host.invocations,
    })
}

/// Public inspection deliberately omits raw inputs, variables and command results.
pub fn inspect(state: &State, run_id: Option<&str>) -> Value {
    let rows = aq_playbooks_runner::inspection::rows(state);
    let values = |kind| {
        rows.iter()
            .filter(|r| r.kind == kind && run_id.is_none_or(|id| r.run == id))
            .map(|r| r.value.clone())
            .collect::<Vec<_>>()
    };
    json!({"runs":values(aq_playbooks_runner::InspectionKind::Runs),"effects":values(aq_playbooks_runner::InspectionKind::Effects),"receipts":values(aq_playbooks_runner::InspectionKind::Receipts),"catalog_revision":state.catalog_revision})
}
