use super::*;

#[test]
fn role_document_parses() {
    let role = parse_role(ROLE).unwrap();
    assert_eq!(
        role,
        Role {
            id: "developer".into(),
            name: "Developer".into(),
            description: "Makes the change a task asks for and reports evidence.".into(),
            execution: "sandbox".into(),
            operations: vec![
                "task.read".into(),
                "knowledge.read".into(),
                "source.write".into()
            ],
            enabled: true,
            role_md:
                "You implement the change your task describes.\n\n```md\n## Not a heading\n```"
                    .into(),
            rules_md: "- Change only what the task asks for.".into(),
        }
    );
}
#[test]
fn role_optional_parts_default() {
    let source = "---\nid: r\nname: \"[R] \\\"x\\\"\"\nexecution: e\noperations: []\n---\n\n## Role\nDo it.\n";
    let role = parse_role(source).unwrap();
    assert!(role.enabled && role.rules_md.is_empty() && role.description.is_empty());
    assert!(role.operations.is_empty());
    assert_eq!(role.name, "[R] \"x\"");
    assert_eq!(parse_role(&role.to_markdown()).unwrap(), role);
}
const ROLE_EDITS: &[(&str, &str, &str)] = &[
    (
        "## Rules",
        "## Rule",
        "line 16: unknown heading `## Rule`; this document takes `## Role` and `## Rules`",
    ),
    (
        "---\n## Role",
        "---\nIntro.\n## Role",
        "line 9: text before the first heading: `Intro.`",
    ),
    (
        "enabled: true",
        "model: opus",
        "line 7: unknown frontmatter key `model`",
    ),
    (
        "enabled: true",
        "id: other",
        "line 7: duplicate frontmatter key `id`",
    ),
    (
        "execution: sandbox\n",
        "",
        "line 1: frontmatter is missing `execution`",
    ),
    (
        "## Role",
        "## Rules",
        "line 16: duplicate heading `## Rules`",
    ),
    (
        "You implement the change your task describes.\n\n```md\n## Not a heading\n```\n",
        "",
        "line 9: `## Role` is empty",
    ),
    (
        "enabled: true",
        "enabled: yes",
        "line 7: `enabled` expects `true` or `false`",
    ),
    (
        "[task.read, knowledge.read, source.write]",
        "task.read",
        "line 6: `operations` expects a list such as `[a, b]`",
    ),
    (
        "[task.read,",
        "[task read,",
        "line 6: `operations` has a malformed list item `task read`",
    ),
    ("name: Developer", "name:", "line 3: `name` is empty"),
    (
        "name: Developer",
        "name: [Developer]",
        "line 3: `name` expects text, not a list; quote text that starts with `[`",
    ),
    (
        "name: Developer",
        "name",
        "line 3: frontmatter expects `key: value`, found `name`",
    ),
];
#[test]
fn role_document_refusals_name_the_fault() {
    let edit = |from: &str, to: &str| {
        assert!(ROLE.contains(from));
        role_error(&ROLE.replacen(from, to, 1))
    };
    for &(original, replacement, expected) in ROLE_EDITS {
        assert_eq!(edit(original, replacement), expected);
    }

    assert_eq!(
        role_error(&ROLE[..ROLE.find("## Role").unwrap()]),
        "`## Role` is missing"
    );

    assert_eq!(
        role_error(&ROLE[4..]),
        "line 1: a document starts with a `---` line"
    );
    assert_eq!(
        role_error("---\nid: developer\n"),
        "frontmatter is not closed by a `---` line"
    );
}
#[test]
fn intelligence_document_parses() {
    let profile = parse_intelligence(INTELLIGENCE).unwrap();
    assert_eq!(profile.id, "standard-high");
    assert_eq!(profile.name, "Standard, high");
    assert_eq!(profile.description, "");
    assert_eq!(
        profile.candidates,
        vec![
            Candidate {
                provider: "anthropic".into(),
                model: "claude-sonnet-5-5".into(),
                reasoning: "high".into(),
                harnesses: vec!["claude".into()],
            },
            Candidate {
                provider: "openai".into(),
                model: "gpt-6".into(),
                reasoning: "medium".into(),
                harnesses: vec!["codex".into()],
            },
            Candidate {
                provider: "openai".into(),
                model: "gpt-6".into(),
                reasoning: "default".into(),
                harnesses: vec!["codex".into(), "opencode".into()],
            },
        ]
    );
    assert_eq!(
        profile.candidates[0].derived_id(),
        "anthropic:claude-sonnet-5-5:high"
    );
    assert_eq!(
        profile.selection,
        SelectionOverrides {
            max_attempts: Some(2),
            fallback_on: Some(vec![
                FallbackReason::Unavailable,
                FallbackReason::Unsupported
            ]),
            observation_max_age_ms: Some(30_000),
            allow_unknown_availability: Some(false),
            unknown_cost: Some(UnknownCostPolicy::Deny),
            require_complete_accounting: Some(true),
            require_hard_cost_enforcement: Some(true),
            max_cost_microunits: Some(Some(5_000_000)),
            currency: Some(Some("USD".into())),
            max_uncharged_continuations: Some(1),
        }
    );
    defaults().with(&profile.selection).check().unwrap();
    // The policy a launch pins carries the retry budget, and Settings stored
    // before the budget keys existed read as the initial values.
    let policy = defaults().with(&profile.selection).policy(1, Vec::new());
    assert_eq!(policy.max_uncharged_continuations, Some(1));
    let mut stored = serde_json::to_value(defaults()).unwrap();
    for key in [
        "max_uncharged_continuations",
        "max_wake_attempts",
        "max_wake_lineage_elapsed_ms",
        "max_never_granted_offers",
    ] {
        stored.as_object_mut().unwrap().remove(key).unwrap();
    }
    let read: SelectionDefaults = serde_json::from_value(stored).unwrap();
    assert_eq!(read, defaults());
}
const REMOVED: [&str; 2] = ["max_lineage_elapsed_ms", "max_lineage_sandbox_ms"];
#[test]
fn the_lineage_time_budgets_are_gone_from_defaults_documents_and_policies() {
    let json = serde_json::to_string(&defaults()).unwrap();
    let policy = serde_json::to_string(&defaults().policy(1, Vec::new())).unwrap();
    for key in REMOVED {
        assert!(!json.contains(key) && !policy.contains(key), "{key}");
        // A save that states one is refused as unknown, as is a profile document.
        let mut stated = serde_json::to_value(defaults()).unwrap();
        stated[key] = 1.into();
        assert!(serde_json::from_value::<SelectionDefaults>(stated).is_err());
        let source = format!("{INTELLIGENCE}- {key}: 3600000\n");
        assert!(parse_intelligence(&source).is_err(), "{key}");
    }
}
#[test]
fn stored_settings_snapshots_and_policies_with_the_removed_keys_still_read() {
    // Defaults and a profile's overrides as an earlier release stored them.
    let mut stored = serde_json::to_value(defaults()).unwrap();
    stored["max_lineage_elapsed_ms"] = 3_600_000.into();
    stored["max_lineage_sandbox_ms"] = 14_400_000.into();
    drop_removed_keys(&mut stored);
    assert_eq!(
        serde_json::from_value::<SelectionDefaults>(stored).unwrap(),
        defaults()
    );
    let snapshot = compile(&default_documents(), &defaults(), &limits()).unwrap();
    let mut content: serde_json::Value =
        serde_json::from_slice(snapshot.canonical_bytes()).unwrap();
    content["selection_defaults"]["max_lineage_elapsed_ms"] = 3_600_000.into();
    content["selection_defaults"]["max_lineage_sandbox_ms"] = 14_400_000.into();
    for profile in content["intelligence"]
        .as_object_mut()
        .unwrap()
        .values_mut()
    {
        profile["selection"] = serde_json::json!({"max_attempts": 3, "max_lineage_elapsed_ms": 7});
    }
    let bytes = serde_json::to_vec(&content).unwrap();
    let read = Snapshot::from_stored_bytes(&bytes).unwrap();
    assert_eq!(read.selection_defaults(), &defaults());
    // The digest is that of the bytes as stored.
    assert_eq!(read.canonical_bytes(), bytes.as_slice());
    for profile in read.intelligence_profiles() {
        assert_eq!(profile.selection.max_attempts, Some(3));
    }
}
#[test]
fn a_profile_is_its_candidates_and_selection_is_optional() {
    let source = &INTELLIGENCE[..INTELLIGENCE.find("\n## Selection").unwrap()];
    let profile = parse_intelligence(source).unwrap();
    assert_eq!(profile.candidates.len(), 3);
    assert!(profile.selection.is_empty());
    // An empty section and single keys are both fine.
    let empty = parse_intelligence(&format!("{source}\n## Selection\n")).unwrap();
    assert_eq!(empty, profile);
    let one = parse_intelligence(&format!("{source}\n## Selection\n- max_attempts: 5\n")).unwrap();
    assert_eq!(
        one.selection,
        SelectionOverrides {
            max_attempts: Some(5),
            ..Default::default()
        }
    );
    let none = parse_intelligence(&format!(
        "{source}\n## Selection\n- max_cost_microunits: none\n- currency: none\n"
    ))
    .unwrap();
    assert_eq!(none.selection.max_cost_microunits, Some(None));
    assert_eq!(none.selection.currency, Some(None));
    assert_eq!(parse_intelligence(&none.to_markdown()).unwrap(), none);
    // The execution profile owns these two; a profile cannot state them.
    for key in [
        "max_output_tokens: 32000",
        "required_capabilities: [aq.terminal]",
    ] {
        let error = parse_intelligence(&format!("{source}\n## Selection\n- {key}\n"))
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            format!(
                "line 13: unknown `## Selection` key `{}`",
                key.split(':').next().unwrap()
            )
        );
    }
}
#[test]
fn intelligence_document_refusals_name_the_fault() {
    let edit = |from: &str, to: &str| {
        assert!(INTELLIGENCE.contains(from));
        parse_intelligence(&INTELLIGENCE.replacen(from, to, 1))
            .unwrap_err()
            .to_string()
    };
    for (original, replacement, expected) in [
        (
            "| provider | model |",
            "| model | provider |",
            "line 5: `## Candidates` expects a table that starts with `| provider | model | reasoning | harnesses |`",
        ),
        (
            "|---|---|---|---|\n",
            "",
            "line 5: `## Candidates` expects `|---|---|---|---|` under the table header",
        ),
        (
            "| high | claude |",
            "| high |",
            "line 8: a candidate row has four cells, as `| provider | model | reasoning | harnesses |`; found `| anthropic | claude-sonnet-5-5 | high |`",
        ),
        (
            "claude-sonnet-5-5",
            "claude sonnet",
            "line 8: candidate `model` is malformed: `claude sonnet`",
        ),
        (
            "- max_attempts: 2",
            "- attempts: 2",
            "line 13: unknown `## Selection` key `attempts`",
        ),
        (
            "- max_attempts: 2",
            "- max_attempts: two",
            "line 13: `max_attempts` expects a whole number",
        ),
        (
            "[unavailable, unsupported]",
            "[tired]",
            "line 14: `fallback_on` has an unknown value `tired`",
        ),
        (
            "- unknown_cost: deny",
            "unknown_cost: deny",
            "line 17: `## Selection` expects `- key: value` items, found `unknown_cost: deny`",
        ),
        (
            "name: Standard, high",
            "automatic_routing: true",
            "line 3: unknown frontmatter key `automatic_routing`",
        ),
    ] {
        assert_eq!(edit(original, replacement), expected);
    }

    assert_eq!(
        edit("## Selection", "## Policy").split(';').next().unwrap(),
        "line 12: unknown heading `## Policy`"
    );
}
#[test]
fn execution_document_parses() {
    let profile = parse_execution(EXECUTION).unwrap();
    assert_eq!(
        profile,
        Execution {
            id: "sandbox".into(),
            name: "Sandbox".into(),
            description: "A container.".into(),
            harnesses: vec!["claude".into(), "codex".into()],
            environment: env("aq-ms1-docker-local-claude"),
            confinement: Confinement::Container,
            credentials: [
                (
                    "claude".to_string(),
                    "owner-claude-subscription".to_string()
                ),
                ("codex".to_string(), "owner-codex".to_string()),
            ]
            .into(),
            limits: Limits {
                wall_time_ms: 900_000,
                memory_bytes: 4 * 1024 * 1024 * 1024,
                cpu_millis: 2000,
                process_limit: 512,
                startup_context_bytes: 65_536,
                max_output_tokens: 32_000,
            },
        }
    );
}
#[test]
fn execution_document_refusals_name_the_fault() {
    let edit = |from: &str, to: &str| {
        assert!(EXECUTION.contains(from));
        parse_execution(&EXECUTION.replacen(from, to, 1))
            .unwrap_err()
            .to_string()
    };
    for (original, replacement, expected) in [
        (
            "confinement: container",
            "confinement: jail",
            "line 7: `confinement` has an unknown value `jail`",
        ),
        (
            "confinement: container",
            "permissions: [task.read]",
            "line 7: unknown frontmatter key `permissions`",
        ),
        (
            "- cpu_millis: 2000",
            "- cpus: 2",
            "line 16: unknown `## Limits` key `cpus`",
        ),
        (
            "- cpu_millis: 2000\n",
            "",
            "line 13: `## Limits` is missing `cpu_millis`",
        ),
        (
            "- codex: owner-codex",
            "- claude: other",
            "line 11: duplicate `## Credentials` key `claude`",
        ),
    ] {
        assert_eq!(edit(original, replacement), expected);
    }

    assert_eq!(
        parse_execution(&EXECUTION[..EXECUTION.find("## Limits").unwrap()])
            .unwrap_err()
            .to_string(),
        "`## Limits` is missing"
    );
}
#[test]
fn confinement_names_cover_the_consistent_settings() {
    let mut seen = Vec::new();
    for c in Confinement::ALL {
        let worker = c.execution_confinement();
        assert!(worker.consistent());
        assert!(!seen.contains(&worker));
        seen.push(worker);
        let source = EXECUTION.replace(
            "confinement: container",
            &format!("confinement: {}", c.name()),
        );
        assert_eq!(parse_execution(&source).unwrap().confinement, c);
    }
    assert!(Confinement::Native.execution_confinement().is_default());
}
#[test]
fn documents_round_trip() {
    let role = parse_role(ROLE).unwrap();
    assert_eq!(parse_role(&role.to_markdown()).unwrap(), role);
    let mut odd = role.clone();
    odd.name = "\"Quoted\" [name]".into();
    odd.description = " padded: with colon ".into();
    odd.rules_md.clear();
    odd.enabled = false;
    assert_eq!(parse_role(&odd.to_markdown()).unwrap(), odd);

    let profile = parse_intelligence(INTELLIGENCE).unwrap();
    assert_eq!(parse_intelligence(&profile.to_markdown()).unwrap(), profile);
    let mut plain = profile.clone();
    plain.selection.fallback_on = Some(Vec::new());
    plain.selection.max_cost_microunits = None;
    plain.selection.currency = None;
    assert_eq!(parse_intelligence(&plain.to_markdown()).unwrap(), plain);

    let execution = parse_execution(EXECUTION).unwrap();
    assert_eq!(
        parse_execution(&execution.to_markdown()).unwrap(),
        execution
    );

    for d in default_documents() {
        // A shipped default is already in the form the serializer writes.
        let written = match d.kind {
            Kind::Role => parse_role(&d.text).unwrap().to_markdown(),
            Kind::Intelligence => parse_intelligence(&d.text).unwrap().to_markdown(),
            Kind::Execution => parse_execution(&d.text).unwrap().to_markdown(),
        };
        assert_eq!(written, d.text, "{}", d.label);
    }
}
