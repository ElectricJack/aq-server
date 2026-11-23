//! Host catalog 2.2.0 (Host9), pure parts: task facts for `work.read`, the
//! route a `work.route` input asks for, the answer schema and output of
//! `judgment.assess`, and the `aq.work.route_needed` event. No I/O.
use aq_routing::{
    Class, CompactRoute, ProviderIntent, Route, RouteReason, Source, classify::Options,
};
use serde_json::{Value, json};
use uuid::Uuid;

/// The playbook binding that routes tasks (spec 0013 §7).
pub const ROUTING_BINDING: &str = "aq.routing";
const TITLE_BYTES: usize = 512;
const DESCRIPTION_BYTES: usize = 8192;
const KIND_BYTES: usize = 64;

/// What a document can test about a task (`work.read` `facts`). Priority,
/// status, owner, project and load are deliberately absent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkFacts {
    pub kind: String,
    pub title: String,
    pub description: String,
    /// `owner`, `agent`, `supervisor`, `playbook` or `unknown`.
    pub origin: &'static str,
    pub level_hint: String,
    pub charged_failures: u32,
    pub ended_attempts: u32,
    /// `none`, `router` or `owner`.
    pub route: &'static str,
}

/// `text` cut to at most `max` bytes at a character boundary.
pub fn truncate(text: &str, max: usize) -> &str {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// The level whoever filed the task asked for, as a class string
/// (`standard-high`), or `""` when there is none or it is not one. A level
/// Settings does not offer is kept: the document says what to do with it.
pub fn normalize_hint(raw: Option<&str>) -> String {
    let text = raw.unwrap_or("").trim().to_ascii_lowercase();
    text.parse::<Class>()
        .map(|c| c.to_string())
        .unwrap_or_default()
}

/// Who set the route in `metadata`: an unreadable route counts as none.
pub fn route_source(metadata: &Value) -> Option<Source> {
    CompactRoute::from_metadata(metadata)
        .ok()
        .flatten()
        .map(|r| r.s)
}

impl WorkFacts {
    /// The facts of a graph task node. `in_namespace` says a playbook of this
    /// namespace created it; nothing else about who filed a task is recorded,
    /// so every other task's origin is `unknown`.
    pub fn of(
        node: &Value,
        in_namespace: bool,
        charged_failures: u32,
        ended_attempts: u32,
    ) -> Self {
        let text = |field: &str| node[field].as_str().unwrap_or("");
        let metadata = &node["metadata"];
        Self {
            kind: truncate(
                metadata["aq:task_type"].as_str().unwrap_or("").trim(),
                KIND_BYTES,
            )
            .into(),
            title: truncate(text("title"), TITLE_BYTES).into(),
            description: truncate(text("body"), DESCRIPTION_BYTES).into(),
            origin: if in_namespace { "playbook" } else { "unknown" },
            level_hint: normalize_hint(metadata["aq:intelligence_hint"].as_str()),
            charged_failures: charged_failures.min(1000),
            ended_attempts: ended_attempts.min(1000),
            route: match route_source(metadata) {
                None => "none",
                Some(Source::Router) => "router",
                Some(Source::Owner) => "owner",
            },
        }
    }
}

/// `<id>@<revision>`: one value per task version.
pub fn reference(id: &str, revision: i64) -> String {
    format!("{id}@{revision}")
}
fn subject(id: &str, revision: i64) -> Value {
    json!({"id": id, "revision": revision.max(0)})
}
/// `{reference, subject}`: the output of `stale` and `recorded`.
pub fn task_output(id: &str, revision: i64) -> Value {
    json!({"reference": reference(id, revision), "subject": subject(id, revision)})
}
/// The output of a `refused` outcome.
pub fn refusal(id: &str, revision: i64, reason: &str) -> Value {
    json!({"reason": reason, "reference": reference(id, revision), "subject": subject(id, revision)})
}
/// The `observed` output of `work.read`.
pub fn read_output(task: Uuid, version: i64, facts: &WorkFacts) -> Value {
    let id = task.to_string();
    json!({
        "subject": subject(&id, version),
        "reference": reference(&id, version),
        "facts": {
            "kind": facts.kind, "title": facts.title, "description": facts.description,
            "origin": facts.origin, "level_hint": facts.level_hint,
            "charged_failures": facts.charged_failures, "ended_attempts": facts.ended_attempts,
            "route": facts.route,
        },
    })
}

fn names(value: &Value) -> Vec<String> {
    let items = value.as_array().map(Vec::as_slice).unwrap_or(&[]);
    let mut out: Vec<String> = vec![];
    for name in items.iter().filter_map(Value::as_str) {
        if !out.iter().any(|n| n == name) {
            out.push(name.into());
        }
    }
    out
}

/// Where a `work.route` input's route comes from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Provenance<'a> {
    /// The active artifact of the routing binding.
    pub artifact_digest: Option<&'a str>,
    /// The source revision that artifact was authored from.
    pub source_revision: Option<u64>,
    pub ended_attempts: u32,
}

/// A dimension with an empty input accepts every offer. A nonempty input
/// must retain at least one of the level's names.
struct OfferedNames {
    requested: bool,
    names: Vec<String>,
}
impl OfferedNames {
    fn select(
        input: &Value,
        offers: &[aq_routing::classify::Offer],
        field: fn(&aq_routing::classify::Offer) -> &String,
    ) -> Self {
        let asked = names(input);
        let requested = !asked.is_empty();
        let names = asked
            .into_iter()
            .filter(|value| offers.iter().any(|o| field(o) == value))
            .collect();
        Self { requested, names }
    }
    fn missing(&self) -> bool {
        self.requested && self.names.is_empty()
    }
    fn includes(&self, value: &String) -> bool {
        self.names.is_empty() || self.names.contains(value)
    }
}

/// The route a `work.route` input asks for, within what Settings offers at
/// its level: the harness, provider and preference lists are intersected
/// with the level's offers. `None` is `not_offered`: the level has no
/// profile, a non-empty list has nothing left, or no offered pair is in both
/// lists. `hold: true` pins the provider.
pub fn route_from_input(input: &Value, options: &Options, from: Provenance<'_>) -> Option<Route> {
    let class: Class = input["level"].as_str()?.parse().ok()?;
    let offers = options.classes.get(&class).filter(|o| !o.is_empty())?;
    let harnesses = OfferedNames::select(&input["harnesses"], offers, |o| &o.harness);
    let providers = OfferedNames::select(&input["providers"], offers, |o| &o.provider);
    if harnesses.missing() || providers.missing() {
        return None;
    }
    let pair = offers
        .iter()
        .any(|offer| harnesses.includes(&offer.harness) && providers.includes(&offer.provider));
    if !pair {
        return None;
    }
    let prefer = names(&input["prefer"])
        .into_iter()
        .filter(|name| offers.iter().any(|o| &o.harness == name) && harnesses.includes(name))
        .collect();
    let reasons = (input["reasons"].as_array().into_iter().flatten())
        .map(|r| RouteReason {
            code: r["code"].as_str().unwrap_or("").into(),
            detail: r["detail"].as_str().unwrap_or("").into(),
        })
        .collect();
    Some(Route {
        class,
        harnesses: harnesses.names,
        providers: providers.names,
        prefer,
        lane: None,
        provider_intent: match input["hold"] == true {
            true => ProviderIntent::Pinned,
            false => ProviderIntent::Any,
        },
        reasons,
        classification: None,
        policy_digest: from.artifact_digest.map(str::to_owned),
        structure_revision: from.source_revision,
        source: Source::Router,
        ended_attempts: from.ended_attempts.into(),
        judgment: Some(input["classification"].clone()).filter(|c| !c.is_null()),
    })
}

/// The `routed` output: the lists as the host kept them.
pub fn routed_output(id: &str, revision: i64, route: &Route) -> Value {
    json!({
        "harnesses": route.harnesses, "providers": route.providers, "prefer": route.prefer,
        "reference": reference(id, revision), "subject": subject(id, revision),
    })
}

/// A digest of what Settings offers, so a `not_offered` task is asked about
/// again when the offers change.
pub fn options_digest(options: &Options) -> String {
    aq_playbook_host::model::digest(options).unwrap_or_default()
}

// ------------------------------------------------------------ judgment.assess

/// Largest `judgment.assess` request the host runs, in canonical bytes.
pub const ASSESS_REQUEST_BYTES: usize = 65_536;

fn ids(items: &Value) -> Vec<&str> {
    let items = items.as_array().map(Vec::as_slice).unwrap_or(&[]);
    items.iter().filter_map(|i| i["id"].as_str()).collect()
}
fn distinct(names: &[&str]) -> bool {
    names
        .iter()
        .enumerate()
        .all(|(i, n)| !n.is_empty() && !names[..i].contains(n))
}

/// Why the host does not run a `judgment.assess` request, if it does not.
/// `invalid_request`: over [`ASSESS_REQUEST_BYTES`]; no question; no field
/// (a request must ask at least one labelled question); a field or flag id
/// that is empty or used twice; a field without choices, or with an empty or
/// repeated choice. `budget_exceeded`: a deadline above `max_deadline_ms` or
/// an output budget above `max_output_tokens` (the host's caps).
pub fn assess_refusal(
    input: &Value,
    max_deadline_ms: u64,
    max_output_tokens: u64,
) -> Option<&'static str> {
    let bytes = aq_playbook_host::model::canonical_bytes(input).map(|b| b.len());
    if !bytes.is_ok_and(|b| b <= ASSESS_REQUEST_BYTES) {
        return Some("invalid_request");
    }
    if !valid_questions(input) {
        return Some("invalid_request");
    }
    let budget = &input["budget"];
    let over = |name: &str, max: u64| budget[name].as_u64().is_none_or(|v| v > max);
    (over("deadline_ms", max_deadline_ms) || over("max_output_tokens", max_output_tokens))
        .then_some("budget_exceeded")
}

fn valid_questions(input: &Value) -> bool {
    let fields = input["fields"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let question = input["question"]
        .as_str()
        .is_some_and(|q| !q.trim().is_empty());
    question
        && !fields.is_empty()
        && distinct(&ids(&input["fields"]))
        && distinct(&ids(&input["flags"]))
        && fields.iter().all(valid_choices)
}
fn valid_choices(field: &Value) -> bool {
    let choices: Vec<&str> = field["choices"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    !choices.is_empty() && distinct(&choices)
}

/// JSON Schema (draft 2020-12) of the answer the model gives, by name:
/// `{labels: {<field id>: <choice>}, flags: {<flag id>: bool}, rationale}`.
pub fn assess_schema(input: &Value) -> Value {
    let object = |properties: serde_json::Map<String, Value>| {
        let required: Vec<&String> = properties.keys().collect();
        json!({"type": "object", "additionalProperties": false, "required": required, "properties": properties})
    };
    let fields = (input["fields"].as_array().into_iter().flatten())
        .map(|f| {
            (
                f["id"].as_str().unwrap_or("").to_owned(),
                json!({"type": "string", "enum": f["choices"]}),
            )
        })
        .collect();
    let flags = (ids(&input["flags"]).into_iter())
        .map(|id| (id.to_owned(), json!({"type": "boolean"})))
        .collect();
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "additionalProperties": false,
        "required": ["labels", "flags", "rationale"],
        "properties": {
            "labels": object(fields),
            "flags": object(flags),
            "rationale": {"type": "string", "maxLength": input["rationale_max_bytes"].as_u64().unwrap_or(0)},
        }
    })
}

/// What the model is told (instructions) and shown (input): the question,
/// each labelled question with its choices, each yes/no question, and the
/// context items.
pub fn assess_prompt(input: &Value) -> (String, String) {
    let mut text = format!(
        "{}\n\nAnswer with one JSON object.\n",
        input["question"].as_str().unwrap_or("")
    );
    text.push_str("\n\"labels\": for each of these, exactly one of its choices:\n");
    for field in input["fields"].as_array().into_iter().flatten() {
        let choices: Vec<&str> = (field["choices"].as_array().into_iter().flatten())
            .filter_map(Value::as_str)
            .collect();
        text.push_str(&format!(
            "- {}: {} Choices: {}\n",
            field["id"].as_str().unwrap_or(""),
            field["description"].as_str().unwrap_or(""),
            serde_json::to_string(&choices).unwrap_or_default()
        ));
    }
    let flags = input["flags"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    if !flags.is_empty() {
        text.push_str("\n\"flags\": for each of these, true or false:\n");
    }
    for flag in flags {
        let (id, description) = (flag["id"].as_str(), flag["description"].as_str());
        text.push_str(&format!(
            "- {}: {}\n",
            id.unwrap_or(""),
            description.unwrap_or("")
        ));
    }
    let limit = input["rationale_max_bytes"].as_u64().unwrap_or(0);
    text.push_str(&format!(
        "\n\"rationale\": why, in at most {limit} bytes.\n"
    ));
    let context = (input["context"]["items"].as_array().into_iter().flatten())
        .map(|i| {
            format!(
                "## {}\n{}\n",
                i["label"].as_str().unwrap_or(""),
                i["text"].as_str().unwrap_or("")
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    (text, context)
}

/// The catalog's answer from the model's: exactly one label per field in
/// field order, each one of that field's choices; the flags that hold, in
/// request order; the rationale, cut to `rationale_max_bytes`. An answer
/// that names no valid choice for a field, or leaves a flag out, is an error.
pub fn assess_answer(
    answer: &Value,
    input: &Value,
) -> Result<(Vec<String>, Vec<String>, String), String> {
    let mut labels = vec![];
    for field in input["fields"].as_array().into_iter().flatten() {
        let id = field["id"].as_str().unwrap_or("");
        let label = (answer["labels"][id].as_str())
            .filter(|l| {
                field["choices"]
                    .as_array()
                    .is_some_and(|c| c.iter().any(|x| x == l))
            })
            .ok_or_else(|| format!("no valid label for field {id}"))?;
        labels.push(label.to_owned());
    }
    let mut flags = vec![];
    for id in ids(&input["flags"]) {
        let holds = answer["flags"][id]
            .as_bool()
            .ok_or_else(|| format!("no answer for flag {id}"))?;
        if holds {
            flags.push(id.to_owned());
        }
    }
    let rationale = answer["rationale"].as_str().ok_or("no rationale")?;
    let max = input["rationale_max_bytes"].as_u64().unwrap_or(0) as usize;
    Ok((labels, flags, truncate(rationale, max).to_owned()))
}

/// The `failed` or `refused` output of a judgment.
pub fn assess_ended(reference: &str, subject: &Value, reason: &str) -> Value {
    json!({"reason": reason, "reference": reference, "subject": subject})
}

// ------------------------------------------------------- aq.work.route_needed

/// A task still without a route is asked about again after this long.
pub const ROUTE_NEEDED_REPEAT_MS: i64 = 120_000;
/// Placement refreshes a waiting task's row at most this often.
pub const ROUTE_WANTED_REFRESH_MS: i64 = 30_000;
/// A row placement has not refreshed for this long is no longer waiting.
pub const ROUTE_WANTED_FRESH_MS: i64 = 300_000;

/// A run that read the task this long ago no longer holds back a new ask,
/// whether or not its end is on record (run history is a rolling record and
/// may not be readable).
pub const ROUTE_RUN_BOUND_MS: i64 = 900_000;

/// One task's `route_needed_emissions` row, as the emitter reads it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Emission {
    /// The task version placement last saw waiting.
    pub wanted_version: i64,
    /// The version last asked about (0 with `emitted_at_ms` 0: never).
    pub emitted_version: i64,
    pub emitted_at_ms: i64,
    /// `not_offered` or `unroutable`: how the last ask ended for good.
    pub outcome: Option<String>,
    pub artifact_digest: Option<String>,
    pub options_digest: Option<String>,
    /// A run that read this task since the last ask has not finished.
    pub run_unfinished: bool,
}

/// Whether to send `aq.work.route_needed` for the row now (one event per
/// cause): never asked, or the task changed, at once; a task raised as
/// `unroutable` only under another routing artifact; one refused
/// `not_offered` only under another artifact or other Settings offers
/// (`offers`, their digest); otherwise again after
/// [`ROUTE_NEEDED_REPEAT_MS`], unless a run for it is unfinished.
pub fn route_needed_due(e: &Emission, now_ms: i64, artifact: &str, offers: Option<&str>) -> bool {
    if e.emitted_at_ms == 0 || e.wanted_version != e.emitted_version {
        return true;
    }
    let other_artifact = e.artifact_digest.as_deref() != Some(artifact);
    match e.outcome.as_deref() {
        Some("unroutable") => other_artifact,
        Some(_) => other_artifact || e.options_digest.as_deref() != offers,
        None => now_ms - e.emitted_at_ms >= ROUTE_NEEDED_REPEAT_MS && !e.run_unfinished,
    }
}

/// The trusted `aq.work.route_needed` event for a task version.
pub fn route_needed_event(
    event_id: Uuid,
    task: Uuid,
    version: i64,
    now_ms: i64,
    contract: &aq_playbook_host::model::EventContract,
) -> Result<aq_playbook_host::model::EventEnvelope, aq_playbook_host::model::ModelError> {
    let revision = version.max(0) as u64;
    Ok(aq_playbook_host::model::EventEnvelope {
        producer: "aq.host.bridge@1".into(),
        event_id: event_id.to_string(),
        contract: aq_playbook_host::model::ContractRef {
            name: contract.name.clone(),
            fingerprint: contract.fingerprint()?,
        },
        scope: "project".into(),
        correlation: Default::default(),
        subject: Some(task.to_string()),
        revision: Some(revision),
        causation: None,
        occurred_at_ms: Some(now_ms),
        observed_at_ms: Some(now_ms),
        payload: json!({"subject": {"id": task.to_string(), "revision": revision}}),
    })
}

// --------------------------------------------------- routing failures (W6)

/// A ready task still without a route after this long is raised.
pub const UNROUTED_RAISE_MS: i64 = 600_000;

/// The reasons a playbook gave, as a sentence: each reason's own words, or
/// its code when it has none.
fn reason_words(reasons: &Value) -> String {
    let words: Vec<&str> = (reasons.as_array().into_iter().flatten())
        .filter_map(|r| {
            r["detail"]
                .as_str()
                .filter(|d| !d.trim().is_empty())
                .or(r["code"].as_str())
        })
        .collect();
    match words.is_empty() {
        true => "none given".into(),
        false => words.join(" "),
    }
}

/// Why a task that could not be routed is raised, once per cause: the cause
/// key (the outcome at the task version) and what happened in the owner's
/// words. `input` is the `work.route` or `work.unroutable` input, or null
/// for a task that only waited.
pub fn routing_failure(outcome: &str, version: i64, input: &Value) -> (String, String) {
    let list = |name: &str| {
        let names = names(&input[name]);
        (!names.is_empty()).then(|| format!(", {name} {}", names.join(", ")))
    };
    let words = match outcome {
        "not_offered" => format!(
            "The routing playbook chose level {}{}{} for this task, but nothing in Settings can \
             take it there. Assign a profile to that level in Settings > Intelligence (or add the \
             harness or provider to it), change the routing document, or set the task's route \
             yourself. The playbook's reasons: {}",
            input["level"].as_str().unwrap_or("unknown"),
            list("harnesses").unwrap_or_default(),
            list("providers").unwrap_or_default(),
            reason_words(&input["reasons"]),
        ),
        "unroutable" => format!(
            "The routing document allows no agent to take this task, so it is left unrouted. \
             Change the task or the routing document, or set the task's route yourself. The \
             playbook's reasons: {}",
            reason_words(&input["reasons"]),
        ),
        _ => "This task has been ready for more than 10 minutes and has no route. The routing \
              playbook has not routed it: its run may have failed or be stuck, or its judgment \
              may have failed. Look at the playbook runs for this task, or set the task's route \
              yourself."
            .to_string(),
    };
    (format!("{outcome}@{version}"), words)
}

#[cfg(test)]
mod tests;
