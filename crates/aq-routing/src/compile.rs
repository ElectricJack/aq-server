//! Compiling the owner's routing policy (spec 0011 §6, §7). A model reads the
//! plain-language source and what Settings offers, and answers a [`Policy`];
//! [`check`] decides deterministically whether that answer may take effect.
//! The request itself goes through internal work; nothing here calls a model.

use std::collections::BTreeSet;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::classify::{Offer, Options};
use crate::policy::{self, Policy, Problem};

/// What the compiling model is told. It describes the structure and how the
/// planner reads it; it contains no example policy.
pub const COMPILER_INSTRUCTIONS: &str = r#"You compile an owner's routing policy into the structure AQ routes tasks with.

The owner wrote the policy in plain language, the way one engineer describes routing to another. AQ's planner is deterministic: for each task it reads your structure, the task's own fields and one classification of the task by a model, and produces the task's route: the level of model it needs and who may take it. You write the structure once, when the owner saves the policy. You never route a task, and scheduling (which worker, machine or exact model, and when) happens after routing.

The input has three parts: the owner's policy; "What Settings offers", the levels that have an assigned profile and, for each, the harnesses (the agent programs) and providers (the model vendors) that can take work there; and how Settings describes the four levels.

Answer with one JSON object: {"policy": <the structure>, "unresolved": [<sentences>]}. Optional fields may be left out.

# Words used in the structure

- A level is written parent-effort, for example "standard-high" for Standard, high effort. Parents are simple, standard, complex and deep; efforts are low, medium, high and max. Levels are ordered by parent, then effort. Use only the levels listed under What Settings offers.
- Harnesses and providers are the ids listed under What Settings offers; use only those. Map the owner's words to them: the name of an agent or a vendor to its harness or provider, and "local model" to the local provider and the harness that serves it there.
- A risk is low, medium, high or very_high.
- Flags are facts the classifier reports about a task: narrow, test_verified and deep_reasoning. Their meaning comes from the owner's policy when it defines them; otherwise narrow means the requirements are settled and the files to change are known, test_verified means a test checks the result, and deep_reasoning means the work needs advanced reasoning.
- Kind ids are short lowercase words. Use these where one fits the owner's words: feature, bugfix, refactor, test, docs, chore, sync, spec, design, plan, review, research, analysis, art. Otherwise make one with underscores. Lane ids are short lowercase words joined by hyphens that say what the lane is for.

# The fields, in the order the planner applies them

version is 2.

1. Kind. The planner takes the task's own kind if it is a key of kinds, else the kind the classifier answered if it is one, else default_kind. default_kind must be one of the kinds: the one the policy treats as ordinary work.

2. Starting level, from kinds.<kind>:
- class: the level the kind starts at.
- max_class: the highest level a task of this kind may start at; a level the task asks for, or the classifier answers, above it is lowered to it. When the policy caps what may be requested for a kind, that cap is max_class; when it is silent, use the highest offered level. class may not be above max_class.
- escalate (or null): {after_failures, to_class}. Once the task has had after_failures failed attempts it starts at to_class. Only attempts that failed for a real reason are counted; outages, lost workers and stopped attempts never are. Until then a kind that escalates starts at class, whatever was asked.
For a kind that does not escalate, the start is the level the task asked for, else the level the classifier answered, else class (when the task could not be classified). The classifier's level is used, so classifier_instructions must give each kind the same level as its class.

3. Lane. kinds.<kind>.lane (or null) names a lane for all work of that kind. origins.<origin> describes tasks by where they came from: its lane (or null) applies to tasks whose kind has no lane, because a kind's lane always wins over an origin's, and narrow set to false keeps them off cheaper lanes (true or null does not). Use the origin repair for tasks that repair a failed integration or build; add another origin only when the policy names another source of tasks. Lanes are the list lanes, each with an id that kinds, origins and reservations refer to. A matched lane applies:
- class (or null): sets the level, unless the task escalated.
- harnesses and providers: the only ones that may take the task. Empty means no restriction.
- prefer: harnesses tried first.
- hold: true makes the task wait for its allowed harnesses rather than go elsewhere. Use it only when the policy says to wait.
A lane with cheap false has no level map (classes), and requires and max_risk do nothing on it. It must restrict something (class, harnesses, providers, prefer or hold) unless a reservation names it.

4. Risk. risk.<risk> applies to work at that risk:
- min_class: the lowest level it may run at; a lower level is raised to it.
- relax (or null): {class, requires}: when every flag in requires is set, the floor is this class instead. It may not be above min_class.
- harnesses: the only harnesses that may take it, narrowing the lane's list. Empty means no restriction.
A risk with no entry has no floor and no restriction. Always include medium, because a task that cannot be classified is treated as medium risk; if the policy sets no floor for medium risk, use the lowest offered level and no harnesses.

5. Cheaper lanes (cheap true). One is tried only when the task has no lane yet, its kind has narrow true, its origin does not have narrow false, and the task was classified. kinds.<kind>.narrow says whether work of the kind may take a cheaper lane. The planner takes the first cheaper lane, in list order, whose requires flags are all set, whose max_risk is at or above the task's risk, whose classes has the task's current level as a key, whose mapped level is not below the risk floor, and that has an offer at that level within its own lists. On it:
- classes maps each level the lane takes to the level the task moves to; map a level to itself to keep it.
- harnesses, providers and prefer are as for any lane, but they replace the lists from the risk rule, so list everyone who may take such work, or leave them empty for anyone.
- max_risk is required: the highest risk the lane takes.
- requires: the flags the task must have.
- A cheaper lane has no class.

6. Preference: the lane's prefer if it has one, else kinds.<kind>.prefer_harnesses. A preference changes who is tried first and never shuts anyone out.

7. Reservations: reserved lists {classes, harness, only_lanes}: at those levels the harness is removed unless the task's lane is one of only_lanes. Use it when the policy keeps a harness at certain levels for certain work.

8. If nothing offered at the final level is allowed, the task is left unrouted and the owner is told.

Whatever the structure says, the planner also enforces: a Deep level is only for work the classifier flags deep_reasoning, and anything else at a Deep level runs at Complex, high effort; risk never raises work to Deep, so min_class and relax.class are never Deep levels; a cheaper lane needs max_risk and replaces the risk rule's lists; a kind's lane wins over an origin's; a lane that is not cheap has no level map. How busy providers are and a task's priority are never inputs.

# How to compile

- Copy the owner's rules. Do not add a rule, and do not strengthen or weaken one. Every restriction in the structure must come from a sentence of the policy.
- Where the policy states a preference ("first", "prefer", "should go to ... to save ..."), it is a preference: write it in prefer or prefer_harnesses, not as an allowlist, unless the policy also says who may not take that work.
- Where the policy is silent, the structure stays permissive: no lane, empty lists, no reservation, max_class at the highest offered level, narrow true, and no risk entry other than medium.
- Who may take work at a risk goes in risk.<risk>.harnesses. A rule for one kind of work goes in a lane that kind names. An exception for narrow or tested work goes in relax or in a cheaper lane.
- A rule that limits everyone the policy does not name to some risks is written by listing, in risk.<risk>.harnesses for every higher risk, the harnesses the policy does allow there.

# classifier_instructions

Write instructions for a model that classifies one task at a time. It answers the task's kind (one of your kind ids), the level it needs (one of the offered levels), its risk, the three flags, and short reasons. In the owner's terms, say: what each kind of work is and the level it needs (the same as its class, with any exception the policy gives); how to judge risk, with the areas the policy puts at each risk, how to choose when several apply, and what to answer when it cannot tell; when each flag is true; and when a Deep level applies. Never mention providers, harnesses, load or priority there: the classifier describes the task and never chooses who takes it.

# What you cannot express

If a sentence of the policy cannot be written with these fields, or names a level, harness or provider that Settings does not offer, leave it out of the structure and copy the sentence from the policy into unresolved. Do not resolve it by restricting more or less than the policy says. The answer must still be valid JSON that matches the schema. Do not list sentences that only explain or define. When everything is expressed, unresolved is [].
"#;

/// The compile answer: the policy, and the source sentences it could not express.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Answer {
    pub policy: Policy,
    #[serde(default)]
    pub unresolved: Vec<String>,
}

/// The result of checking a compile answer.
#[derive(Clone, Debug, PartialEq)]
pub enum Compiled {
    /// May take effect.
    Valid {
        policy: Policy,
        digest: String,
        summary: Vec<String>,
    },
    /// May not: what [`policy::validate`] reports, and the sentences the
    /// compiler could not express.
    Refused {
        policy: Option<Policy>,
        problems: Vec<Problem>,
        unresolved: Vec<String>,
    },
}

/// What a finished compile records.
#[derive(Clone, Debug, PartialEq)]
pub struct Recorded {
    /// `active`, `superseded` or `failed`.
    pub status: &'static str,
    /// The policy that checked, its digest and its summary.
    pub valid: Option<(Policy, String, Vec<String>)>,
    pub error: Option<String>,
    pub problems: Vec<Problem>,
    pub unresolved: Vec<String>,
}

/// What to record for a compile that answered `answer`, or failed with that
/// error. A policy that checks against `options` is active, or superseded when
/// a later revision has been submitted for compiling (`newer`); anything else
/// failed, with each problem and unresolved sentence in the error.
pub fn record(answer: Result<&Value, String>, options: &Options, newer: bool) -> Recorded {
    let failed = |error: String, problems, unresolved| Recorded {
        status: "failed",
        valid: None,
        error: Some(error),
        problems,
        unresolved,
    };
    match answer.map(|answer| check(answer, options)) {
        Err(error) => failed(error, vec![], vec![]),
        Ok(Compiled::Refused {
            problems,
            unresolved,
            ..
        }) => failed(error_text(&problems, &unresolved), problems, unresolved),
        Ok(Compiled::Valid {
            policy,
            digest,
            summary,
        }) => Recorded {
            status: if newer { "superseded" } else { "active" },
            valid: Some((policy, digest, summary)),
            error: None,
            problems: vec![],
            unresolved: vec![],
        },
    }
}

/// JSON Schema of the compile answer: `{"policy": <policy schema>, "unresolved": [string]}`.
pub fn answer_schema(options: &Options) -> Value {
    let mut policy = policy::schema(options);
    let schema = policy.as_object_mut().and_then(|p| p.remove("$schema"));
    json!({
        "$schema": schema,
        "type": "object",
        "additionalProperties": false,
        "required": ["policy"],
        "properties": {
            "policy": policy,
            "unresolved": {"type": "array", "items": {"type": "string"}},
        }
    })
}

/// The compiler's input: the source, what Settings offers, and the four level
/// descriptions (`(parent id, description)`).
pub fn input(source: &str, options: &Options, parents: &[(String, String)]) -> String {
    let mut out = String::from("# The owner's routing policy\n\n");
    out.push_str(source);
    out.push_str(
        "\n\n# What Settings offers\n\nThe levels with an assigned profile, and the harnesses \
         that can take work at each, with their providers:\n\n",
    );
    for (class, offers) in &options.classes {
        let who: Vec<String> = offers
            .iter()
            .map(|o| match policy::provider_words(&o.provider) {
                words if words.eq_ignore_ascii_case(&o.provider) => {
                    format!("{} (provider {})", o.harness, o.provider)
                }
                words => format!("{} (provider {}, {words})", o.harness, o.provider),
            })
            .collect();
        let who = if who.is_empty() {
            "no qualified harness at the moment".to_string()
        } else {
            who.join(", ")
        };
        out.push_str(&format!(
            "- {class} ({}): {who}\n",
            policy::class_words(*class)
        ));
    }
    let named = |ids: BTreeSet<&str>, words: fn(&str) -> String| -> String {
        let items: Vec<String> = ids
            .into_iter()
            .map(|id| format!("{id} ({})", words(id)))
            .collect();
        if items.is_empty() {
            "none".into()
        } else {
            items.join(", ")
        }
    };
    let harnesses = options.offers().map(|o| o.harness.as_str()).collect();
    let providers = options.offers().map(|o| o.provider.as_str()).collect();
    out.push_str(&format!(
        "\nHarnesses: {}.\nProviders: {}.\n\n# The levels as Settings describes them\n\n",
        named(harnesses, policy::harness_words),
        named(providers, policy::provider_words)
    ));
    for (id, description) in parents {
        let parent = policy::humanize(id);
        out.push_str(&format!("- {parent}: {}\n", description.trim()));
    }
    out
}

/// Parses and validates a compile answer against what Settings offers now.
pub fn check(answer: &Value, options: &Options) -> Compiled {
    let answer: Answer = match serde_json::from_value(answer.clone()) {
        Ok(answer) => answer,
        Err(e) => {
            return Compiled::Refused {
                policy: None,
                problems: vec![Problem {
                    path: "policy".into(),
                    message: format!("the compiled policy does not parse: {e}"),
                }],
                unresolved: vec![],
            };
        }
    };
    let problems = policy::validate(&answer.policy, options);
    let unresolved: Vec<String> = (answer.unresolved.into_iter())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if problems.is_empty() && unresolved.is_empty() {
        return Compiled::Valid {
            digest: policy::digest(&answer.policy),
            summary: policy::summary(&answer.policy),
            policy: answer.policy,
        };
    }
    Compiled::Refused {
        policy: Some(answer.policy),
        problems,
        unresolved,
    }
}

/// One line per problem (`path: message`), then one per unresolved sentence.
pub fn error_text(problems: &[Problem], unresolved: &[String]) -> String {
    let lines = problems
        .iter()
        .map(|p| format!("{}: {}", p.path, p.message));
    let sentences = unresolved
        .iter()
        .map(|s| format!("Could not be resolved: {s}"));
    lines.chain(sentences).collect::<Vec<_>>().join("\n")
}

/// `"sha256:"` and the hex SHA-256 of the source text.
pub fn source_digest(source: &str) -> String {
    use sha2::{Digest, Sha256};
    let hex: String = Sha256::digest(source.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("sha256:{hex}")
}

/// A profile's offers: each (provider, harness) pair of its candidates whose
/// harness is in `qualified`, once, in candidate order.
pub fn offers<'a>(
    candidates: impl IntoIterator<Item = (&'a str, &'a [String])>,
    qualified: &BTreeSet<&str>,
) -> Vec<Offer> {
    let mut out: Vec<Offer> = Vec::new();
    for (provider, harnesses) in candidates {
        for harness in harnesses.iter().filter(|h| qualified.contains(h.as_str())) {
            let offer = Offer {
                provider: provider.into(),
                harness: harness.clone(),
            };
            if !out.contains(&offer) {
                out.push(offer);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;

    /// Every property name anywhere in `schema`.
    fn property_names(schema: &Value, out: &mut BTreeSet<String>) {
        match schema {
            Value::Object(map) => {
                if let Some(Value::Object(props)) = map.get("properties") {
                    out.extend(props.keys().cloned());
                }
                for v in map.values() {
                    property_names(v, out);
                }
            }
            Value::Array(items) => items.iter().for_each(|v| property_names(v, out)),
            _ => {}
        }
    }

    #[test]
    fn the_instructions_explain_every_field_and_stay_under_12_kib() {
        assert!(
            COMPILER_INSTRUCTIONS.len() < 12 * 1024,
            "{} bytes",
            COMPILER_INSTRUCTIONS.len()
        );
        let mut names = BTreeSet::new();
        property_names(&answer_schema(&fixtures::options()), &mut names);
        assert!(names.len() > 30, "{names:?}");
        for name in names {
            assert!(
                COMPILER_INSTRUCTIONS.contains(&name),
                "the instructions never mention {name}"
            );
        }
        for rule in [
            "deep_reasoning",
            "never raises work to Deep",
            "replace",
            "wins over",
            "preference",
            "silent",
            "unresolved",
            "local model",
            "Never mention providers",
        ] {
            assert!(COMPILER_INSTRUCTIONS.contains(rule), "{rule}");
        }
    }

    #[test]
    fn the_instructions_contain_neither_the_reference_prose_nor_the_reference_policy() {
        let prose = include_str!("../../../docs/specs/0011-routing-policy-default.md");
        let flat = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        let instructions = flat(COMPILER_INSTRUCTIONS);
        for sentence in flat(prose).split(['.', ':']) {
            let sentence = sentence.trim();
            if sentence.len() >= 30 {
                assert!(!instructions.contains(sentence), "{sentence}");
            }
        }
        for sentence in flat(fixtures::CLASSIFIER_INSTRUCTIONS).split('.') {
            let sentence = sentence.trim();
            if sentence.len() >= 30 {
                assert!(!instructions.contains(sentence), "{sentence}");
            }
        }
        for lane in ["local-first", "local-medium-risk", "deep-analysis"] {
            assert!(!instructions.contains(lane), "{lane}");
        }
    }

    #[test]
    fn the_answer_schema_wraps_the_policy_schema_with_an_optional_unresolved_list() {
        let options = fixtures::options();
        let s = answer_schema(&options);
        assert_eq!(s["$schema"], "https://json-schema.org/draft/2020-12/schema");
        assert_eq!(s["additionalProperties"], false);
        assert_eq!(s["required"], json!(["policy"]));
        let mut policy = policy::schema(&options);
        policy.as_object_mut().unwrap().remove("$schema");
        assert_eq!(s["properties"]["policy"], policy);
        assert_eq!(
            s["properties"]["unresolved"],
            json!({"type": "array", "items": {"type": "string"}})
        );
    }

    #[test]
    fn the_input_holds_the_source_the_offered_levels_in_words_and_the_parent_descriptions() {
        let options = fixtures::options();
        let parents: Vec<(String, String)> = [
            ("simple", "Routine work."),
            ("standard", "Most work."),
            ("complex", "Specs and evaluation."),
            ("deep", "Advanced reasoning."),
        ]
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
        let source = "# Routing\n\nBug fixes are Standard.\n";
        let text = input(source, &options, &parents);
        assert!(text.contains(source), "{text}");
        let offers = text
            .split("# What Settings offers")
            .nth(1)
            .expect("offers section");
        assert!(text.find(source).unwrap() < text.find("# What Settings offers").unwrap());
        assert!(
            offers.contains(
                "- standard-medium (Standard, medium effort): claude (provider anthropic), codex (provider openai), opencode (provider ollama, local models)"
            ),
            "{offers}"
        );
        assert!(offers.contains(
            "- deep-high (Deep, high effort): claude (provider anthropic), codex (provider openai)"
        ));
        assert!(!offers.contains("simple-medium"));
        assert!(offers.contains("Providers: anthropic (Anthropic), google (Google), ollama (local models), openai (OpenAI)."), "{offers}");
        assert!(
            offers.contains(
                "Harnesses: claude (Claude), codex (Codex), gemini (Gemini), opencode (OpenCode)."
            ),
            "{offers}"
        );
        let levels = text.split("# The levels").nth(1).expect("levels section");
        assert!(levels.contains("- Simple: Routine work."));
        assert!(levels.contains("- Deep: Advanced reasoning."));
        assert!(text.find("# What Settings offers").unwrap() < text.find("# The levels").unwrap());
    }

    fn answer(policy: &Policy, unresolved: &[&str]) -> Value {
        json!({"policy": policy, "unresolved": unresolved})
    }

    #[test]
    fn a_valid_answer_with_nothing_unresolved_may_take_effect_with_its_digest_and_summary() {
        let (policy, options) = fixtures::reference();
        let mut value = answer(&policy, &[]);
        assert_eq!(
            check(&value, &options),
            Compiled::Valid {
                digest: policy::digest(&policy),
                summary: policy::summary(&policy),
                policy: policy.clone(),
            }
        );
        // `unresolved` is optional.
        value.as_object_mut().unwrap().remove("unresolved");
        assert!(matches!(check(&value, &options), Compiled::Valid { .. }));
        // Blank entries are not sentences.
        let blank = answer(&policy, &["  "]);
        assert!(matches!(check(&blank, &options), Compiled::Valid { .. }));
    }

    #[test]
    fn an_unresolved_sentence_refuses_an_otherwise_valid_policy() {
        let (policy, options) = fixtures::reference();
        let sentence = "Art and visual design go to the plotter.";
        match check(&answer(&policy, &[sentence]), &options) {
            Compiled::Refused {
                policy: parsed,
                problems,
                unresolved,
            } => {
                assert_eq!(parsed, Some(policy));
                assert_eq!(problems, vec![]);
                assert_eq!(unresolved, vec![sentence.to_string()]);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn validation_problems_and_unparsable_answers_are_refused_with_paths() {
        let (mut policy, options) = fixtures::reference();
        policy.kinds.get_mut("chore").unwrap().class = "simple-medium".parse().unwrap();
        match check(&answer(&policy, &[]), &options) {
            Compiled::Refused { problems, .. } => {
                assert_eq!(problems.len(), 1);
                assert_eq!(problems[0].path, "kinds.chore.class");
            }
            other => panic!("{other:?}"),
        }
        match check(&json!({"policy": {"version": 2}}), &options) {
            Compiled::Refused {
                policy: None,
                problems,
                unresolved,
            } => {
                assert_eq!(problems.len(), 1);
                assert_eq!(problems[0].path, "policy");
                assert!(
                    problems[0].message.contains("missing field"),
                    "{problems:?}"
                );
                assert!(unresolved.is_empty());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_record_activates_a_policy_that_checks_unless_a_later_revision_compiles() {
        let (policy, options) = fixtures::reference();
        let value = answer(&policy, &[]);
        let valid = Some((
            policy.clone(),
            policy::digest(&policy),
            policy::summary(&policy),
        ));
        let active = record(Ok(&value), &options, false);
        assert_eq!(
            active,
            Recorded {
                status: "active",
                valid: valid.clone(),
                error: None,
                problems: vec![],
                unresolved: vec![],
            }
        );
        let superseded = record(Ok(&value), &options, true);
        assert_eq!((superseded.status, superseded.valid), ("superseded", valid));
        let refused = record(
            Ok(&answer(&policy, &["Art goes to the plotter."])),
            &options,
            false,
        );
        assert_eq!(refused.status, "failed");
        assert_eq!(refused.valid, None);
        assert_eq!(refused.unresolved, ["Art goes to the plotter."]);
        assert_eq!(
            refused.error.as_deref(),
            Some("Could not be resolved: Art goes to the plotter.")
        );
        let failed = record(Err("The compile did not finish".into()), &options, false);
        assert_eq!(
            failed,
            Recorded {
                status: "failed",
                valid: None,
                error: Some("The compile did not finish".into()),
                problems: vec![],
                unresolved: vec![],
            }
        );
    }

    #[test]
    fn the_error_text_lists_problems_by_path_then_unresolved_sentences() {
        let problems = vec![Problem {
            path: "kinds.chore.class".into(),
            message: "Simple, medium effort has no assigned profile in Settings".into(),
        }];
        let unresolved = vec!["Art goes to the plotter.".to_string()];
        assert_eq!(
            error_text(&problems, &unresolved),
            "kinds.chore.class: Simple, medium effort has no assigned profile in Settings\n\
             Could not be resolved: Art goes to the plotter."
        );
        assert_eq!(error_text(&[], &[]), "");
    }

    #[test]
    fn source_digest_is_the_sha256_of_the_text() {
        assert_eq!(
            source_digest("abc"),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn offers_keep_qualified_harnesses_once_in_candidate_order() {
        let qualified: BTreeSet<&str> = ["claude", "codex"].into();
        let a = vec!["claude".to_string(), "cursor".to_string()];
        let b = vec!["codex".to_string(), "claude".to_string()];
        let candidates = [
            ("anthropic", a.as_slice()),
            ("openai", b.as_slice()),
            ("anthropic", a.as_slice()),
        ];
        let pairs: Vec<(String, String)> = offers(candidates, &qualified)
            .into_iter()
            .map(|o| (o.provider, o.harness))
            .collect();
        assert_eq!(
            pairs,
            [
                ("anthropic".to_string(), "claude".to_string()),
                ("openai".to_string(), "codex".to_string()),
                ("openai".to_string(), "claude".to_string()),
            ]
        );
    }
}
