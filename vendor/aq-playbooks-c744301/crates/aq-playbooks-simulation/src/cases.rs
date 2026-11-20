//! Cases: a situation in plain words and in data, and what must follow.
//!
//! A case says how a run starts (an event, or an entry with its input), what
//! each command the playbook calls returns, in call order, and what must
//! follow: the outcome the run ends with and the calls it makes, with their
//! inputs. [`run_case`] runs a sealed artifact on a fresh in-memory engine
//! with a host that answers from the case, records every call with its
//! input, and reads the run's outcome through public inspection (a finished
//! run has left the engine's state; spec 0003). Nothing is read from the
//! network, a clock or a file.
//!
//! - An answer to a synchronous command is its result.
//! - An answer to an asynchronous command (such as `judgment.assess`) is
//!   returned as `accepted` with the handle `case-<n>`, and its completion
//!   event is then admitted with `{outcome, output}` as its payload.
//! - `{"pending": true}` accepts an asynchronous command and never answers
//!   it: the clock moves on to each deadline, so the playbook's timeout path
//!   runs. A run that has returned and waits only for such an operation to
//!   settle ends, for the case, with the outcome it returned.
//! - A call the case gives no return for fails the case, naming the step.
//! - A call the case does not list fails the case, and so does a listed
//!   call the run does not make: `expect.calls` is every call the run makes,
//!   in order, and each `input` must equal what the playbook sent. A return
//!   the run never uses fails the case too.
//! - A case starts from any event a trigger of the playbook names, with its
//!   payload and the values of the event's correlation fields
//!   (`start.correlation`), or from an entry with its input.
//! - Events are built from their contract: its first trusted producer, ids
//!   `case-<i>`, the playbook's scope, and the subject and revision read
//!   from the payload at the contract's subject and revision fields.
//! - [`run_cases`] runs a save's cases within one total transition budget
//!   ([`CASES_TRANSITION_BUDGET`] by default) as well as the per-case cap.
//!
//! [`Outcome`] is what an agent's tools write. Every field of it is the
//! agent's claim: a reader re-derives each one (seals the draft again from
//! its own inputs, re-runs the cases, re-quotes each sentence from its own
//! copy of the document with [`quote`], recomputes the digests) and never
//! stores the agent's text or digests as fact.
use aq_playbooks_model::{Artifact, ContractRef, EventEnvelope, Operation, Registry, digest};
use aq_playbooks_runner::{
    Authority, DispatchResult, Engine, Host, InspectionKind, InspectionQuery, Intent, MemoryStorage,
};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// A save carries at least this many cases, and at most [`MAX_CASES`].
pub const MIN_CASES: usize = 5;
pub const MAX_CASES: usize = 40;
pub const MAX_CASE_NAME_BYTES: usize = 128;
pub const MAX_SITUATION_BYTES: usize = 1024;
/// Returns given for one command in one case.
pub const MAX_RETURNS_PER_COMMAND: usize = 64;
/// Engine transitions one case may take.
pub const MAX_CASE_TRANSITIONS: usize = 10_000;
/// Engine transitions all the cases of one save may take together.
pub const CASES_TRANSITION_BUDGET: usize = 40_000;
/// Times the clock may move on to a deadline in one case.
const MAX_CASE_ROUNDS: usize = 256;

/// The outcome file the authoring agent's `save`, `ask` and `unable` write,
/// relative to the workspace, and the most bytes it may have: 4 MiB. A
/// saved outcome holds the draft and every case, and a case spells out each
/// call with its whole input, so 40 routing cases and their draft come to
/// about 0.6 MiB (`a_forty_case_routing_save_fits_the_outcome_bound` in the
/// CLI's tests). A host reads at most this many bytes, plus one to notice a
/// longer file.
pub const OUTCOME_FILE: &str = "out/outcome.json";
pub const MAX_OUTCOME_BYTES: usize = 4_194_304;
pub const MAX_QUESTION_BYTES: usize = 4096;
pub const MAX_REASON_BYTES: usize = 4096;
/// One question names at most this many sentences.
pub const MAX_SENTENCES: usize = 16;
pub const MAX_SENTENCE_BYTES: usize = 8192;
/// The workspace files the host writes, which an `unable` may name instead
/// of a sentence when the fault is in one of them.
pub const HOST_FILES: [&str; 4] = [
    "record.json",
    "catalog.json",
    "facts.json",
    "extensions.json",
];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    /// Short and unique among the cases saved together.
    pub name: String,
    /// The situation in the owner's words, such as "a bug fix that failed once".
    pub situation: String,
    pub start: CaseStart,
    /// Per command name, what each call returns, in call order.
    #[serde(default)]
    pub returns: BTreeMap<String, Vec<CaseReturn>>,
    pub expect: Expectation,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CaseStart {
    /// The run is started by this event, through the playbook's trigger on it.
    Event {
        event: String,
        payload: Value,
        #[serde(default)]
        correlation: BTreeMap<String, String>,
    },
    /// The run is started at this entry with this input.
    Entry { entry: String, input: Value },
}
/// `{"outcome": …, "output": …}`, or `{"pending": true}` for no answer.
/// For an asynchronous command, `{"outcome": …, "output": …, "at_once":
/// true}` answers the call itself with one of the command's own outcomes
/// other than `accepted` (such as `refused` at admission), instead of
/// accepting it and answering through the completion event.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum CaseReturn {
    Answer {
        outcome: String,
        output: Value,
    },
    AtOnce {
        outcome: String,
        output: Value,
        at_once: bool,
    },
    Pending {
        pending: bool,
    },
}
// Written by hand so a wrong return says which shape is accepted.
impl<'de> Deserialize<'de> for CaseReturn {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let shape = "a return is an answer, {\"outcome\": \"<outcome>\", \"output\": {…}}, an answer an asynchronous command gives at once, {\"outcome\": \"<outcome>\", \"output\": {…}, \"at_once\": true}, or no answer, {\"pending\": true}";
        let map = serde_json::Map::<String, Value>::deserialize(d)?;
        let keys: Vec<&str> = map.keys().map(String::as_str).collect();
        match keys.as_slice() {
            ["outcome", "output"] => match &map["outcome"] {
                Value::String(outcome) => Ok(Self::Answer {
                    outcome: outcome.clone(),
                    output: map["output"].clone(),
                }),
                other => Err(D::Error::custom(format!(
                    "outcome must be a string, found {other}; {shape}"
                ))),
            },
            ["at_once", "outcome", "output"] => match (&map["outcome"], &map["at_once"]) {
                (Value::String(outcome), Value::Bool(at_once)) => Ok(Self::AtOnce {
                    outcome: outcome.clone(),
                    output: map["output"].clone(),
                    at_once: *at_once,
                }),
                _ => Err(D::Error::custom(format!(
                    "outcome must be a string and at_once true; {shape}"
                ))),
            },
            ["pending"] => match &map["pending"] {
                Value::Bool(pending) => Ok(Self::Pending { pending: *pending }),
                other => Err(D::Error::custom(format!(
                    "pending must be true, found {other}; {shape}"
                ))),
            },
            _ => Err(D::Error::custom(format!(
                "found the fields {keys:?}; {shape}"
            ))),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expectation {
    /// The entry outcome the run must end with.
    pub outcome: String,
    /// Every call the run makes, in order, each with its whole input.
    /// Required: a case that expects no call writes `[]`.
    pub calls: Vec<ExpectedCall>,
}
/// One call the run must make: its command, and its whole input, which
/// must equal what the playbook sent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectedCall {
    pub command: String,
    pub input: Value,
}
/// A command the run called: its step, its input, and the outcome the case
/// answered with (`None` when the case gave no answer).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Call {
    pub step: String,
    pub command: String,
    pub input: Value,
    pub outcome: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaseReport {
    pub name: String,
    pub situation: String,
    pub passed: bool,
    /// The outcome the run ended with, read through inspection.
    pub outcome: Option<String>,
    pub calls: Vec<Call>,
    /// Each way the run differs from the case, or the case is malformed.
    pub problems: Vec<String>,
}

fn names<'a>(items: impl IntoIterator<Item = &'a str>) -> String {
    let items: Vec<&str> = items.into_iter().collect();
    if items.is_empty() {
        "none".into()
    } else {
        items.join(", ")
    }
}
fn clip(value: &Value) -> String {
    let text = value.to_string();
    if text.len() <= 240 {
        return text;
    }
    let mut end = 240;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}
fn bounded(field: &str, text: &str, max: usize, problems: &mut Vec<String>) {
    if text.trim().is_empty() {
        problems.push(format!("{field}: is empty; write it in the owner's words."));
    } else if text.len() > max {
        problems.push(format!(
            "{field}: is {} bytes; keep it to {max}.",
            text.len()
        ));
    }
}

/// The commands the playbook's steps invoke.
fn invoked(artifact: &Artifact) -> BTreeSet<&str> {
    artifact
        .content
        .playbook
        .steps
        .values()
        .filter_map(|s| match &s.op {
            Operation::Invoke { command, .. } => Some(command.name.as_str()),
            _ => None,
        })
        .collect()
}

/// What is wrong with the case itself, before it is run.
fn check_case(artifact: &Artifact, case: &Case) -> Vec<String> {
    let mut problems = vec![];
    let p = &mut problems;
    bounded("name", &case.name, MAX_CASE_NAME_BYTES, p);
    bounded("situation", &case.situation, MAX_SITUATION_BYTES, p);
    let playbook = &artifact.content.playbook;
    let registry = &artifact.content.registry;
    let called = invoked(artifact);
    let entries: BTreeSet<&str> = match &case.start {
        CaseStart::Entry { entry, .. } => {
            if !playbook.entries.contains_key(entry) {
                p.push(format!(
                    "start.entry: the playbook has no entry {entry:?}; its entries are: {}.",
                    names(playbook.entries.keys().map(String::as_str))
                ));
            }
            BTreeSet::from([entry.as_str()])
        }
        CaseStart::Event {
            event, correlation, ..
        } => {
            if let Some(contract) = registry.events.get(event) {
                let fields = &contract.correlation_fields;
                let missing: Vec<&str> = fields
                    .iter()
                    .filter(|f| correlation.get(*f).is_none_or(|v| v.is_empty()))
                    .map(String::as_str)
                    .collect();
                if !missing.is_empty() {
                    p.push(format!(
                        "start.correlation: {event} is correlated by {}; give each a value, such as \"correlation\": {{{}}}.",
                        names(missing.iter().copied()),
                        missing
                            .iter()
                            .map(|f| format!("\"{f}\": \"op-1\""))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                for extra in correlation.keys().filter(|k| !fields.contains(*k)) {
                    p.push(format!(
                        "start.correlation.{extra}: {event} has no correlation field {extra}; its correlation fields are: {}.",
                        names(fields.iter().map(String::as_str))
                    ));
                }
            }
            let started: BTreeSet<&str> = playbook
                .triggers
                .iter()
                .filter(|t| &t.event.name == event)
                .map(|t| t.entry.as_str())
                .collect();
            if started.is_empty() {
                p.push(format!(
                    "start.event: the playbook has no trigger on {event:?}; it starts on: {}. Start the case from an event a trigger names, or from an entry: {{\"kind\": \"entry\", \"entry\": \"main\", \"input\": {{…}}}}.",
                    names(playbook.triggers.iter().map(|t| t.event.name.as_str()))
                ));
            }
            started
        }
    };
    for (command, returns) in &case.returns {
        let at = format!("returns.{command:?}");
        let Some(contract) = registry
            .commands
            .get(command)
            .filter(|_| called.contains(command.as_str()))
        else {
            p.push(format!(
                "{at}: the playbook never calls {command}; it calls: {}. Give returns only for those.",
                names(called.iter().copied())
            ));
            continue;
        };
        if returns.len() > MAX_RETURNS_PER_COMMAND {
            p.push(format!(
                "{at}: gives {} returns; a case gives at most {MAX_RETURNS_PER_COMMAND} for one command.",
                returns.len()
            ));
        }
        let answers = || {
            names(
                contract
                    .outcomes
                    .keys()
                    .map(String::as_str)
                    .filter(|o| *o != "accepted"),
            )
        };
        for (i, given) in returns.iter().enumerate() {
            let at = format!("{at}[{i}]");
            match given {
                CaseReturn::Pending { pending } => {
                    if !pending {
                        p.push(format!(
                            "{at}.pending: write {{\"pending\": true}} for a call that never gets an answer, or give its answer."
                        ));
                    }
                    if contract.asynchronous.is_none() {
                        p.push(format!(
                            "{at}: {command} answers at once, so it cannot be left pending; give its outcome and output (one of: {}).",
                            answers()
                        ));
                    }
                }
                CaseReturn::AtOnce {
                    outcome,
                    output,
                    at_once,
                } => {
                    if !at_once {
                        p.push(format!(
                            "{at}.at_once: write \"at_once\": true, or leave it out for an answer that arrives later."
                        ));
                    }
                    let direct = || {
                        names(
                            contract
                                .outcomes
                                .keys()
                                .map(String::as_str)
                                .filter(|o| *o != "accepted"),
                        )
                    };
                    match (&contract.asynchronous, contract.outcomes.get(outcome)) {
                        (None, _) => p.push(format!(
                            "{at}.at_once: {command} always answers at once; leave at_once out."
                        )),
                        (Some(_), None) if outcome != "accepted" => p.push(format!(
                            "{at}.outcome: {outcome:?} is not an outcome of {command}; it can answer at once with: {}.",
                            direct()
                        )),
                        (Some(_), _) if outcome == "accepted" => p.push(format!(
                            "{at}.outcome: an answer at once is one of {command}'s own outcomes other than accepted: {}. For an answer that arrives later, leave at_once out.",
                            direct()
                        )),
                        (Some(_), Some(schema)) => {
                            if let Err(e) = schema.validate(output) {
                                p.push(format!(
                                    "{at}.output: does not fit {command}'s {outcome:?} output: {e}. `aq-playbooks commands describe {command}` shows each outcome's output."
                                ));
                            }
                        }
                        (Some(_), None) => {}
                    }
                }
                CaseReturn::Answer { outcome, output } => match &contract.asynchronous {
                    None => match contract.outcomes.get(outcome) {
                        None => p.push(format!(
                            "{at}.outcome: {outcome:?} is not an outcome of {command}; its outcomes are: {}.",
                            answers()
                        )),
                        Some(schema) => {
                            if let Err(e) = schema.validate(output) {
                                p.push(format!(
                                    "{at}.output: does not fit {command}'s {outcome:?} output: {e}. `aq-playbooks commands describe {command}` shows each outcome's output."
                                ));
                            }
                        }
                    },
                    Some(asynchronous) => {
                        let payload = json!({"outcome": outcome, "output": output});
                        if let Err(e) = asynchronous.completion.validate(&payload) {
                            p.push(format!(
                                "{at}: is not an answer {command} can give: {e}. Give the answer itself, one of: {} (the tool returns `accepted` with a handle first, by itself). `aq-playbooks commands describe {command}` shows each outcome's output.",
                                answers()
                            ));
                        }
                    }
                },
            }
        }
    }
    for (i, call) in case.expect.calls.iter().enumerate() {
        if !called.contains(call.command.as_str()) {
            p.push(format!(
                "expect.calls[{i}].command: the playbook never calls {:?}; it calls: {}.",
                call.command,
                names(called.iter().copied())
            ));
        }
    }
    let outcomes: BTreeSet<&str> = playbook
        .entries
        .iter()
        .filter(|(name, _)| entries.contains(name.as_str()))
        .flat_map(|(_, entry)| entry.outcomes.keys().map(String::as_str))
        .collect();
    if !outcomes.is_empty() && !outcomes.contains(case.expect.outcome.as_str()) {
        p.push(format!(
            "expect.outcome: the run cannot end {:?}; it ends with one of: {}.",
            case.expect.outcome,
            names(outcomes)
        ));
    }
    problems
}

fn field<'v>(value: &'v Value, dotted: &str) -> Option<&'v Value> {
    dotted.split('.').try_fold(value, |v, key| v.get(key))
}
/// An envelope for `event`, built from its contract.
fn envelope(
    registry: &Registry,
    scope: &str,
    event: &str,
    id: String,
    correlation: BTreeMap<String, String>,
    payload: Value,
) -> Result<EventEnvelope, String> {
    let contract = registry.events.get(event).ok_or_else(|| {
        format!(
            "the playbook knows no event {event}; it knows: {}",
            names(registry.events.keys().map(String::as_str))
        )
    })?;
    let producer = contract
        .trusted_producers
        .iter()
        .next()
        .ok_or_else(|| format!("{event} names no producer"))?
        .clone();
    let subject = match &contract.subject_field {
        Some(at) => Some(
            field(&payload, at)
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    format!(
                        "payload.{at}: {event} carries its subject here, so it must be a string"
                    )
                })?
                .to_string(),
        ),
        None => None,
    };
    let revision = match &contract.revision_field {
        Some(at) => Some(field(&payload, at).and_then(Value::as_u64).ok_or_else(|| {
            format!("payload.{at}: {event} carries its revision here, so it must be a whole number")
        })?),
        None => None,
    };
    Ok(EventEnvelope {
        producer,
        event_id: id,
        contract: ContractRef {
            name: event.into(),
            fingerprint: contract.fingerprint().map_err(|e| e.to_string())?,
        },
        scope: scope.into(),
        correlation,
        subject,
        revision,
        causation: None,
        occurred_at_ms: None,
        observed_at_ms: None,
        payload,
    })
}

/// Answers each call from the case and records it.
struct CaseHost<'a> {
    registry: &'a Registry,
    scope: String,
    returns: BTreeMap<String, VecDeque<CaseReturn>>,
    calls: Vec<Call>,
    problems: Vec<String>,
    /// Completion events of answered asynchronous calls, admitted once the
    /// engine has stopped.
    completions: Vec<EventEnvelope>,
    handles: usize,
}
impl CaseHost<'_> {
    fn handle(&mut self) -> String {
        self.handles += 1;
        format!("case-{}", self.handles)
    }
}
impl Host for CaseHost<'_> {
    fn authorize(&self, a: &Authority, c: &aq_playbooks_model::CommandContract) -> bool {
        c.capabilities.is_subset(&a.capabilities) && c.scopes.contains(&a.scope)
    }
    fn dispatch(&mut self, intent: &Intent) -> DispatchResult {
        let command = intent.command.name.clone();
        let mut call = Call {
            step: intent.step.clone(),
            command: command.clone(),
            input: intent.input.clone(),
            outcome: None,
        };
        let given = self.returns.get_mut(&command).and_then(VecDeque::pop_front);
        let result = match given {
            None => {
                self.problems.push(format!(
                    "step {} called {command} but the case gives no return for it. Add its answer to returns.{command:?} (one per call, in call order), or {{\"pending\": true}} for an asynchronous command that never answers.",
                    intent.step
                ));
                DispatchResult::Rejected {
                    reason: "the case gives no return for this call".into(),
                }
            }
            Some(CaseReturn::Answer { outcome, output }) => match &intent.command.asynchronous {
                None => {
                    call.outcome = Some(outcome.clone());
                    DispatchResult::Completed { outcome, output }
                }
                Some(asynchronous) => {
                    let handle = self.handle();
                    let id = format!("case-{}", self.handles);
                    let completion = envelope(
                        self.registry,
                        &self.scope,
                        &asynchronous.completion_event,
                        id,
                        BTreeMap::from([(asynchronous.correlation_field.clone(), handle.clone())]),
                        json!({"outcome": outcome, "output": output}),
                    );
                    match completion {
                        Ok(event) => self.completions.push(event),
                        Err(e) => self.problems.push(format!(
                            "step {}: the answer for {command} cannot be delivered: {e}.",
                            intent.step
                        )),
                    }
                    call.outcome = Some(outcome);
                    DispatchResult::Accepted { handle }
                }
            },
            Some(CaseReturn::AtOnce {
                outcome, output, ..
            }) => {
                call.outcome = Some(outcome.clone());
                DispatchResult::Completed { outcome, output }
            }
            Some(CaseReturn::Pending { .. }) => DispatchResult::Accepted {
                handle: self.handle(),
            },
        };
        self.calls.push(call);
        result
    }
    fn reconcile(&mut self, _intent: &Intent) -> DispatchResult {
        DispatchResult::Unknown {
            reason: "a case answers each call once; it does not reconcile".into(),
        }
    }
}

fn inspected<S: aq_playbooks_runner::Storage>(
    engine: &Engine<S>,
    kind: InspectionKind,
) -> anyhow::Result<Vec<Value>> {
    let mut items = vec![];
    let mut after = None;
    loop {
        let page = engine.inspect(&InspectionQuery {
            kind,
            run: None,
            after,
            limit: 1000,
        })?;
        items.extend(page.items);
        match page.next {
            Some(next) => after = Some(next),
            None => return Ok(items),
        }
    }
}

/// Why a failed run stopped, when it stopped on one of the playbook's own
/// limits: the engine records an `engine_failure` for the run, and its time
/// and the run's visits tell which limit it was.
fn own_limit<S: aq_playbooks_runner::Storage>(
    engine: &Engine<S>,
    artifact: &Artifact,
    run: &Value,
) -> anyhow::Result<Option<String>> {
    let receipts = inspected(engine, InspectionKind::Receipts)?;
    let Some(failure) = receipts
        .iter()
        .find(|r| r["run"] == run["id"] && r["kind"] == "engine_failure")
    else {
        return Ok(None);
    };
    let limits = &artifact.content.playbook.limits;
    let step = failure["step"].as_str().unwrap_or_default();
    let elapsed =
        failure["at_ms"].as_i64().unwrap_or(0) - run["admitted_at_ms"].as_i64().unwrap_or(0);
    let visits = run["visits"].as_u64().unwrap_or(0);
    Ok(Some(if elapsed >= limits.max_duration_ms as i64 {
        format!(
            "the run stopped at step {step} on the playbook's own duration limit: it had run {elapsed} ms, and `limits.max_duration_ms` is {}. The limit covers the whole run from its start, so it must be longer than every wait and join deadline on the longest way through the run. Set it from what the document says (a wait of a day needs a limit above a day).",
            limits.max_duration_ms
        )
    } else if visits >= limits.max_step_visits {
        format!(
            "the run stopped at step {step} on the playbook's own limit of {} step visits (`limits.max_step_visits`). Look for a loop that runs more often than the document allows, or set the limit from the document.",
            limits.max_step_visits
        )
    } else {
        format!(
            "the run stopped at step {step} on one of the playbook's own `limits` (iterations, child runs, history or payload size). Compare the run with the limits set in the draft, and set them from the document."
        )
    }))
}

/// The outcome a live run has returned while it finishes.
fn returned<S: aq_playbooks_runner::Storage>(
    engine: &Engine<S>,
    run: &Value,
) -> anyhow::Result<Option<String>> {
    let id = run["id"].as_str().unwrap_or_default();
    Ok(engine
        .snapshot()?
        .runs
        .get(id)
        .and_then(|r| r.pending_return.as_ref())
        .map(|(outcome, _)| outcome.clone()))
}

/// Each place `got` differs from `want`, at most `limit` of them: the path,
/// what the case expects there and what the call had.
fn differences(want: &Value, got: &Value, path: &str, out: &mut Vec<String>, limit: usize) {
    if out.len() >= limit || want == got {
        return;
    }
    match (want, got) {
        (Value::Object(w), Value::Object(g)) => {
            let keys: BTreeSet<&String> = w.keys().chain(g.keys()).collect();
            for key in keys {
                let at = format!("{path}.{key}");
                match (w.get(key), g.get(key)) {
                    (Some(w), Some(g)) => differences(w, g, &at, out, limit),
                    (Some(w), None) => out.push(format!(
                        "{at}: the case expects {}; the call has nothing there",
                        clip(w)
                    )),
                    (None, Some(g)) => out.push(format!(
                        "{at}: the case does not list it; the call has {}",
                        clip(g)
                    )),
                    (None, None) => {}
                }
                if out.len() >= limit {
                    return;
                }
            }
        }
        (Value::Array(w), Value::Array(g)) if w.len() == g.len() => {
            for (i, (w, g)) in w.iter().zip(g).enumerate() {
                differences(w, g, &format!("{path}[{i}]"), out, limit);
            }
        }
        _ => out.push(format!(
            "{path}: the case expects {}, the call has {}",
            clip(want),
            clip(got)
        )),
    }
}
/// The calls made must be exactly the calls the case lists, in order, each
/// with exactly its input.
fn expected_calls(expect: &[ExpectedCall], calls: &[Call], problems: &mut Vec<String>) {
    let made = || {
        names(
            calls
                .iter()
                .map(|c| format!("{} at step {}", c.command, c.step))
                .collect::<Vec<_>>()
                .iter()
                .map(String::as_str),
        )
    };
    for (i, pair) in expect.iter().zip(calls).enumerate() {
        let (want, call) = pair;
        if want.command != call.command {
            problems.push(format!(
                "expect.calls[{i}]: the case expects a call to {} here, but the run called {} at step {}. List every call the run makes, in order; the calls made were: {}.",
                want.command,
                call.command,
                call.step,
                made()
            ));
            // The rest no longer line up; one problem says it.
            return;
        }
        let mut found = vec![];
        differences(&want.input, &call.input, "input", &mut found, 8);
        if !found.is_empty() {
            problems.push(format!(
                "expect.calls[{i}].input: the call to {} at step {} differs from the case: {}.",
                call.command,
                call.step,
                found.join("; ")
            ));
        }
    }
    if expect.len() > calls.len() {
        problems.push(format!(
            "expect.calls[{}]: the case expects a call to {} here, but the run made {} calls: {}.",
            calls.len(),
            expect[calls.len()].command,
            calls.len(),
            made()
        ));
    } else if calls.len() > expect.len() {
        let extra = &calls[expect.len()];
        problems.push(format!(
            "expect.calls: the run called {} at step {} after the {} calls the case lists (it lists {}). List every call the run makes, in order, each with its whole input; the calls made were: {}.",
            extra.command,
            extra.step,
            expect.len(),
            expect.len(),
            made()
        ));
    }
}

/// One case run within `limit` transitions: the report, the transitions it
/// took, and whether it was stopped at `limit`.
struct Ran {
    report: CaseReport,
    used: usize,
    stopped: bool,
}

/// Runs one case on `artifact`, within [`MAX_CASE_TRANSITIONS`]. An `Err` is
/// a fault of the artifact or the engine; everything about the case is in
/// the report's `problems`.
pub fn run_case(artifact: &Artifact, case: &Case) -> anyhow::Result<CaseReport> {
    artifact.verify()?;
    let mut ran = run_within(artifact, case, MAX_CASE_TRANSITIONS)?;
    if ran.stopped {
        stopped_at_case_cap(&mut ran.report);
    }
    Ok(ran.report)
}
fn stopped_at_case_cap(report: &mut CaseReport) {
    report.problems.push(format!(
        "the run took more than {MAX_CASE_TRANSITIONS} transitions; look for a loop that never ends."
    ));
    report.passed = false;
}

fn run_within(artifact: &Artifact, case: &Case, limit: usize) -> anyhow::Result<Ran> {
    let mut report = CaseReport {
        name: case.name.clone(),
        situation: case.situation.clone(),
        passed: false,
        outcome: None,
        calls: vec![],
        problems: check_case(artifact, case),
    };
    let done = |report: CaseReport| Ran {
        report,
        used: 0,
        stopped: false,
    };
    if !report.problems.is_empty() {
        return Ok(done(report));
    }
    let playbook = &artifact.content.playbook;
    let registry = &artifact.content.registry;
    let engine = Engine::new(MemoryStorage::default(), "case", "case-runner")?;
    engine.install(artifact.clone())?;
    let authority = Authority {
        actor: "case".into(),
        scope: playbook.scope.clone(),
        capabilities: playbook.required_capabilities.clone(),
    };
    engine.activate(&playbook.id, &artifact.digest, authority.clone())?;
    let mut host = CaseHost {
        registry,
        scope: playbook.scope.clone(),
        returns: case
            .returns
            .iter()
            .map(|(k, v)| (k.clone(), v.iter().cloned().collect()))
            .collect(),
        calls: vec![],
        problems: vec![],
        completions: vec![],
        handles: 0,
    };
    let started = match &case.start {
        CaseStart::Entry { entry, input } => engine
            .admit(&playbook.id, entry, input.clone(), authority, 0)
            .map(|_| ())
            .map_err(|e| format!("start.input: the entry {entry} refused it: {e}.")),
        CaseStart::Event {
            event,
            payload,
            correlation,
        } => (|| {
            let event = envelope(
                registry,
                &playbook.scope,
                event,
                "case-0".into(),
                correlation.clone(),
                payload.clone(),
            )
            .map_err(|e| format!("start.{e}"))?;
            let producer = event.producer.clone();
            let accepted = engine
                .admit_event(&producer, event, 0)
                .map_err(|e| format!("start.payload: the event was refused: {e}."))?;
            engine
                .fanout(&accepted.key, 100, 0)
                .map(|_| ())
                .map_err(|e| format!("start: the event started no run: {e}."))
        })(),
    };
    if let Err(problem) = started {
        report.problems.push(problem);
        return Ok(done(report));
    }
    let mut clock = 0;
    let mut used = 0;
    let mut stopped = false;
    for _ in 0..MAX_CASE_ROUNDS {
        used += engine.drive(&mut host, clock, limit - used)?;
        let completions = std::mem::take(&mut host.completions);
        if !completions.is_empty() {
            for event in completions {
                let producer = event.producer.clone();
                if let Err(e) = engine.admit_event(&producer, event, clock) {
                    host.problems
                        .push(format!("an answer's completion event was refused: {e}."));
                }
            }
            continue;
        }
        if used >= limit {
            stopped = true;
            break;
        }
        // Nothing more happens now: move the clock to the next deadline.
        match engine
            .snapshot()?
            .indexes
            .due
            .keys()
            .find(|at| **at > clock)
        {
            Some(at) => clock = *at,
            None => break,
        }
    }
    report.calls = std::mem::take(&mut host.calls);
    report.problems = std::mem::take(&mut host.problems);
    // Finished runs have left the state: read them through inspection.
    let runs = inspected(&engine, InspectionKind::Runs)?;
    let roots: Vec<&Value> = runs.iter().filter(|r| r["parent"].is_null()).collect();
    let at = |run: &Value| run["step"].as_str().unwrap_or_default().to_string();
    match roots.as_slice() {
        [] => report.problems.push("start: no run started.".to_string()),
        [run] => match run["status"].as_str() {
            Some("completed") => report.outcome = run["outcome"].as_str().map(String::from),
            Some("failed") => {
                let problem = match own_limit(&engine, artifact, run)? {
                    Some(problem) => problem,
                    None => format!(
                        "the run failed at step {} without an outcome; give each step's command a return that fits it, and an `on_error` edge where a call may fail.",
                        at(run)
                    ),
                };
                report.problems.push(problem);
            }
            Some("cancelled") => report
                .problems
                .push(format!("the run was cancelled at step {}.", at(run))),
            status => {
                // A run that has returned and waits only for an operation it
                // started to settle (an answer the case leaves pending) is
                // still live, so the outcome it returned is read from the state.
                let returned = match status {
                    Some("finishing") => returned(&engine, run)?,
                    _ => None,
                };
                match returned {
                    Some(outcome) => report.outcome = Some(outcome),
                    None => report.problems.push(format!(
                        "the run did not finish: it is {} at step {}. A case runs until nothing more can happen; give the returns this step needs.",
                        status.unwrap_or("unknown"),
                        at(run)
                    )),
                }
            }
        },
        many => report.problems.push(format!(
            "start: the event started {} runs; a case follows one run, so start it from an event one trigger names, or from an entry.",
            many.len()
        )),
    }
    if let Some(outcome) = &report.outcome
        && *outcome != case.expect.outcome
    {
        let step = roots.first().map(|r| at(r)).unwrap_or_default();
        report.problems.push(format!(
            "expect.outcome: the case expects the run to end {:?}, but it ended {outcome:?} (at step {step}).",
            case.expect.outcome
        ));
    }
    expected_calls(&case.expect.calls, &report.calls, &mut report.problems);
    // Every return the case gives is used by a call.
    for (command, left) in &host.returns {
        let given = case.returns.get(command).map_or(0, Vec::len);
        for i in given - left.len()..given {
            report.problems.push(format!(
                "returns.{command:?}[{i}]: never used: the run called {command} {} time(s). Give one return per call the run makes.",
                given - left.len()
            ));
        }
    }
    report.passed = report.problems.is_empty();
    Ok(Ran {
        report,
        used,
        stopped,
    })
}

/// Runs the cases a playbook is saved with: at least [`MIN_CASES`], at most
/// [`MAX_CASES`], each with its own name. Together they may take at most
/// `total_transitions` engine transitions ([`CASES_TRANSITION_BUDGET`] unless
/// the caller sets another), and each at most [`MAX_CASE_TRANSITIONS`].
/// Running out of the total is an `Err` naming the case it stopped at, never
/// a case failure and never a hang.
pub fn run_cases(
    artifact: &Artifact,
    cases: &[Case],
    total_transitions: usize,
) -> anyhow::Result<Vec<CaseReport>> {
    anyhow::ensure!(
        (MIN_CASES..=MAX_CASES).contains(&cases.len()),
        "a playbook is saved with at least {MIN_CASES} cases and at most {MAX_CASES}; {} were given. Write a case for each situation the document distinguishes.",
        cases.len()
    );
    let mut seen = BTreeSet::new();
    for case in cases {
        anyhow::ensure!(
            seen.insert(case.name.as_str()),
            "two cases are named {:?}; give each case its own name.",
            case.name
        );
    }
    artifact.verify()?;
    let mut left = total_transitions;
    let mut reports = vec![];
    for (k, case) in cases.iter().enumerate() {
        let limit = left.min(MAX_CASE_TRANSITIONS);
        let exceeded = || {
            anyhow::anyhow!(
                "the cases exceeded the total budget of {total_transitions} transitions at case {} ({:?}); together the cases may take at most that many. Make the cases or the playbook smaller.",
                k + 1,
                case.name
            )
        };
        if limit == 0 {
            return Err(exceeded());
        }
        let mut ran = run_within(artifact, case, limit)?;
        if ran.stopped {
            if limit < MAX_CASE_TRANSITIONS {
                return Err(exceeded());
            }
            stopped_at_case_cap(&mut ran.report);
        }
        left -= ran.used;
        reports.push(ran.report);
    }
    Ok(reports)
}

/// The digest of the facts a playbook was authored with (`facts.json`, as
/// the map of fact name to value), recorded on a save.
pub fn facts_digest(facts: &BTreeMap<String, Value>) -> anyhow::Result<String> {
    Ok(digest(facts)?)
}

/// A sentence of `source.md`: its first line, its last line when it spans
/// more than one (absent for a single line), and those lines as they stand,
/// joined with newlines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sentence {
    pub line: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
    pub text: String,
}
/// A sentence spans at most this many lines.
pub const MAX_SENTENCE_LINES: u32 = 8;
/// Quotes lines `line` to `end_line` (or `line` alone) of `markdown`: every
/// line must exist and hold text, at most [`MAX_SENTENCE_LINES`] of them.
/// The tools quote with it, and a reader re-quotes an outcome's sentences
/// from its own copy of the document with it.
pub fn quote(markdown: &str, line: u32, end_line: Option<u32>) -> Result<Sentence, String> {
    let lines: Vec<&str> = markdown.lines().collect();
    let end = end_line.unwrap_or(line);
    let range = if end == line {
        format!("line {line}")
    } else {
        format!("lines {line}-{end}")
    };
    if line == 0 || end < line {
        return Err(format!(
            "{range}: name a sentence by its line N, or by its lines A-B with A no greater than B, counted from 1."
        ));
    }
    if end - line + 1 > MAX_SENTENCE_LINES {
        return Err(format!(
            "{range}: a sentence spans at most {MAX_SENTENCE_LINES} lines; name the lines of one sentence."
        ));
    }
    if end as usize > lines.len() {
        return Err(format!(
            "{range}: source.md has lines 1 to {}; name the lines that hold the sentence.",
            lines.len()
        ));
    }
    let quoted = &lines[line as usize - 1..end as usize];
    if let Some(blank) = quoted.iter().position(|l| l.trim().is_empty()) {
        return Err(format!(
            "{range}: line {} of source.md is blank; a sentence does not cross a blank line. Name the lines that hold it.",
            line as usize + blank
        ));
    }
    let text = quoted.join("\n");
    if text.len() > MAX_SENTENCE_BYTES {
        return Err(format!(
            "{range}: the sentence is longer than {MAX_SENTENCE_BYTES} bytes."
        ));
    }
    Ok(Sentence {
        line,
        end_line: (end != line).then_some(end),
        text,
    })
}
/// The lines a sentence names must be a range [`quote`] accepts.
fn lines_named(line: u32, end_line: Option<u32>) -> Result<u32, String> {
    match end_line {
        _ if line == 0 => Err("a sentence's line is zero".into()),
        None => Ok(line),
        Some(end) if end <= line => Err(format!(
            "end_line {end} is not after line {line}; leave it out for a one-line sentence"
        )),
        Some(end) if end - line + 1 > MAX_SENTENCE_LINES => Err(format!(
            "lines {line}-{end} span more than {MAX_SENTENCE_LINES} lines"
        )),
        Some(end) => Ok(end),
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeKind {
    Saved,
    Question,
    Unable,
}
/// `out/outcome.json`: how an authoring run ended. Data for the host to
/// check again, never an instruction. **Every field is the agent's claim**
/// (the agent can write this file, and the files the tools read, itself).
/// A reader re-derives each field and never stores the agent's value as
/// fact: it re-seals `draft` from its own record, catalog, facts and
/// document and compares the digest with `artifact`; re-runs `cases` on the
/// artifact it sealed; recomputes `facts_digest` from the facts it holds;
/// and re-quotes each sentence's `text` (or `sentence`) from its own copy of
/// the document with [`quote`], keeping only the line numbers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Outcome {
    /// The draft and its cases; the digest of the artifact the agent's tools
    /// sealed and of the facts it was authored with.
    Saved {
        draft: Value,
        cases: Vec<Case>,
        artifact: String,
        facts_digest: String,
    },
    /// A question for the owner, about these sentences of the document.
    Question {
        question: String,
        sentences: Vec<Sentence>,
    },
    /// Why the document could not be turned into a playbook: either the
    /// sentence it is about (`line`, `end_line` as in [`Sentence`], and
    /// `sentence`), or, for a fault in one of the host's own files, that
    /// file ([`HOST_FILES`]) and no sentence.
    Unable {
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        line: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        end_line: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sentence: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file: Option<String>,
    },
}
impl Outcome {
    pub fn kind(&self) -> OutcomeKind {
        match self {
            Self::Saved { .. } => OutcomeKind::Saved,
            Self::Question { .. } => OutcomeKind::Question,
            Self::Unable { .. } => OutcomeKind::Unable,
        }
    }
    /// Every bound the outcome must meet, except its total size.
    pub fn check(&self) -> Result<(), String> {
        let text = |field: &str, value: &str, max: usize| {
            if value.trim().is_empty() {
                Err(format!("{field} is empty"))
            } else if value.len() > max {
                Err(format!("{field} is {} bytes; at most {max}", value.len()))
            } else {
                Ok(())
            }
        };
        let digest = |field: &str, value: &str| {
            if value.starts_with("sha256:") && value.len() == 71 {
                Ok(())
            } else {
                Err(format!("{field} is not a sha256: digest"))
            }
        };
        match self {
            Self::Saved {
                cases,
                artifact,
                facts_digest,
                ..
            } => {
                if !(MIN_CASES..=MAX_CASES).contains(&cases.len()) {
                    return Err(format!(
                        "a save has {} cases; at least {MIN_CASES} and at most {MAX_CASES}",
                        cases.len()
                    ));
                }
                digest("artifact", artifact)?;
                digest("facts_digest", facts_digest)
            }
            Self::Question {
                question,
                sentences,
            } => {
                text("question", question, MAX_QUESTION_BYTES)?;
                if sentences.is_empty() || sentences.len() > MAX_SENTENCES {
                    return Err(format!(
                        "a question names 1 to {MAX_SENTENCES} sentences; it names {}",
                        sentences.len()
                    ));
                }
                // In document order, and apart.
                let mut after = 0;
                for s in sentences {
                    let end = lines_named(s.line, s.end_line)?;
                    if s.line <= after {
                        return Err(format!(
                            "the sentence at line {} is not after the one before it (sentences are in document order and do not overlap)",
                            s.line
                        ));
                    }
                    after = end;
                    text("sentence text", &s.text, MAX_SENTENCE_BYTES)?;
                }
                Ok(())
            }
            Self::Unable {
                reason,
                line,
                end_line,
                sentence,
                file,
            } => {
                text("reason", reason, MAX_REASON_BYTES)?;
                match (line, sentence, file) {
                    (Some(line), Some(sentence), None) => {
                        lines_named(*line, *end_line)?;
                        text("sentence", sentence, MAX_SENTENCE_BYTES)
                    }
                    (None, None, Some(file)) if end_line.is_none() => {
                        if HOST_FILES.contains(&file.as_str()) {
                            Ok(())
                        } else {
                            Err(format!(
                                "file {file:?} is not one of the host's files ({})",
                                HOST_FILES.join(", ")
                            ))
                        }
                    }
                    _ => Err(
                        "an unable names either a sentence (line and sentence) or a host file, not both and not neither".into(),
                    ),
                }
            }
        }
    }
    /// The file's bytes: checked, and at most [`MAX_OUTCOME_BYTES`].
    pub fn to_bytes(&self) -> anyhow::Result<Vec<u8>> {
        self.check().map_err(anyhow::Error::msg)?;
        let mut bytes = serde_json::to_vec_pretty(self)?;
        bytes.push(b'\n');
        anyhow::ensure!(
            bytes.len() <= MAX_OUTCOME_BYTES,
            "the outcome is {} bytes; {OUTCOME_FILE} may be at most {MAX_OUTCOME_BYTES}. Shorten the draft or the cases.",
            bytes.len()
        );
        Ok(bytes)
    }
    /// Reads an outcome file: at most [`MAX_OUTCOME_BYTES`], strict, checked.
    pub fn from_bytes(bytes: &[u8]) -> anyhow::Result<Self> {
        anyhow::ensure!(
            bytes.len() <= MAX_OUTCOME_BYTES,
            "the outcome is {} bytes; at most {MAX_OUTCOME_BYTES}",
            bytes.len()
        );
        let outcome: Self = serde_json::from_slice(bytes)?;
        outcome.check().map_err(anyhow::Error::msg)?;
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aq_playbooks_compiler::draft::{SourceRecord, Workspace, seal};
    use aq_playbooks_model::{EvalContext, EvalLimits, EvalValue, Expr, Registry, eval};
    use serde_json::json;

    const REGISTRY_2_2: &str = include_str!("../../../contracts/aq-host-2.2.0/registry.json");
    const ROUTING_DRAFT: &str =
        include_str!("../../aq-playbooks-cli/tests/data/routing_draft.json");

    fn strings(xs: &[&str]) -> std::collections::BTreeSet<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }
    fn facts() -> BTreeMap<String, Value> {
        let everyone = json!({"harnesses": ["claude", "codex", "gemini", "opencode"],
                              "providers": ["anthropic", "google", "ollama", "openai"]});
        BTreeMap::from([
            (
                "levels".into(),
                json!(["simple-high", "standard-high", "complex-high", "deep-high"]),
            ),
            (
                "offers".into(),
                json!({"simple-high": everyone, "standard-high": everyone,
                       "complex-high": everyone, "deep-high": everyone}),
            ),
            (
                "harnesses".into(),
                json!(["claude", "codex", "gemini", "opencode"]),
            ),
            (
                "providers".into(),
                json!(["anthropic", "google", "ollama", "openai"]),
            ),
        ])
    }
    fn sealed(commands: &[&str], events: &[&str], capabilities: &[&str], draft: Value) -> Artifact {
        let ws = Workspace {
            markdown: "# Routing\n\nThe owner's routing document.\n".into(),
            record: SourceRecord {
                id: "routing".into(),
                binding: "aq.routing".into(),
                scope: "project".into(),
                purpose: "Route each task".into(),
                commands: strings(commands),
                events: strings(events),
                capabilities: strings(capabilities),
                facts_version: "settings-1".into(),
            },
            catalog: serde_json::from_str::<Registry>(REGISTRY_2_2).unwrap(),
            facts: facts(),
            draft,
        };
        seal(&ws).unwrap_or_else(|d| panic!("{d:#?}"))
    }
    /// The owner's routing document as a draft (the A4 feasibility draft),
    /// sealed against catalog 2.2.0 with the owner's Settings as facts.
    fn routing() -> Artifact {
        sealed(
            &["work.read", "work.route", "judgment.assess"],
            &["aq.work.route_needed"],
            &[
                "work.read",
                "work.route",
                "judgment.assess",
                "operation.get",
            ],
            serde_json::from_str(ROUTING_DRAFT).unwrap(),
        )
    }
    fn subject() -> Value {
        json!({"id": "task-1", "revision": 3})
    }
    fn output(r: &CaseReturn) -> Value {
        match r {
            CaseReturn::Answer { output, .. } | CaseReturn::AtOnce { output, .. } => output.clone(),
            CaseReturn::Pending { .. } => panic!("no output"),
        }
    }
    fn observed(filed_kind: &str) -> CaseReturn {
        CaseReturn::Answer {
            outcome: "observed".into(),
            output: json!({"reference": "task-1@3", "subject": subject(), "facts": {
                "kind": filed_kind, "title": "A task", "description": "", "origin": "owner",
                "level_hint": "", "charged_failures": 0, "ended_attempts": 0, "route": "none"}}),
        }
    }
    fn assessed(kind: &str, risk: &str, flags: &[&str]) -> CaseReturn {
        CaseReturn::Answer {
            outcome: "assessed".into(),
            output: json!({"reference": "judgment:op-1", "subject": subject(),
                "labels": [kind, risk], "flags": flags, "rationale": "Because.",
                "provenance": {"context_digest": "sha256:a", "credential_profile": "profile/a",
                    "model": "model-a", "output_digest": null, "prompt_digest": "sha256:c",
                    "provider": "provider-a",
                    "usage": {"cost_microunits": 1, "input_tokens": 1, "output_tokens": 1}}}),
        }
    }
    fn routed() -> CaseReturn {
        CaseReturn::Answer {
            outcome: "routed".into(),
            output: json!({"reference": "route-1", "subject": subject(),
                "harnesses": [], "providers": [], "prefer": []}),
        }
    }
    /// A step's input, evaluated as the draft writes it.
    fn step_input(step: &str, roots: &[(&str, Value)]) -> Value {
        let draft: Value = serde_json::from_str(ROUTING_DRAFT).unwrap();
        let expr: Expr =
            serde_json::from_value(draft["steps"][step]["op"]["input"].clone()).unwrap();
        let mut context = EvalContext::from([(
            "definitions".to_string(),
            EvalValue::Known(json!(
                facts().into_iter().collect::<serde_json::Map<_, _>>()
            )),
        )]);
        for (root, value) in roots {
            context.insert(root.to_string(), EvalValue::Known(value.clone()));
        }
        match eval(&expr, &context, &EvalLimits::default()).unwrap() {
            EvalValue::Known(value) => value,
            other => panic!("{step}: {other:?}"),
        }
    }
    fn call(command: &str, input: Value) -> ExpectedCall {
        ExpectedCall {
            command: command.into(),
            input,
        }
    }
    /// Every call a run of the routing draft makes, each with its whole
    /// input: read, ask, then `route` on an assessed answer or `assume`.
    fn calls_for(read: &CaseReturn, answer: Option<&CaseReturn>) -> Vec<ExpectedCall> {
        let task = output(read);
        let route = match answer {
            Some(answer) => step_input(
                "route",
                &[
                    ("task", task.clone()),
                    (
                        "answer",
                        json!({"c": {"participant": "op", "outcome": "assessed", "output": output(answer)}}),
                    ),
                ],
            ),
            None => step_input("assume", &[("task", task.clone())]),
        };
        vec![
            call("work.read", json!({"subject": subject()})),
            call("judgment.assess", step_input("ask", &[("task", task)])),
            call("work.route", route),
        ]
    }
    fn case(
        returns: Vec<(&str, Vec<CaseReturn>)>,
        outcome: &str,
        calls: Vec<ExpectedCall>,
    ) -> Case {
        Case {
            name: "a task".into(),
            situation: "A task needs a route".into(),
            start: CaseStart::Entry {
                entry: "main".into(),
                input: json!({"task": subject()}),
            },
            returns: returns
                .into_iter()
                .map(|(c, r)| (c.to_string(), r))
                .collect(),
            expect: Expectation {
                outcome: outcome.into(),
                calls,
            },
        }
    }
    fn classified(filed: &str, kind: &str, risk: &str, flags: &[&str]) -> Case {
        let (read, answer) = (observed(filed), assessed(kind, risk, flags));
        let calls = calls_for(&read, Some(&answer));
        case(
            vec![
                ("work.read", vec![read]),
                ("judgment.assess", vec![answer]),
                ("work.route", vec![routed()]),
            ],
            "routed",
            calls,
        )
    }
    fn route_call(report: &CaseReport) -> &Call {
        let routes: Vec<&Call> = report
            .calls
            .iter()
            .filter(|c| c.command == "work.route")
            .collect();
        assert_eq!(routes.len(), 1, "{report:#?}");
        routes[0]
    }
    fn problem<'a>(report: &'a CaseReport, starts: &str) -> &'a str {
        report
            .problems
            .iter()
            .find(|p| p.starts_with(starts))
            .unwrap_or_else(|| panic!("no problem starting {starts:?}: {report:#?}"))
    }

    #[test]
    fn a_case_records_each_call_with_its_input() {
        let artifact = routing();
        let report = run_case(&artifact, &classified("feature", "feature", "low", &[])).unwrap();
        assert!(report.passed, "{report:#?}");
        assert_eq!(report.outcome.as_deref(), Some("routed"));
        let seen: Vec<(&str, &str, Option<&str>)> = report
            .calls
            .iter()
            .map(|c| (c.step.as_str(), c.command.as_str(), c.outcome.as_deref()))
            .collect();
        assert_eq!(
            seen,
            [
                ("read", "work.read", Some("observed")),
                ("ask", "judgment.assess", Some("assessed")),
                ("route", "work.route", Some("routed")),
            ]
        );
        assert_eq!(report.calls[0].input, json!({"subject": subject()}));
        assert_eq!(
            report.calls[1].input["context"]["items"][0]["text"],
            "A task"
        );
        let route = &report.calls[2].input;
        assert_eq!(route["subject"], subject());
        assert_eq!(route["level"], "standard-high");
        assert_eq!(route["classification"]["labels"], json!(["feature", "low"]));
    }

    /// The A2 runtime proof: the route's level comes from the judgment's
    /// first label, read through the named join, not from the kind the
    /// filer gave (always "feature" here).
    #[test]
    fn an_async_answer_reaches_the_playbook_through_the_join() {
        let artifact = routing();
        for (kind, flags, level) in [
            ("chore", &[][..], "simple-high"),
            ("feature", &[][..], "standard-high"),
            ("research", &["clearly_harder"][..], "complex-high"),
            ("spec", &["needs_deep_reasoning"][..], "deep-high"),
        ] {
            let report = run_case(&artifact, &classified("feature", kind, "low", flags)).unwrap();
            assert!(report.passed, "{kind}: {report:#?}");
            let route = route_call(&report);
            assert_eq!(route.step, "route", "{kind}");
            assert_eq!(route.input["level"], level, "{kind}");
            assert_eq!(route.input["classification"]["labels"][0], kind);
        }
    }

    #[test]
    fn a_pending_answer_takes_the_timeout_path() {
        let artifact = routing();
        let read = observed("bugfix");
        let pending = case(
            vec![
                ("work.read", vec![read.clone()]),
                (
                    "judgment.assess",
                    vec![CaseReturn::Pending { pending: true }],
                ),
                ("work.route", vec![routed()]),
            ],
            "routed",
            calls_for(&read, None),
        );
        let report = run_case(&artifact, &pending).unwrap();
        assert!(report.passed, "{report:#?}");
        let ask = report
            .calls
            .iter()
            .find(|c| c.command == "judgment.assess")
            .unwrap();
        assert_eq!(ask.outcome, None, "no answer came");
        let route = route_call(&report);
        assert_eq!(route.step, "assume");
        assert_eq!(route.input["level"], "standard-high");
        assert_eq!(route.input["classification"], Value::Null);
        let codes: Vec<&Value> = route.input["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| &r["code"])
            .collect();
        assert!(codes.contains(&&json!("risk_assumed")), "{codes:?}");
    }

    #[test]
    fn a_call_without_a_return_fails_the_case_and_names_the_step() {
        let artifact = routing();
        let mut missing = classified("feature", "feature", "low", &[]);
        missing.returns.remove("work.route");
        let report = run_case(&artifact, &missing).unwrap();
        assert!(!report.passed, "{report:#?}");
        problem(
            &report,
            "step route called work.route but the case gives no return for it",
        );
        let route = route_call(&report);
        assert_eq!(route.outcome, None);
        assert_eq!(
            route.input["level"], "standard-high",
            "the input is still recorded"
        );
    }

    /// A case lists every call the run makes, in order: a call it does not
    /// list fails it, and so does one it lists that the run does not make.
    #[test]
    fn every_call_the_run_makes_must_be_listed_in_order() {
        let artifact = routing();
        let full = classified("feature", "design", "low", &[]);
        assert!(run_case(&artifact, &full).unwrap().passed);
        // The review's probe: naming only the route, even with its whole
        // input, no longer passes.
        let mut only_route = full.clone();
        only_route.expect.calls = vec![full.expect.calls[2].clone()];
        let report = run_case(&artifact, &only_route).unwrap();
        assert!(!report.passed);
        let p = problem(&report, "expect.calls[0]");
        assert!(p.contains("work.route") && p.contains("work.read"), "{p}");
        // No calls listed at all.
        let mut none = full.clone();
        none.expect.calls = vec![];
        let report = run_case(&artifact, &none).unwrap();
        assert!(!report.passed);
        let p = problem(
            &report,
            "expect.calls: the run called work.read at step read",
        );
        assert!(p.contains("lists 0"), "{p}");
        // Out of order.
        let mut swapped = full.clone();
        swapped.expect.calls.swap(0, 1);
        let report = run_case(&artifact, &swapped).unwrap();
        assert!(!report.passed);
        problem(&report, "expect.calls[0]");
        // One call more than the run makes.
        let mut more = full.clone();
        more.expect.calls.push(full.expect.calls[2].clone());
        let report = run_case(&artifact, &more).unwrap();
        assert!(!report.passed);
        let p = problem(&report, "expect.calls[3]");
        assert!(p.contains("made 3 calls"), "{p}");
    }

    #[test]
    fn an_expected_input_must_equal_the_call_exactly() {
        let artifact = routing();
        let full = classified("feature", "design", "low", &[]);
        // The review's probe: the level is wrong, and the case fails with
        // the place, what it expects and what the call had.
        let mut wrong = full.clone();
        wrong.expect.calls[2].input["level"] = json!("deep-high");
        let report = run_case(&artifact, &wrong).unwrap();
        assert!(!report.passed);
        let p = problem(&report, "expect.calls[2].input");
        assert!(
            p.contains(
                "input.level: the case expects \"deep-high\", the call has \"complex-high\""
            ),
            "{p}"
        );
        // A part of the input is not enough.
        let mut part = full.clone();
        part.expect.calls[2].input = json!({"level": "complex-high"});
        let report = run_case(&artifact, &part).unwrap();
        assert!(!report.passed);
        let p = problem(&report, "expect.calls[2].input");
        assert!(
            p.contains(
                "input.harnesses: the case does not list it; the call has [\"claude\",\"codex\"]"
            ),
            "{p}"
        );
        // An array must be equal, not a part of it.
        let mut fewer = full.clone();
        fewer.expect.calls[2].input["harnesses"] = json!(["claude"]);
        let report = run_case(&artifact, &fewer).unwrap();
        assert!(!report.passed);
        let p = problem(&report, "expect.calls[2].input");
        assert!(p.contains("input.harnesses"), "{p}");
    }

    #[test]
    fn a_return_the_run_never_uses_fails_the_case() {
        let artifact = routing();
        let mut extra = classified("feature", "design", "low", &[]);
        extra.returns.get_mut("work.route").unwrap().push(routed());
        let report = run_case(&artifact, &extra).unwrap();
        assert!(!report.passed, "{report:#?}");
        let p = problem(&report, "returns.\"work.route\"[1]");
        assert!(p.contains("never used"), "{p}");
    }

    #[test]
    fn the_calls_and_each_input_are_required() {
        let case = serde_json::to_value(classified("feature", "design", "low", &[])).unwrap();
        let mut no_calls = case.clone();
        no_calls["expect"].as_object_mut().unwrap().remove("calls");
        let error = serde_json::from_value::<Case>(no_calls)
            .unwrap_err()
            .to_string();
        assert!(error.contains("calls"), "{error}");
        let mut no_input = case;
        no_input["expect"]["calls"][0]
            .as_object_mut()
            .unwrap()
            .remove("input");
        let error = serde_json::from_value::<Case>(no_input)
            .unwrap_err()
            .to_string();
        assert!(error.contains("input"), "{error}");
    }

    #[test]
    fn a_triggered_case_starts_from_the_event() {
        let artifact = routing();
        let mut c = classified("feature", "chore", "low", &[]);
        c.start = CaseStart::Event {
            event: "aq.work.route_needed".into(),
            payload: json!({"subject": {"id": "task-1", "revision": 2}}),
            correlation: BTreeMap::new(),
        };
        c.expect.calls[0].input = json!({"subject": {"id": "task-1", "revision": 2}});
        let report = run_case(&artifact, &c).unwrap();
        assert!(report.passed, "{report:#?}");
        // An event the playbook has no trigger for starts nothing.
        c.start = CaseStart::Event {
            event: "aq.judgment.assessed".into(),
            payload: json!({}),
            correlation: BTreeMap::new(),
        };
        let report = run_case(&artifact, &c).unwrap();
        assert!(!report.passed);
        problem(&report, "start.event");
    }

    /// Any event a trigger names can start a case, with the correlation
    /// values the case gives; `aq.work.changed` is correlated by `operation`.
    #[test]
    fn a_case_starts_from_any_trigger_event_with_its_correlation() {
        let routing: Value = serde_json::from_str(ROUTING_DRAFT).unwrap();
        let ret = |outcome: &str| {
            json!({"op": {"kind": "return", "outcome": outcome,
            "output": {"op": "literal", "value": null}}, "edges": {}, "on_error": null})
        };
        let draft = json!({
            "entries": {"main": {"input": routing["entries"]["main"]["input"],
                "outcomes": {"seen": {"type": "null"}, "failed": {"type": "null"}}, "start": "read"}},
            "triggers": [{"id": "changed", "event": "aq.work.changed", "entry": "main",
                "input": {"op": "object", "fields": {"task": {"op": "ref", "root": "event", "path": ["subject"]}}}}],
            "steps": {
                "read": {"op": {"kind": "invoke", "command": "work.read", "bind": "task", "retry": null,
                    "input": {"op": "object", "fields": {"subject": {"op": "ref", "root": "input", "path": ["task"]}}}},
                    "edges": {"observed": "seen", "refused": "failed"}, "on_error": "failed"},
                "seen": ret("seen"),
                "failed": ret("failed"),
            },
        });
        let artifact = sealed(&["work.read"], &["aq.work.changed"], &["work.read"], draft);
        let changed = |correlation: &[(&str, &str)]| Case {
            name: "changed".into(),
            situation: "A task changed".into(),
            start: CaseStart::Event {
                event: "aq.work.changed".into(),
                payload: json!({"outcome": "changed", "reference": "task-1@3", "subject": subject()}),
                correlation: correlation
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            },
            returns: BTreeMap::from([("work.read".into(), vec![observed("feature")])]),
            expect: Expectation {
                outcome: "seen".into(),
                calls: vec![call("work.read", json!({"subject": subject()}))],
            },
        };
        let report = run_case(&artifact, &changed(&[("operation", "op-7")])).unwrap();
        assert!(report.passed, "{report:#?}");
        let report = run_case(&artifact, &changed(&[])).unwrap();
        assert!(!report.passed);
        assert!(problem(&report, "start.correlation").contains("operation"));
        let report = run_case(
            &artifact,
            &changed(&[("operation", "op-7"), ("other", "x")]),
        )
        .unwrap();
        assert!(!report.passed);
        assert!(problem(&report, "start.correlation").contains("other"));
    }

    #[test]
    fn fewer_than_five_or_more_than_forty_cases_are_refused() {
        let artifact = routing();
        let one = classified("feature", "feature", "low", &[]);
        let named = |n: usize| {
            (0..n)
                .map(|i| Case {
                    name: format!("case {i}"),
                    ..one.clone()
                })
                .collect::<Vec<_>>()
        };
        for n in [0, 4, 41] {
            let error = run_cases(&artifact, &named(n), CASES_TRANSITION_BUDGET)
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("at least 5") && error.contains("at most 40"),
                "{error}"
            );
        }
        for n in [MIN_CASES, MAX_CASES] {
            let reports = run_cases(&artifact, &named(n), CASES_TRANSITION_BUDGET).unwrap();
            assert_eq!(reports.len(), n);
            assert!(reports.iter().all(|r| r.passed));
        }
        // Each case has its own name.
        let mut twins = named(5);
        twins[1].name = twins[0].name.clone();
        let error = run_cases(&artifact, &twins, CASES_TRANSITION_BUDGET)
            .unwrap_err()
            .to_string();
        assert!(error.contains("case 0"), "{error}");
    }

    /// All the cases of a save share one transition budget, and running out
    /// of it is its own failure, named by the case it stopped at.
    #[test]
    fn the_cases_share_a_total_transition_budget() {
        let artifact = routing();
        let one = classified("feature", "feature", "low", &[]);
        let cases: Vec<Case> = (0..5)
            .map(|i| Case {
                name: format!("case {i}"),
                ..one.clone()
            })
            .collect();
        let error = run_cases(&artifact, &cases, 30).unwrap_err().to_string();
        assert!(
            error.starts_with("the cases exceeded the total budget of 30 transitions at case "),
            "{error}"
        );
        assert!(error.contains("\"case 2\""), "{error}");
        assert!(run_cases(&artifact, &cases, CASES_TRANSITION_BUDGET).is_ok());
    }

    #[test]
    fn a_sentence_is_quoted_from_its_lines() {
        let markdown = "# Title\n\n- Research is Standard, unless it is clearly harder,\n  in which case it is Complex.\n\nLast.\n";
        let one = quote(markdown, 6, None).unwrap();
        assert_eq!(
            serde_json::to_value(&one).unwrap(),
            json!({"line": 6, "text": "Last."})
        );
        let two = quote(markdown, 3, Some(4)).unwrap();
        assert_eq!(
            serde_json::to_value(&two).unwrap(),
            json!({"line": 3, "end_line": 4,
                   "text": "- Research is Standard, unless it is clearly harder,\n  in which case it is Complex."})
        );
        assert_eq!(quote(markdown, 6, Some(6)).unwrap(), one);
        let past = quote(markdown, 6, Some(7)).unwrap_err();
        assert!(past.contains("1 to 6"), "{past}");
        assert!(quote(markdown, 2, None).unwrap_err().contains("blank"));
        assert!(quote(markdown, 3, Some(5)).unwrap_err().contains("blank"));
        assert!(quote(markdown, 4, Some(3)).is_err());
        let long = "x\n".repeat(20);
        assert!(quote(&long, 1, Some(8)).is_ok());
        assert!(quote(&long, 1, Some(9)).unwrap_err().contains("8 lines"));
        // The outcome holds its sentences in document order, apart.
        let question = |sentences: Vec<Sentence>| Outcome::Question {
            question: "Which?".into(),
            sentences,
        };
        assert!(question(vec![two.clone(), one.clone()]).check().is_ok());
        assert!(question(vec![one.clone(), two.clone()]).check().is_err());
        assert!(
            question(vec![two.clone(), quote(markdown, 4, None).unwrap()])
                .check()
                .is_err()
        );
        let bad = json!({"kind": "question", "question": "Which?",
                         "sentences": [{"line": 6, "end_line": 6, "text": "Last."}]});
        assert!(Outcome::from_bytes(&serde_json::to_vec(&bad).unwrap()).is_err());
        let unable = json!({"kind": "unable", "reason": "No command does it.", "line": 3,
            "end_line": 4, "sentence": two.text});
        assert!(Outcome::from_bytes(&serde_json::to_vec(&unable).unwrap()).is_ok());
    }

    /// Run by hand in release: `cargo test --release -p aq-playbooks-simulation
    /// -- --ignored forty_routing_cases --nocapture`.
    #[test]
    #[ignore = "a timing, run by hand in release"]
    fn forty_routing_cases_take_this_long() {
        let artifact = routing();
        let one = classified("feature", "design", "low", &[]);
        let cases: Vec<Case> = (0..MAX_CASES)
            .map(|i| Case {
                name: format!("case {i}"),
                ..one.clone()
            })
            .collect();
        let started = std::time::Instant::now();
        let reports = run_cases(&artifact, &cases, CASES_TRANSITION_BUDGET).unwrap();
        println!("40 routing cases: {:?}", started.elapsed());
        assert!(reports.iter().all(|r| r.passed));
    }
}
