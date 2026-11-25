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
        described: Default::default(),
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
                "aq:project_id": "p", "aq:role": "reviewer"}});
    let task = Uuid::from_u128(9);
    let out = read_output(task, 4, &WorkFacts::of(&node, false, 2, 3));
    assert_eq!(
        out,
        json!({
            "reference":format!("{task}@4"),"subject":{"id":task.to_string(),"revision":4},
            "facts":{"charged_failures":2,"description":"B","ended_attempts":3,"kind":"bugfix","level_hint":"complex-high","origin":"unknown","route":"none","title":"T"},
        })
    );
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
}

#[test]
fn invalid_and_unoffered_level_hints_are_distinct() {
    // A level Settings does not offer is still the level asked for.
    assert_eq!(normalize_hint(Some(" deep-max ")), "deep-max");
    assert_eq!(normalize_hint(None), "");
}

#[test]
fn route_facts_identify_the_owner_of_a_readable_route() {
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

fn provenance() -> Provenance<'static> {
    Provenance {
        artifact_digest: Some("sha256:ab"),
        source_revision: Some(7),
        ended_attempts: 2,
    }
}
#[test]
fn a_route_keeps_only_what_the_level_offers() {
    let from = provenance();
    let route = route_from_input(&input("standard-high", &[], &[]), &options(), from).unwrap();
    assert!(route.harnesses.is_empty() && route.providers.is_empty());
    assert_eq!(
        (route.source, route.provider_intent),
        (Source::Router, ProviderIntent::Any)
    );
}

#[test]
fn a_routed_task_receives_the_active_policy_and_source_revision() {
    let route =
        route_from_input(&input("standard-high", &[], &[]), &options(), provenance()).unwrap();
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
}

fn filtered_route() -> Route {
    let from = provenance();
    // Unoffered names are dropped; what is left is the route.
    let mut asked = input(
        "standard-high",
        &["claude", "cursor", "claude"],
        &["anthropic", "google"],
    );
    asked["prefer"] = json!(["cursor", "claude", "codex"]);
    asked["hold"] = json!(true);
    asked["classification"] = json!({"reference": "judgment:1", "fields": ["risk"], "labels": ["low"], "flags": [], "rationale": "r"});
    route_from_input(&asked, &options(), from).unwrap()
}

#[test]
fn offered_routes_keep_order_and_classification_with_legacy_hold_as_preference() {
    let route = filtered_route();
    assert_eq!(
        (route.harnesses, route.providers, route.prefer),
        (
            vec!["claude".to_string()],
            vec!["anthropic".to_string()],
            vec!["claude".to_string(), "codex".to_string()]
        )
    );
    assert_eq!(route.provider_intent, ProviderIntent::Any);
    assert_eq!(route.judgment.unwrap()["labels"], json!(["low"]));
}

#[test]
fn routed_output_reports_only_selected_names_and_the_current_task_revision() {
    let out = routed_output("t", 3, &filtered_route());
    assert_eq!(
        out,
        json!({"harnesses": ["claude"], "providers": ["anthropic"], "prefer": ["claude", "codex"],
            "reference": "t@3", "subject": {"id": "t", "revision": 3}})
    );
}

#[test]
fn an_unoffered_level_refuses_but_an_unoffered_preference_does_not() {
    let from = Provenance::default();
    for refused in [
        input("complex-high", &[], &[]), // no profile at the level
        input("deep-high", &[], &[]),    // a profile that lists nothing
    ] {
        assert_eq!(route_from_input(&refused, &options(), from), None);
    }
    let route = route_from_input(
        &input("standard-high", &["cursor"], &["google"]),
        &options(),
        from,
    )
    .unwrap();
    assert!(route.harnesses.is_empty());
    assert!(route.providers.is_empty());
    let route = route_from_input(
        &input("standard-high", &["claude"], &["openai"]),
        &options(),
        from,
    )
    .unwrap();
    assert_eq!(route.harnesses, ["claude"]);
    assert_eq!(route.providers, ["openai"]);
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
    let invalid: &[fn(&mut Value)] = &[
        |i| i["fields"] = json!([]),
        |i| i["fields"][1]["id"] = json!("risk"),
        |i| i["fields"][0]["choices"] = json!([]),
        |i| i["fields"][0]["choices"] = json!(["low", "low"]),
        |i| i["flags"][1]["id"] = json!("narrow"),
        |i| i["question"] = json!("  "),
        |i| i["context"]["items"][0]["text"] = json!("x".repeat(70_000)),
    ];
    for change in invalid {
        assert_eq!(with(*change), Some("invalid_request"));
    }
    // A field and a flag may share an id: they are answered apart.
    assert_eq!(with(|i| i["flags"][0]["id"] = json!("risk")), None);
}

#[test]
fn budget_refusals_and_indeterminate_replies_remain_explicit() {
    let ok = assess_input();
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
fn asked_emission() -> Emission {
    Emission {
        wanted_version: 4,
        emitted_version: 4,
        emitted_at_ms: 1_000,
        ..Default::default()
    }
}
#[test]
fn route_needed_is_sent_once_per_cause() {
    let asked = asked_emission();
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
}

#[test]
fn a_repeated_route_waits_for_its_interval_and_for_an_unfinished_run() {
    let asked = asked_emission();
    let due = |e: &Emission, now| route_needed_due(e, now, "art-1", Some("offers-1"));
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
}

#[test]
fn a_settled_route_reconsiders_only_changed_causes() {
    let asked = asked_emission();
    // Raised as unroutable: only another routing artifact asks again.
    let settled = |outcome: &str| Emission {
        outcome: Some(outcome.into()),
        artifact_digest: Some("art-1".into()),
        options_digest: Some("offers-1".into()),
        ..asked.clone()
    };
    let cases = [
        ("unroutable", "art-1", "offers-1", false),
        ("unroutable", "art-2", "offers-1", true),
        ("unroutable", "art-1", "offers-2", false),
        ("not_offered", "art-1", "offers-1", false),
        ("not_offered", "art-2", "offers-1", true),
        ("not_offered", "art-1", "offers-2", true),
    ];
    for (outcome, art, offers, expected) in cases {
        assert_eq!(
            route_needed_due(&settled(outcome), 900000, art, Some(offers)),
            expected,
            "{outcome}, {art}, {offers}"
        );
    }
    let newer = Emission {
        wanted_version: 5,
        ..settled("unroutable")
    };
    assert!(route_needed_due(&newer, 2000, "art-1", Some("offers-1")));
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
    assert_eq!(
        (event.correlation.is_empty(), event.producer.as_str()),
        (true, "aq.host.bridge@1")
    );
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
}

#[test]
fn an_unroutable_task_reports_the_policy_reason() {
    let (cause, words) = routing_failure(
        "unroutable",
        4,
        &json!({"reasons": [{"code": "policy", "detail": "Never route payroll work."}]}),
    );
    assert_eq!(cause, "unroutable@4");
    assert!(words.contains("allows no agent") && words.ends_with("Never route payroll work."));
}

#[test]
fn a_task_left_unrouted_reports_the_wait() {
    let (cause, words) = routing_failure("unrouted", 4, &Value::Null);
    assert_eq!(cause, "unrouted@4");
    assert!(words.contains("more than 10 minutes"));
}
