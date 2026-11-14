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

/// The route a `work.route` input asks for, within what Settings offers at
/// its level: the harness, provider and preference lists are intersected
/// with the level's offers. `None` is `not_offered`: the level has no
/// profile, a non-empty list has nothing left, or no offered pair is in both
/// lists. `hold: true` pins the provider.
pub fn route_from_input(input: &Value, options: &Options, from: Provenance<'_>) -> Option<Route> {
    let class: Class = input["level"].as_str()?.parse().ok()?;
    let offers = options.classes.get(&class).filter(|o| !o.is_empty())?;
    let (asked_harnesses, asked_providers) =
        (names(&input["harnesses"]), names(&input["providers"]));
    let offered = |asked: &[String], pick: fn(&aq_routing::classify::Offer) -> &String| {
        (asked.iter())
            .filter(|n| offers.iter().any(|o| pick(o) == *n))
            .cloned()
            .collect::<Vec<_>>()
    };
    let harnesses = offered(&asked_harnesses, |o| &o.harness);
    let providers = offered(&asked_providers, |o| &o.provider);
    let within = |list: &[String], name: &String| list.is_empty() || list.contains(name);
    if (harnesses.is_empty() && !asked_harnesses.is_empty())
        || (providers.is_empty() && !asked_providers.is_empty())
        || !(offers.iter())
            .any(|o| within(&harnesses, &o.harness) && within(&providers, &o.provider))
    {
        return None;
    }
    let prefer = (names(&input["prefer"]).into_iter())
        .filter(|n| offers.iter().any(|o| &o.harness == n) && within(&harnesses, n))
        .collect();
    let reasons = (input["reasons"].as_array().into_iter().flatten())
        .map(|r| RouteReason {
            code: r["code"].as_str().unwrap_or("").into(),
            detail: r["detail"].as_str().unwrap_or("").into(),
        })
        .collect();
    Some(Route {
        class,
        harnesses,
        providers,
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
    let fields = input["fields"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let choices_ok = |field: &Value| {
        let choices: Vec<&str> = (field["choices"].as_array().into_iter().flatten())
            .filter_map(Value::as_str)
            .collect();
        !choices.is_empty() && distinct(&choices)
    };
    if !bytes.is_ok_and(|b| b <= ASSESS_REQUEST_BYTES)
        || input["question"]
            .as_str()
            .is_none_or(|q| q.trim().is_empty())
        || fields.is_empty()
        || !distinct(&ids(&input["fields"]))
        || !distinct(&ids(&input["flags"]))
        || !fields.iter().all(choices_ok)
    {
        return Some("invalid_request");
    }
    let budget = &input["budget"];
    let over = |name: &str, max: u64| budget[name].as_u64().is_none_or(|v| v > max);
    (over("deadline_ms", max_deadline_ms) || over("max_output_tokens", max_output_tokens))
        .then_some("budget_exceeded")
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
mod tests {
    use super::*;
    use aq_routing::classify::Offer;

    fn options() -> Options {
        let offer = |provider: &str, harness: &str| Offer {
            provider: provider.into(),
            harness: harness.into(),
        };
        Options {
            classes: [
                (
                    "standard-high".parse().unwrap(),
                    vec![offer("anthropic", "claude"), offer("openai", "codex")],
                ),
                ("deep-high".parse().unwrap(), vec![]),
            ]
            .into(),
        }
    }
    fn input(level: &str, harnesses: &[&str], providers: &[&str]) -> Value {
        json!({"level": level, "harnesses": harnesses, "providers": providers, "prefer": [],
            "hold": false, "reasons": [{"code": "level_from_kind"}], "classification": null,
            "subject": {"id": "t", "revision": 1}})
    }

    #[test]
    fn facts_never_include_priority() {
        let node = json!({"title": "T", "body": "B", "version": 4, "task_state": "active",
            "labels": ["x"], "created_by": "someone",
            "metadata": {"aq:priority": 7, "aq:task_type": "bugfix", "aq:intelligence_hint": "Complex-High",
                "aq:project_id": "p", "aq:profile_hint": "h"}});
        let task = Uuid::from_u128(9);
        let out = read_output(task, 4, &WorkFacts::of(&node, false, 2, 3));
        let mut keys: Vec<&String> = out["facts"].as_object().unwrap().keys().collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "charged_failures",
                "description",
                "ended_attempts",
                "kind",
                "level_hint",
                "origin",
                "route",
                "title"
            ]
        );
        assert_eq!(out["facts"]["kind"], "bugfix");
        assert_eq!(out["facts"]["level_hint"], "complex-high");
        assert_eq!(out["facts"]["origin"], "unknown");
        assert_eq!(out["facts"]["route"], "none");
        assert_eq!(
            (
                &out["facts"]["charged_failures"],
                &out["facts"]["ended_attempts"]
            ),
            (&json!(2), &json!(3))
        );
        assert_eq!(out["reference"], format!("{task}@4"));
        assert_eq!(
            out["subject"],
            json!({"id": task.to_string(), "revision": 4})
        );
        assert!(!out.to_string().contains("priority") && !out.to_string().contains("someone"));
    }

    #[test]
    fn facts_are_cut_at_character_boundaries_and_a_hint_that_is_no_level_is_empty() {
        let node = json!({"title": "é".repeat(300), "body": "界".repeat(3000),
            "metadata": {"aq:task_type": "k".repeat(80), "aq:intelligence_hint": "fast"}});
        let facts = WorkFacts::of(&node, true, 5000, 5000);
        assert_eq!(
            (facts.title.len(), facts.description.len(), facts.kind.len()),
            (512, 8190, 64)
        );
        assert_eq!((facts.level_hint.as_str(), facts.origin), ("", "playbook"));
        assert_eq!((facts.charged_failures, facts.ended_attempts), (1000, 1000));
        // A level Settings does not offer is still the level asked for.
        assert_eq!(normalize_hint(Some(" deep-max ")), "deep-max");
        assert_eq!(normalize_hint(None), "");
        let routed = |s: &str| json!({"aq:route": {"c": "standard-high", "l": null, "i": "any", "d": null, "s": s, "n": 1}});
        assert_eq!(
            WorkFacts::of(&json!({"metadata": routed("owner")}), false, 0, 0).route,
            "owner"
        );
        assert_eq!(
            WorkFacts::of(&json!({"metadata": routed("router")}), false, 0, 0).route,
            "router"
        );
        assert_eq!(
            WorkFacts::of(&json!({"metadata": {"aq:route": 1}}), false, 0, 0).route,
            "none"
        );
    }

    #[test]
    fn a_route_keeps_only_what_the_level_offers() {
        let from = Provenance {
            artifact_digest: Some("sha256:ab"),
            source_revision: Some(7),
            ended_attempts: 2,
        };
        let route = route_from_input(&input("standard-high", &[], &[]), &options(), from).unwrap();
        assert!(route.harnesses.is_empty() && route.providers.is_empty());
        assert_eq!(
            (route.source, route.provider_intent),
            (Source::Router, ProviderIntent::Any)
        );
        assert_eq!(route.policy_digest.as_deref(), Some("sha256:ab"));
        assert_eq!(
            (route.structure_revision, route.ended_attempts),
            (Some(7), 2)
        );
        assert_eq!(
            route.reasons,
            [RouteReason {
                code: "level_from_kind".into(),
                detail: String::new()
            }]
        );
        // Unoffered names are dropped; what is left is the route.
        let mut asked = input(
            "standard-high",
            &["claude", "cursor", "claude"],
            &["anthropic", "google"],
        );
        asked["prefer"] = json!(["cursor", "claude", "codex"]);
        asked["hold"] = json!(true);
        asked["classification"] = json!({"reference": "judgment:1", "fields": ["risk"], "labels": ["low"], "flags": [], "rationale": "r"});
        let route = route_from_input(&asked, &options(), from).unwrap();
        assert_eq!(
            (route.harnesses, route.providers, route.prefer),
            (
                vec!["claude".to_string()],
                vec!["anthropic".to_string()],
                vec!["claude".to_string()]
            )
        );
        assert_eq!(route.provider_intent, ProviderIntent::Pinned);
        assert_eq!(route.judgment.unwrap()["labels"], json!(["low"]));
        let out = routed_output("t", 3, &route_from_input(&asked, &options(), from).unwrap());
        assert_eq!(
            out,
            json!({"harnesses": ["claude"], "providers": ["anthropic"], "prefer": ["claude"],
            "reference": "t@3", "subject": {"id": "t", "revision": 3}})
        );
    }

    #[test]
    fn a_level_or_list_with_nothing_offered_is_not_offered() {
        let from = Provenance::default();
        for refused in [
            input("complex-high", &[], &[]),          // no profile at the level
            input("deep-high", &[], &[]),             // a profile that lists nothing
            input("standard-high", &["cursor"], &[]), // nothing left of the harnesses
            input("standard-high", &[], &["google"]), // nothing left of the providers
            input("standard-high", &["claude"], &["openai"]), // each offered, never together
        ] {
            assert_eq!(
                route_from_input(&refused, &options(), from),
                None,
                "{refused}"
            );
        }
        assert_ne!(
            options_digest(&options()),
            options_digest(&Options::default())
        );
    }
    fn assess_input() -> Value {
        json!({"subject": {"id": "t", "revision": 3}, "question": "How risky is this task?",
            "context": {"items": [{"label": "Title", "text": "Fix billing"}, {"label": "Description", "text": "Rounding"}]},
            "fields": [
                {"id": "risk", "description": "Risk of the change.", "choices": ["low", "medium", "high", "unknown"]},
                {"id": "kind", "description": "What it is.", "choices": ["bugfix", "feature"]}],
            "flags": [{"id": "narrow", "description": "Touches one module."}, {"id": "hard", "description": "Unusually hard reasoning."}],
            "rationale_max_bytes": 12, "budget": {"deadline_ms": 120000, "max_output_tokens": 2048}})
    }

    #[test]
    fn an_assessment_schema_asks_each_field_and_flag_by_name() {
        let schema = assess_schema(&assess_input());
        assert_eq!(
            schema["$schema"],
            "https://json-schema.org/draft/2020-12/schema"
        );
        assert_eq!(schema["additionalProperties"], false);
        let labels = &schema["properties"]["labels"];
        assert_eq!(
            labels["properties"]["risk"],
            json!({"type": "string", "enum": ["low", "medium", "high", "unknown"]})
        );
        assert_eq!(
            (
                &labels["additionalProperties"],
                labels["required"].as_array().unwrap().len()
            ),
            (&json!(false), 2)
        );
        let flags = &schema["properties"]["flags"];
        assert_eq!(flags["properties"]["hard"], json!({"type": "boolean"}));
        assert_eq!(flags["required"].as_array().unwrap().len(), 2);
        assert_eq!(
            schema["properties"]["rationale"],
            json!({"type": "string", "maxLength": 12})
        );
        let (instructions, context) = assess_prompt(&assess_input());
        for part in [
            "How risky is this task?",
            "- risk: Risk of the change. Choices: [\"low\",\"medium\",\"high\",\"unknown\"]",
            "- hard: Unusually hard reasoning.",
            "at most 12 bytes",
        ] {
            assert!(instructions.contains(part), "{instructions}");
        }
        assert_eq!(
            context,
            "## Title\nFix billing\n\n## Description\nRounding\n"
        );
    }

    #[test]
    fn an_answer_becomes_one_label_per_field_in_field_order_and_the_flags_that_hold() {
        let input = assess_input();
        let answer = json!({"flags": {"hard": true, "narrow": false}, "labels": {"kind": "bugfix", "risk": "high"},
            "rationale": "Billing is délicat."});
        let (labels, flags, rationale) = assess_answer(&answer, &input).unwrap();
        assert_eq!(
            (labels, flags),
            (
                vec!["high".to_string(), "bugfix".to_string()],
                vec!["hard".to_string()]
            )
        );
        assert_eq!(rationale, "Billing is d");
        let both = json!({"flags": {"hard": true, "narrow": true}, "labels": {"kind": "feature", "risk": "low"}, "rationale": ""});
        assert_eq!(assess_answer(&both, &input).unwrap().1, ["narrow", "hard"]);
        for bad in [
            json!({"flags": {"hard": true, "narrow": false}, "labels": {"kind": "bugfix", "risk": "severe"}, "rationale": ""}),
            json!({"flags": {"hard": true, "narrow": false}, "labels": {"risk": "high"}, "rationale": ""}),
            json!({"flags": {"hard": true}, "labels": {"kind": "bugfix", "risk": "high"}, "rationale": ""}),
            json!({"flags": {"hard": true, "narrow": false}, "labels": {"kind": "bugfix", "risk": "high"}}),
        ] {
            assert!(assess_answer(&bad, &input).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_request_the_host_does_not_run_says_why() {
        let ok = assess_input();
        assert_eq!(assess_refusal(&ok, 900_000, 262_144), None);
        let with = |change: fn(&mut Value)| {
            let mut input = assess_input();
            change(&mut input);
            assess_refusal(&input, 900_000, 262_144)
        };
        let invalid = Some("invalid_request");
        assert_eq!(with(|i| i["fields"] = json!([])), invalid);
        assert_eq!(with(|i| i["fields"][1]["id"] = json!("risk")), invalid);
        assert_eq!(with(|i| i["fields"][0]["choices"] = json!([])), invalid);
        assert_eq!(
            with(|i| i["fields"][0]["choices"] = json!(["low", "low"])),
            invalid
        );
        assert_eq!(with(|i| i["flags"][1]["id"] = json!("narrow")), invalid);
        assert_eq!(with(|i| i["question"] = json!("  ")), invalid);
        assert_eq!(
            with(|i| i["context"]["items"][0]["text"] = json!("x".repeat(70_000))),
            invalid
        );
        // A field and a flag may share an id: they are answered apart.
        assert_eq!(with(|i| i["flags"][0]["id"] = json!("risk")), None);
        assert_eq!(
            assess_refusal(&ok, 60_000, 262_144),
            Some("budget_exceeded")
        );
        assert_eq!(assess_refusal(&ok, 900_000, 1024), Some("budget_exceeded"));
        assert_eq!(
            assess_ended("judgment:1", &ok["subject"], "indeterminate"),
            json!({"reason": "indeterminate", "reference": "judgment:1", "subject": {"id": "t", "revision": 3}})
        );
    }
    #[test]
    fn route_needed_is_sent_once_per_cause() {
        let asked = Emission {
            wanted_version: 4,
            emitted_version: 4,
            emitted_at_ms: 1_000,
            ..Default::default()
        };
        let due = |e: &Emission, now| route_needed_due(e, now, "art-1", Some("offers-1"));
        // Never asked, and a new task version: at once.
        assert!(due(
            &Emission {
                wanted_version: 4,
                ..Default::default()
            },
            1
        ));
        assert!(due(
            &Emission {
                wanted_version: 5,
                ..asked.clone()
            },
            1_001
        ));
        // The same version: again after two minutes, not while a run is unfinished.
        assert!(!due(&asked, 120_999));
        assert!(due(&asked, 121_000));
        assert!(!due(
            &Emission {
                run_unfinished: true,
                ..asked.clone()
            },
            500_000
        ));
        // Raised as unroutable: only another routing artifact asks again.
        let settled = |outcome: &str| Emission {
            outcome: Some(outcome.into()),
            artifact_digest: Some("art-1".into()),
            options_digest: Some("offers-1".into()),
            ..asked.clone()
        };
        assert!(!due(&settled("unroutable"), 900_000));
        assert!(route_needed_due(
            &settled("unroutable"),
            900_000,
            "art-2",
            Some("offers-1")
        ));
        assert!(!route_needed_due(
            &settled("unroutable"),
            900_000,
            "art-1",
            Some("offers-2")
        ));
        // Refused not_offered: another artifact, or other Settings offers.
        assert!(!due(&settled("not_offered"), 900_000));
        assert!(route_needed_due(
            &settled("not_offered"),
            900_000,
            "art-2",
            Some("offers-1")
        ));
        assert!(route_needed_due(
            &settled("not_offered"),
            900_000,
            "art-1",
            Some("offers-2")
        ));
        assert!(due(
            &Emission {
                wanted_version: 5,
                ..settled("unroutable")
            },
            2_000
        ));
    }

    #[test]
    fn the_route_needed_event_names_the_task_version_and_is_valid() {
        let registry = crate::review_domain::CatalogProfile::Host9.registry();
        let (id, task) = (Uuid::from_u128(1), Uuid::from_u128(2));
        let contract = &registry.events["aq.work.route_needed"];
        let event = route_needed_event(id, task, 7, 5_000, contract).unwrap();
        aq_playbook_host::model::validate_event(&event, &registry).unwrap();
        assert_eq!(
            event.payload,
            json!({"subject": {"id": task.to_string(), "revision": 7}})
        );
        assert_eq!(
            (event.subject.as_deref(), event.revision),
            (Some(task.to_string().as_str()), Some(7))
        );
        assert!(event.correlation.is_empty());
        assert_eq!(event.producer, "aq.host.bridge@1");
    }
    #[test]
    fn a_routing_failure_is_keyed_by_cause_and_version_and_says_what_to_do() {
        let input = json!({"level": "deep-high", "harnesses": ["claude"], "providers": [],
            "reasons": [{"code": "deep_reserved", "detail": "Hard analysis needs Deep."}, {"code": "policy"}]});
        let (cause, words) = routing_failure("not_offered", 4, &input);
        assert_eq!(cause, "not_offered@4");
        assert!(
            words.contains("level deep-high, harnesses claude for this task"),
            "{words}"
        );
        assert!(
            words.ends_with("The playbook's reasons: Hard analysis needs Deep. policy"),
            "{words}"
        );
        assert_ne!(routing_failure("not_offered", 5, &input).0, cause);
        let (cause, words) = routing_failure(
            "unroutable",
            4,
            &json!({"reasons": [{"code": "policy", "detail": "Never route payroll work."}]}),
        );
        assert_eq!(cause, "unroutable@4");
        assert!(words.contains("allows no agent") && words.ends_with("Never route payroll work."));
        let (cause, words) = routing_failure("unrouted", 4, &Value::Null);
        assert_eq!(cause, "unrouted@4");
        assert!(words.contains("more than 10 minutes"));
    }
}
