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
}
