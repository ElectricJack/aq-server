use super::*;
use crate::{FallbackReason, UnknownCostPolicy};
use aq_session_api::Permission;
use uuid::Uuid;

const ROLE: &str = "---
id: developer
name: Developer
description: Makes the change a task asks for and reports evidence.
execution: sandbox
operations: [task.read, knowledge.read, source.write]
enabled: true
---
## Role
You implement the change your task describes.

```md
## Not a heading
```

## Rules
- Change only what the task asks for.
";
const INTELLIGENCE: &str = "---
id: standard-high
name: Standard, high
---
## Candidates
| provider | model | reasoning | harnesses |
|---|---|---|---|
| anthropic | claude-sonnet-5-5 | high | claude |
| openai | gpt-6 | medium | codex |
| openai | gpt-6 | default | codex, opencode |

## Selection
- max_attempts: 2
- fallback_on: [unavailable, unsupported]
- max_lineage_elapsed_ms: 3600000
- observation_max_age_ms: 30000
- allow_unknown_availability: false
- unknown_cost: deny
- require_complete_accounting: true
- require_hard_cost_enforcement: true
- max_cost_microunits: 5000000
- currency: USD
";
const EXECUTION: &str = "---
id: sandbox
name: Sandbox
description: A container.
harnesses: [claude, codex]
environment: aq-ms1-docker-local-claude
confinement: container
---
## Credentials
- claude: owner-claude-subscription
- codex: owner-codex

## Limits
- wall_time_ms: 900000
- memory_bytes: 4294967296
- cpu_millis: 2000
- process_limit: 512
- startup_context_bytes: 65536
- max_output_tokens: 32000
";

fn role_error(source: &str) -> String {
    parse_role(source).unwrap_err().to_string()
}
fn limits() -> DocumentLimits {
    DocumentLimits::default()
}
fn defaults() -> SelectionDefaults {
    SelectionDefaults::default()
}
/// The shipped defaults, parsed.
fn set() -> DocumentSet {
    let mut set = DocumentSet::default();
    for d in default_documents() {
        match d.kind {
            Kind::Role => set.roles.push(parse_role(&d.text).unwrap()),
            Kind::Intelligence => set.intelligence.push(parse_intelligence(&d.text).unwrap()),
            Kind::Execution => set.execution.push(parse_execution(&d.text).unwrap()),
        }
    }
    set
}
fn problems(set: DocumentSet) -> Vec<String> {
    validate(set, &defaults(), &limits())
        .unwrap_err()
        .iter()
        .map(Problem::to_string)
        .collect()
}
#[track_caller]
fn assert_has(problems: &[String], expected: &str) {
    assert!(
        problems.iter().any(|p| p == expected),
        "missing `{expected}` in {problems:#?}"
    );
}

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
#[test]
fn role_document_refusals_name_the_fault() {
    let edit = |from: &str, to: &str| {
        assert!(ROLE.contains(from));
        role_error(&ROLE.replacen(from, to, 1))
    };
    assert_eq!(
        edit("## Rules", "## Rule"),
        "line 16: unknown heading `## Rule`; this document takes `## Role` and `## Rules`"
    );
    assert_eq!(
        edit("---\n## Role", "---\nIntro.\n## Role"),
        "line 9: text before the first heading: `Intro.`"
    );
    assert_eq!(
        edit("enabled: true", "model: opus"),
        "line 7: unknown frontmatter key `model`"
    );
    assert_eq!(
        edit("enabled: true", "id: other"),
        "line 7: duplicate frontmatter key `id`"
    );
    assert_eq!(
        edit("execution: sandbox\n", ""),
        "line 1: frontmatter is missing `execution`"
    );
    assert_eq!(
        edit("## Role", "## Rules"),
        "line 16: duplicate heading `## Rules`"
    );
    assert_eq!(
        role_error(&ROLE[..ROLE.find("## Role").unwrap()]),
        "`## Role` is missing"
    );
    assert_eq!(
        edit(
            "You implement the change your task describes.\n\n```md\n## Not a heading\n```\n",
            ""
        ),
        "line 9: `## Role` is empty"
    );
    assert_eq!(
        edit("enabled: true", "enabled: yes"),
        "line 7: `enabled` expects `true` or `false`"
    );
    assert_eq!(
        edit("[task.read, knowledge.read, source.write]", "task.read"),
        "line 6: `operations` expects a list such as `[a, b]`"
    );
    assert_eq!(
        edit("[task.read,", "[task read,"),
        "line 6: `operations` has a malformed list item `task read`"
    );
    assert_eq!(edit("name: Developer", "name:"), "line 3: `name` is empty");
    assert_eq!(
        edit("name: Developer", "name: [Developer]"),
        "line 3: `name` expects text, not a list; quote text that starts with `[`"
    );
    assert_eq!(
        edit("name: Developer", "name"),
        "line 3: frontmatter expects `key: value`, found `name`"
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
            max_lineage_elapsed_ms: Some(3_600_000),
            observation_max_age_ms: Some(30_000),
            allow_unknown_availability: Some(false),
            unknown_cost: Some(UnknownCostPolicy::Deny),
            require_complete_accounting: Some(true),
            require_hard_cost_enforcement: Some(true),
            max_cost_microunits: Some(Some(5_000_000)),
            currency: Some(Some("USD".into())),
        }
    );
    defaults().with(&profile.selection).check().unwrap();
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
    assert_eq!(
        edit("| provider | model |", "| model | provider |"),
        "line 5: `## Candidates` expects a table that starts with `| provider | model | reasoning | harnesses |`"
    );
    assert_eq!(
        edit("|---|---|---|---|\n", ""),
        "line 5: `## Candidates` expects `|---|---|---|---|` under the table header"
    );
    assert_eq!(
        edit("| high | claude |", "| high |"),
        "line 8: a candidate row has four cells, as `| provider | model | reasoning | harnesses |`; found `| anthropic | claude-sonnet-5-5 | high |`"
    );
    assert_eq!(
        edit("claude-sonnet-5-5", "claude sonnet"),
        "line 8: candidate `model` is malformed: `claude sonnet`"
    );
    assert_eq!(
        edit("- max_attempts: 2", "- attempts: 2"),
        "line 13: unknown `## Selection` key `attempts`"
    );
    assert_eq!(
        edit("- max_attempts: 2", "- max_attempts: two"),
        "line 13: `max_attempts` expects a whole number"
    );
    assert_eq!(
        edit("[unavailable, unsupported]", "[tired]"),
        "line 14: `fallback_on` has an unknown value `tired`"
    );
    assert_eq!(
        edit("- unknown_cost: deny", "unknown_cost: deny"),
        "line 18: `## Selection` expects `- key: value` items, found `unknown_cost: deny`"
    );
    assert_eq!(
        edit("## Selection", "## Policy").split(';').next().unwrap(),
        "line 12: unknown heading `## Policy`"
    );
    assert_eq!(
        edit("name: Standard, high", "automatic_routing: true"),
        "line 3: unknown frontmatter key `automatic_routing`"
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
            environment: "aq-ms1-docker-local-claude".into(),
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
    assert_eq!(
        edit("confinement: container", "confinement: jail"),
        "line 7: `confinement` has an unknown value `jail`"
    );
    assert_eq!(
        edit("confinement: container", "permissions: [task.read]"),
        "line 7: unknown frontmatter key `permissions`"
    );
    assert_eq!(
        edit("- cpu_millis: 2000", "- cpus: 2"),
        "line 16: unknown `## Limits` key `cpus`"
    );
    assert_eq!(
        edit("- cpu_millis: 2000\n", ""),
        "line 13: `## Limits` is missing `cpu_millis`"
    );
    assert_eq!(
        edit("- codex: owner-codex", "- claude: other"),
        "line 11: duplicate `## Credentials` key `claude`"
    );
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

#[test]
fn defaults_compile_into_a_snapshot() {
    let snapshot = compile(&default_documents(), &defaults(), &limits()).unwrap();
    assert_eq!(
        snapshot.roles().map(|r| r.id.as_str()).collect::<Vec<_>>(),
        ["developer", "reviewer"]
    );
    let developer = snapshot.role("developer").unwrap();
    assert_eq!(developer.execution, "sandbox");
    assert_eq!(
        developer.operations,
        ["task.read", "knowledge.read", "source.write"]
    );
    assert!(snapshot.role("reviewer").unwrap().enabled);
    let levels = [
        ("simple-low", "claude-haiku-5-5", "medium"),
        ("simple-high", "claude-haiku-5-5", "high"),
        ("standard-low", "claude-sonnet-5-5", "low"),
        ("standard-medium", "claude-sonnet-5-5", "medium"),
        ("standard-high", "claude-sonnet-5-5", "high"),
        ("complex-low", "claude-opus-5-5", "low"),
        ("complex-high", "claude-opus-5-5", "high"),
        ("deep-low", "claude-fable-5-2", "low"),
        ("deep-high", "claude-fable-5-2", "high"),
    ];
    assert_eq!(snapshot.intelligence_profiles().count(), levels.len());
    for (id, model, reasoning) in levels {
        let first = &snapshot.intelligence(id).unwrap().candidates[0];
        assert_eq!(
            (
                first.provider.as_str(),
                first.model.as_str(),
                first.reasoning.as_str()
            ),
            ("anthropic", model, reasoning),
            "{id}"
        );
        assert_eq!(first.harnesses, ["claude"]);
    }
    assert_eq!(snapshot.execution_profiles().count(), 1);
    let sandbox = snapshot.execution("sandbox").unwrap();
    assert_eq!(sandbox.harnesses, ["claude"]);
    assert_eq!(sandbox.environment, "aq-ms1-docker-local-claude");
    assert_eq!(sandbox.confinement, Confinement::Container);
    assert_eq!(sandbox.credentials["claude"], "owner-claude-subscription");
    assert_eq!(
        sandbox.limits,
        Limits {
            wall_time_ms: 900_000,
            memory_bytes: 4 << 30,
            cpu_millis: 2000,
            process_limit: 512,
            startup_context_bytes: 65_536,
            max_output_tokens: 32_000,
        }
    );
    assert!(snapshot.role("missing").is_none());
}

#[test]
fn validation_reports_every_problem_together() {
    let mut s = set();
    s.roles[0].id = "Developer".into();
    s.roles[0].execution = "missing".into();
    s.roles[0].operations = vec![
        "task.read".into(),
        "task.read".into(),
        "project.admin".into(),
        "task.fly".into(),
    ];
    s.roles[0].name = " ".into();
    s.roles[0].description = "two\nlines".into();
    s.roles[0].role_md = "Text.\n## Extra\nMore.".into();
    s.intelligence[0].candidates[0].reasoning = "ultra".into();
    s.execution[0].limits.cpu_millis = 0;
    let found = problems(s);
    for expected in [
        "role `Developer`: id: must be a lowercase letter followed by up to 62 lowercase letters, digits or `-`",
        "role `Developer`: name: must be one non-empty line",
        "role `Developer`: description: must be one line",
        "role `Developer`: execution: no execution profile `missing` exists",
        "role `Developer`: operations: lists `task.read` twice",
        "role `Developer`: operations: `project.admin` is not an AQ operation a role can hold",
        "role `Developer`: operations: `task.fly` is not an AQ operation a role can hold",
        "role `Developer`: ## Role: contains a `## ` heading line; use `###` inside a section",
        "intelligence profile `simple-low`: candidate 1: harness `claude` does not accept reasoning `ultra`",
        "execution profile `sandbox`: ## Limits: `cpu_millis` must be a positive number",
    ] {
        assert_has(&found, expected);
    }
    assert_eq!(found.len(), 10, "{found:#?}");
}
#[test]
fn ids_follow_the_pattern() {
    for (id, ok) in [
        ("a", true),
        ("a1-b", true),
        (&"a".repeat(63), true),
        (&"a".repeat(64), false),
        ("", false),
        ("1a", false),
        ("-a", false),
        ("a_b", false),
        ("a@1", false),
        ("A", false),
    ] {
        let mut s = set();
        s.intelligence[0].id = id.to_string();
        assert_eq!(validate(s, &defaults(), &limits()).is_ok(), ok, "{id}");
    }
}
#[test]
fn duplicate_ids_are_refused_within_a_kind_only() {
    let mut s = set();
    let copy = s.roles[0].clone();
    s.roles.push(copy);
    assert_has(
        &problems(s),
        "role `developer`: id: is used by more than one document of this kind",
    );
    // The same id in two kinds names two different things.
    let mut s = set();
    s.intelligence[0].id = "sandbox".into();
    s.roles[1].id = "sandbox".into();
    validate(s, &defaults(), &limits()).unwrap();
}
#[test]
fn an_enabled_role_is_required() {
    let mut s = set();
    s.roles.iter_mut().for_each(|r| r.enabled = false);
    assert_eq!(problems(s), ["all documents: hold no enabled role"]);
    let mut s = set();
    s.roles[0].enabled = false;
    validate(s, &defaults(), &limits()).unwrap();
}
#[test]
fn candidates_are_checked_against_their_harness() {
    let candidate = |provider: &str, reasoning: &str, harnesses: &[&str]| Candidate {
        provider: provider.into(),
        model: "m".into(),
        reasoning: reasoning.into(),
        harnesses: harnesses.iter().map(|h| h.to_string()).collect(),
    };
    let check = |candidates: Vec<Candidate>| {
        let mut s = set();
        s.intelligence[0].candidates = candidates;
        validate(s, &defaults(), &limits())
            .map(|_| Vec::new())
            .unwrap_or_else(|p| p.iter().map(Problem::to_string).collect::<Vec<_>>())
    };
    assert!(check(vec![candidate("openai", "medium", &["codex"])]).is_empty());
    assert!(check(vec![candidate("anthropic", "off", &["claude"])]).is_empty());
    let p = "intelligence profile `simple-low`: ";
    assert_eq!(
        check(vec![]),
        [format!("{p}## Candidates: lists no candidate")]
    );
    assert_eq!(
        check(vec![candidate("openai", "high", &["claude"])]),
        [format!(
            "{p}candidate 1: harness `claude` does not run provider `openai`"
        )]
    );
    assert_eq!(
        check(vec![candidate("anthropic", "high", &["codex"])]),
        [format!(
            "{p}candidate 1: harness `codex` does not run provider `anthropic`"
        )]
    );
    assert_eq!(
        check(vec![candidate("anthropic", "minimal", &["claude"])]),
        [format!(
            "{p}candidate 1: harness `claude` does not accept reasoning `minimal`"
        )]
    );
    // gemini takes only its own setting; opencode takes what codex does.
    assert_eq!(
        check(vec![candidate("google", "high", &["gemini", "opencode"])]),
        [format!(
            "{p}candidate 1: harness `gemini` does not accept reasoning `high`"
        )]
    );
    assert_eq!(
        check(vec![candidate("openai", "xhigh", &["codex", "opencode"])]),
        [
            format!("{p}candidate 1: harness `codex` does not accept reasoning `xhigh`"),
            format!("{p}candidate 1: harness `opencode` does not accept reasoning `xhigh`"),
        ]
    );
    // The whole table, which mirrors the worker's: every other value is refused.
    let all = [
        "default", "off", "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
        "Default", "",
    ];
    for (harness, accepted) in [
        (
            "claude",
            &["default", "off", "low", "medium", "high", "xhigh", "max"][..],
        ),
        ("codex", &["default", "low", "medium", "high"][..]),
        ("opencode", &["default", "low", "medium", "high"][..]),
        ("gemini", &["default"][..]),
    ] {
        for value in all {
            assert_eq!(
                harness_supports(harness, value),
                accepted.contains(&value),
                "{harness} {value}"
            );
        }
    }
    assert!(
        check(vec![candidate(
            "google",
            "default",
            &["gemini", "opencode"]
        )])
        .is_empty()
    );
    assert!(check(vec![candidate("openai", "default", &["codex"])]).is_empty());
    for harness in HARNESSES {
        assert!(harness_supports(harness, "default"), "{harness}");
    }
    assert!(!harness_supports("pi", "default"));
    assert_eq!(
        check(vec![candidate(
            "anthropic",
            "high",
            &["claude", "claude", "pi"]
        )]),
        [
            format!("{p}candidate 1: lists harness `claude` twice"),
            format!(
                "{p}candidate 1: unknown harness `pi`; the harnesses are claude, codex, gemini, opencode"
            ),
        ]
    );
    assert_eq!(
        check(vec![candidate("anthropic", "high", &[])]),
        [format!("{p}candidate 1: names no harness")]
    );
    assert_eq!(
        check(vec![
            candidate("anthropic", "high", &["claude"]),
            candidate("anthropic", "high", &["claude"])
        ]),
        [format!(
            "{p}candidate 2: repeats `m` with the same reasoning"
        )]
    );
    assert_eq!(
        check(vec![candidate("an thropic", "high", &["claude"])]),
        [
            format!("{p}candidate 1: provider `an thropic` is malformed"),
            format!("{p}candidate 1: harness `claude` does not run provider `an thropic`"),
        ]
    );
}
#[test]
fn selection_policy_rules_still_apply() {
    let mut s = set();
    s.intelligence[0].selection.max_attempts = Some(0);
    assert_eq!(
        problems(s),
        ["intelligence profile `simple-low`: ## Selection: selection policy bounds"]
    );
    // A cost limit and its currency go together, after the defaults apply.
    let mut s = set();
    s.intelligence[0].selection.max_cost_microunits = Some(Some(5));
    assert_eq!(
        problems(s),
        ["intelligence profile `simple-low`: ## Selection: selection cost basis"]
    );
    // Faulty defaults are reported once, not once per profile.
    let faulty = SelectionDefaults {
        max_attempts: 0,
        ..defaults()
    };
    let found: Vec<_> = validate(set(), &faulty, &limits())
        .unwrap_err()
        .iter()
        .map(Problem::to_string)
        .collect();
    assert_eq!(found, ["selection defaults: selection policy bounds"]);
}
#[test]
fn execution_profile_rules() {
    let check = |edit: fn(&mut Execution)| {
        let mut s = set();
        edit(&mut s.execution[0]);
        problems(s)
    };
    let p = "execution profile `sandbox`: ";
    assert_eq!(
        check(|e| e.harnesses.push("codex".into())),
        [format!(
            "{p}## Credentials: names no credential for harness `codex`"
        )]
    );
    assert_eq!(
        check(|e| {
            e.credentials.insert("codex".into(), "x".into());
        }),
        [format!(
            "{p}## Credentials: `codex` is not one of this profile's harnesses"
        )]
    );
    assert_eq!(
        check(|e| e.harnesses = vec!["docker".into()]),
        [
            format!(
                "{p}harnesses: unknown harness `docker`; the harnesses are claude, codex, gemini, opencode"
            ),
            format!("{p}## Credentials: names no credential for harness `docker`"),
            format!("{p}## Credentials: `claude` is not one of this profile's harnesses"),
        ]
    );
    assert_eq!(
        check(|e| e.environment = String::new()),
        [format!("{p}environment: must be one non-empty line")]
    );
    assert_eq!(
        check(|e| e.limits.startup_context_bytes = 1_048_577),
        [format!(
            "{p}## Limits: `startup_context_bytes` is over the renderer's bound of 1048576 bytes"
        )]
    );
}
#[test]
fn byte_limits_come_from_the_caller() {
    let documents = default_documents();
    let longest = documents.iter().map(|d| d.text.len()).max().unwrap();
    let total: usize = documents.iter().map(|d| d.text.len()).sum();
    let exact = DocumentLimits {
        max_document_bytes: longest,
        max_set_bytes: total,
        max_documents_per_kind: 9,
        ..limits()
    };
    compile(&documents, &defaults(), &exact).unwrap();
    validate(set(), &defaults(), &exact).unwrap();

    let tight = DocumentLimits {
        max_document_bytes: longest - 1,
        max_set_bytes: total - 1,
        max_documents_per_kind: 8,
        max_name_bytes: 3,
        max_description_bytes: 3,
    };
    let from_source: Vec<_> = compile(&documents, &defaults(), &tight)
        .unwrap_err()
        .iter()
        .map(Problem::to_string)
        .collect();
    let over = documents.iter().find(|d| d.text.len() == longest).unwrap();
    assert_has(
        &from_source,
        &format!(
            "{} document `{}`: is {longest} bytes, over the limit of {} bytes",
            over.kind.label(),
            over.label,
            longest - 1
        ),
    );
    let set_problem = format!(
        "all documents: are {total} bytes together, over the limit of {} bytes",
        total - 1
    );
    assert_has(&from_source, &set_problem);
    assert_has(
        &from_source,
        "all documents: hold 9 intelligence profile documents, over the limit of 8",
    );
    assert_has(
        &from_source,
        "role `developer`: name: is over the limit of 3 bytes",
    );
    assert_has(
        &from_source,
        "execution profile `sandbox`: description: is over the limit of 3 bytes",
    );
    let from_types = problems_with(set(), &tight);
    assert_has(&from_types, &set_problem);
    assert!(from_types.iter().any(|p| p.ends_with(&format!(
        ": is {longest} bytes, over the limit of {} bytes",
        longest - 1
    ))));
}
fn problems_with(set: DocumentSet, limits: &DocumentLimits) -> Vec<String> {
    validate(set, &defaults(), limits)
        .unwrap_err()
        .iter()
        .map(Problem::to_string)
        .collect()
}
#[test]
fn compile_collects_parse_and_validation_problems() {
    let mut documents = default_documents();
    documents[0].text = documents[0].text.replace("## Rules", "## Rulez");
    documents[1].text = documents[1]
        .text
        .replace("execution: sandbox", "execution: nowhere");
    let found: Vec<_> = compile(&documents, &defaults(), &limits())
        .unwrap_err()
        .iter()
        .map(Problem::to_string)
        .collect();
    assert_eq!(found.len(), 2, "{found:#?}");
    assert!(
        found[0].starts_with(
            "role document `roles/developer.md`: line 21: unknown heading `## Rulez`;"
        ),
        "{found:#?}"
    );
    assert_eq!(
        found[1],
        "role `reviewer`: execution: no execution profile `nowhere` exists"
    );
}

#[test]
fn snapshot_digest_ignores_order_and_insignificant_whitespace() {
    let documents = default_documents();
    let snapshot = compile(&documents, &defaults(), &limits()).unwrap();
    assert_eq!(snapshot.digest().len(), 64);
    assert_eq!(
        snapshot.digest(),
        aq_session_protocol::Digest::of(snapshot.canonical_bytes()).as_str()
    );
    let mut reordered = documents.clone();
    reordered.reverse();
    assert_eq!(
        compile(&reordered, &defaults(), &limits()).unwrap(),
        snapshot
    );

    let spaced: Vec<_> = documents
        .iter()
        .map(|d| SourceDocument {
            kind: d.kind,
            label: format!("other-{}", d.label),
            text: d
                .text
                .replace("operations: [task.read, ", "operations:   [ task.read ,")
                .replace("---\n## ", "---\n\n\n## ")
                .replace("\n- ", "\n\n  - ")
                .replace("| anthropic |", "|anthropic   |")
                .replace("|---|---|---|---|", "| --- | :-- | --: | - |")
                .replace("name: ", "name:    ")
                .replace('\n', "\r\n")
                + "\r\n\r\n",
        })
        .collect();
    // Indenting list items also touched the role's `## Rules` prose, which is content.
    let digest_of = |sources: &[SourceDocument]| {
        compile(sources, &defaults(), &limits())
            .unwrap()
            .digest()
            .to_string()
    };
    let profiles_only = |sources: &[SourceDocument]| -> Vec<SourceDocument> {
        sources
            .iter()
            .map(|d| match d.kind {
                Kind::Role => documents
                    .iter()
                    .find(|o| d.label.ends_with(&o.label))
                    .unwrap()
                    .clone(),
                _ => d.clone(),
            })
            .collect()
    };
    assert_eq!(digest_of(&profiles_only(&spaced)), snapshot.digest());
    let role_spaced: Vec<_> = documents
        .iter()
        .map(|d| SourceDocument {
            text: d
                .text
                .replace("operations: [task.read, ", "operations:   [ task.read ,")
                .replace("---\n## Role\n", "---\n\n## Role   \n\n\n")
                .replace('\n', "\r\n")
                + "\r\n",
            ..d.clone()
        })
        .collect();
    assert_eq!(digest_of(&role_spaced), snapshot.digest());
}
#[test]
fn snapshot_digest_changes_with_any_content() {
    let base = validate(set(), &defaults(), &limits()).unwrap();
    assert_eq!(
        base,
        compile(&default_documents(), &defaults(), &limits()).unwrap()
    );
    let edits: Vec<fn(&mut DocumentSet)> = vec![
        |s| s.roles[0].role_md.push('.'),
        |s| s.roles[0].rules_md.push('.'),
        |s| s.roles[0].name.push('x'),
        |s| s.roles[0].description.push('x'),
        |s| s.roles[1].enabled = false,
        |s| s.roles[0].operations.reverse(),
        |s| s.roles[1].operations.push("wait.register".into()),
        |s| s.intelligence[3].candidates[0].reasoning = "max".into(),
        |s| s.intelligence[3].candidates[0].model.push('x'),
        |s| s.intelligence[3].selection.max_attempts = Some(3),
        |s| s.intelligence[3].id.push('x'),
        |s| s.execution[0].limits.wall_time_ms += 1,
        |s| s.execution[0].confinement = Confinement::ContainerPermitted,
        |s| s.execution[0].environment.push('x'),
        |s| {
            s.execution[0]
                .credentials
                .insert("claude".into(), "other".into());
        },
    ];
    let mut seen = std::collections::BTreeSet::from([base.digest().to_string()]);
    for (index, edit) in edits.iter().enumerate() {
        let mut s = set();
        edit(&mut s);
        let changed = validate(s, &defaults(), &limits()).unwrap();
        assert!(seen.insert(changed.digest().to_string()), "edit {index}");
    }
    // Outer whitespace of a prompt is not content.
    let mut s = set();
    s.roles[0].role_md = format!("\r\n{}\n\n", s.roles[0].role_md);
    assert_eq!(
        validate(s, &defaults(), &limits()).unwrap().digest(),
        base.digest()
    );
}
#[test]
fn snapshot_reads_back_only_from_its_canonical_bytes() {
    let snapshot = compile(&default_documents(), &defaults(), &limits()).unwrap();
    let bytes = snapshot.canonical_bytes();
    assert_eq!(
        Snapshot::from_canonical_bytes(bytes, &limits()).unwrap(),
        snapshot
    );
    let text = std::str::from_utf8(bytes).unwrap();
    // No entry has a version and nothing is named `id@version`.
    assert!(!text.contains('@'));
    for kind in ["roles", "intelligence", "execution"] {
        let value: serde_json::Value = serde_json::from_slice(bytes).unwrap();
        for entry in value[kind].as_object().unwrap().values() {
            assert!(entry.get("version").is_none());
        }
    }
    let refused = |bytes: &str| {
        Snapshot::from_canonical_bytes(bytes.as_bytes(), &limits()).unwrap_err()[0].to_string()
    };
    assert_eq!(
        refused(&format!(" {text}")),
        "snapshot: bytes are not canonical"
    );
    assert_eq!(
        refused(&text.replace("\"schema_version\":1", "\"schema_version\":2")),
        "snapshot: has an unknown schema version"
    );
    assert!(refused("{}").starts_with("snapshot: is not readable: "));
    assert_eq!(
        refused(&text.replace("\"execution\":\"sandbox\"", "\"execution\":\"gone\"")),
        "role `developer`: execution: no execution profile `gone` exists"
    );
}

fn permission(operation: &str, scope: String) -> Permission {
    Permission {
        operation: operation.into(),
        scope_reference: scope,
    }
}
fn pairs(list: &[Permission]) -> Vec<(String, String)> {
    list.iter()
        .map(|p| (p.operation.clone(), p.scope_reference.clone()))
        .collect()
}
#[test]
fn permissions_take_the_scope_of_the_project_or_its_repository() {
    let project = Uuid::from_u128(7);
    let repository = Uuid::from_u128(9);
    // What compose.rs builds today as the installation ceiling for the live case.
    let mut live = ["task.read", "knowledge.read"]
        .map(|operation| permission(operation, format!("project:{project}")))
        .to_vec();
    live.push(permission(
        "source.write",
        format!("repository:{repository}"),
    ));
    let snapshot = compile(&default_documents(), &defaults(), &limits()).unwrap();
    let developer = snapshot.role("developer").unwrap();
    let effective = effective_permissions(developer, project, repository, &live, &live);
    assert_eq!(pairs(&effective), pairs(&live));

    // A reviewer does not write source, whatever the ceilings allow.
    let reviewer = snapshot.role("reviewer").unwrap();
    assert_eq!(
        pairs(&effective_permissions(
            reviewer, project, repository, &live, &live
        )),
        pairs(&live[..2])
    );
    // Both the project's ceilings and the caller must hold the exact permission.
    assert_eq!(
        pairs(&effective_permissions(
            developer,
            project,
            repository,
            &live[1..],
            &live
        )),
        pairs(&live[1..])
    );
    assert_eq!(
        pairs(&effective_permissions(
            developer,
            project,
            repository,
            &live,
            &live[..1]
        )),
        pairs(&live[..1])
    );
    assert!(effective_permissions(developer, project, repository, &live, &[]).is_empty());
    // A ceiling for another project or with the wrong scope kind grants nothing.
    let foreign = vec![
        permission("task.read", format!("project:{}", Uuid::from_u128(8))),
        permission("source.write", format!("project:{project}")),
        permission("knowledge.read", format!("repository:{repository}")),
    ];
    assert!(effective_permissions(developer, project, repository, &foreign, &live).is_empty());

    let mut waiter = developer.clone();
    waiter.operations = vec![
        "wait.register".into(),
        "task.read".into(),
        "wait.register".into(),
        "wait.show".into(),
    ];
    let ceiling = vec![
        permission("task.read", format!("project:{project}")),
        permission("wait.register", "attempt:self".into()),
        // A project-scoped wait is not what a role's wait operation means.
        permission("wait.show", format!("project:{project}")),
    ];
    assert_eq!(
        pairs(&effective_permissions(
            &waiter, project, repository, &ceiling, &ceiling
        )),
        [
            ("wait.register".to_string(), "attempt:self".to_string()),
            ("task.read".to_string(), format!("project:{project}")),
        ]
    );
}
#[test]
fn role_operations_are_the_known_vocabulary() {
    for operation in aq_agent_grants::op::ALL {
        assert!(operation_known(operation));
    }
    assert!(operation_known("source.write"));
    for operation in aq_agent_grants::OWNER_ONLY {
        assert!(!operation_known(operation));
    }
    assert!(!operation_known("source.read") && !operation_known(""));
}

fn startup<'a>(role: &'a Role) -> StartupInput<'a> {
    StartupInput {
        platform: "Platform.",
        role,
        project_notes: Some("Project notes."),
        repository_notes: Some("Repository notes."),
        task: "Task é.",
        task_thread: None,
        reporting: Some("Report."),
    }
}
#[test]
fn startup_text_has_a_fixed_order_and_double_newline_joining() {
    let role = parse_role(ROLE).unwrap();
    let text = assemble_startup(&startup(&role), 65_536).unwrap();
    assert_eq!(
        text.instructions
            .iter()
            .map(|(part, _)| *part)
            .collect::<Vec<_>>(),
        [
            StartupPart::Platform,
            StartupPart::Role,
            StartupPart::Rules,
            StartupPart::ProjectNotes,
            StartupPart::RepositoryNotes,
            StartupPart::Task,
            StartupPart::Reporting,
        ]
    );
    assert_eq!(
        text.rendered,
        "Platform.\n\n## Role\nYou implement the change your task describes.\n\n```md\n## Not a heading\n```\n\n## Rules\n- Change only what the task asks for.\n\nProject notes.\n\nRepository notes.\n\nTask é.\n\nReport."
    );
    // The same join the renderer applies to the instruction list.
    let joined = text
        .instructions
        .iter()
        .map(|(_, t)| t.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    assert_eq!(text.rendered, joined);

    let mut bare = role.clone();
    bare.rules_md.clear();
    let input = StartupInput {
        project_notes: None,
        repository_notes: Some(" \n"),
        ..startup(&bare)
    };
    let text = assemble_startup(&input, 65_536).unwrap();
    assert_eq!(
        text.instructions
            .iter()
            .map(|(part, _)| *part)
            .collect::<Vec<_>>(),
        [
            StartupPart::Platform,
            StartupPart::Role,
            StartupPart::Task,
            StartupPart::Reporting,
        ]
    );
}
#[test]
fn startup_text_over_the_limit_is_refused_not_trimmed() {
    let role = parse_role(ROLE).unwrap();
    let input = startup(&role);
    let bytes = assemble_startup(&input, u64::MAX).unwrap().rendered.len() as u64;
    assert!(
        bytes
            > assemble_startup(&input, u64::MAX)
                .unwrap()
                .rendered
                .chars()
                .count() as u64
    );
    assemble_startup(&input, bytes).unwrap();
    let error = assemble_startup(&input, bytes - 1).unwrap_err();
    assert_eq!(
        error,
        StartupError::OverLimit {
            bytes,
            limit: bytes - 1
        }
    );
    assert_eq!(
        error.to_string(),
        format!(
            "the startup text is {bytes} bytes, over the execution profile's limit of {} bytes",
            bytes - 1
        )
    );
    for (part, input) in [
        (
            StartupPart::Platform,
            StartupInput {
                platform: " ",
                ..input
            },
        ),
        (StartupPart::Task, StartupInput { task: "", ..input }),
        (
            StartupPart::Reporting,
            StartupInput {
                task_thread: None,
                reporting: Some("\n"),
                ..input
            },
        ),
    ] {
        assert_eq!(
            assemble_startup(&input, u64::MAX).unwrap_err(),
            StartupError::Empty(part)
        );
    }
    assert_eq!(
        StartupError::Empty(StartupPart::Task).to_string(),
        "the task is empty"
    );
}

fn compiled(defaults: &SelectionDefaults, extra: &[(Kind, &str)]) -> Snapshot {
    let mut documents = default_documents();
    for (kind, text) in extra {
        documents.push(SourceDocument {
            kind: *kind,
            label: "extra".into(),
            text: text.to_string(),
        });
    }
    compile(&documents, defaults, &limits()).unwrap()
}
#[test]
fn a_launch_resolves_defaults_overrides_and_the_execution_profile() {
    let snapshot = compiled(&defaults(), &[]);
    assert_eq!(snapshot.selection_defaults(), &defaults());
    let launch = snapshot.resolve("developer", "standard-high").unwrap();
    assert_eq!(
        (launch.role.id.as_str(), launch.execution.id.as_str()),
        ("developer", "sandbox")
    );
    assert_eq!(launch.candidates, launch.intelligence.candidates);
    let p = &launch.selection_policy;
    p.validate().unwrap();
    // What the nine level documents each said before they became candidates-only.
    assert_eq!((p.schema_version, p.max_attempts), (1, 2));
    assert_eq!(p.fallback_on.len(), 5);
    assert_eq!(
        (p.max_lineage_elapsed_ms, p.observation_max_age_ms),
        (3_600_000, 30_000)
    );
    assert!(!p.allow_unknown_availability);
    assert_eq!(p.budget.unknown_cost, UnknownCostPolicy::AllowBounded);
    assert!(!p.budget.require_complete_accounting && !p.budget.require_hard_cost_enforcement);
    assert_eq!(
        (p.budget.max_cost_microunits, &p.budget.currency),
        (None, &None)
    );
    // From the execution profile: its output limit and, for a container, five capabilities.
    assert_eq!(p.max_output_tokens, 32_000);
    let ids: Vec<_> = p
        .required_capabilities
        .iter()
        .map(|c| (c.id.as_str(), c.version))
        .collect();
    assert_eq!(
        ids,
        [
            ("aq.profiled-launch", 1),
            ("aq.terminal", 1),
            ("aq.harness.startup-context", 1),
            ("aq.harness.container-confinement", 1),
            ("aq.harness.stop", 1),
        ]
    );

    assert_eq!(
        snapshot
            .resolve("tester", "standard-high")
            .unwrap_err()
            .to_string(),
        "no role `tester` exists"
    );
    assert_eq!(
        snapshot
            .resolve("developer", "router")
            .unwrap_err()
            .to_string(),
        "no intelligence profile `router` exists"
    );
}
#[test]
fn a_profile_overrides_single_keys_and_the_serializer_writes_only_those() {
    let custom = "---\nid: custom\nname: Custom\n---\n## Candidates\n| provider | model | reasoning | harnesses |\n|---|---|---|---|\n| anthropic | m | high | claude |\n| openai | n | high | codex |\n\n## Selection\n- max_attempts: 5\n- observation_max_age_ms: 30000\n- fallback_on: [unavailable]\n";
    let native = EXECUTION
        .replace("id: sandbox", "id: native")
        .replace("confinement: container", "confinement: native")
        .replace("max_output_tokens: 32000", "max_output_tokens: 9000");
    let role = ROLE
        .replace("id: developer", "id: local")
        .replace("execution: sandbox", "execution: native");
    let extra = [
        (Kind::Intelligence, custom),
        (Kind::Execution, native.as_str()),
        (Kind::Role, role.as_str()),
    ];
    let snapshot = compiled(&defaults(), &extra);
    let profile = snapshot.intelligence("custom").unwrap();
    // `observation_max_age_ms: 30000` states the default, so it is not kept.
    assert_eq!(
        profile.selection,
        SelectionOverrides {
            max_attempts: Some(5),
            fallback_on: Some(vec![FallbackReason::Unavailable]),
            ..Default::default()
        }
    );
    let written = profile.to_markdown();
    assert!(written.ends_with("\n## Selection\n- max_attempts: 5\n- fallback_on: [unavailable]\n"));
    assert_eq!(parse_intelligence(&written).unwrap(), *profile);
    // A profile with no differing key is written without the section.
    assert!(
        !snapshot
            .intelligence("simple-low")
            .unwrap()
            .to_markdown()
            .contains("## Selection")
    );

    let launch = snapshot.resolve("local", "custom").unwrap();
    let p = &launch.selection_policy;
    assert_eq!((p.max_attempts, p.observation_max_age_ms), (5, 30_000));
    assert_eq!(p.fallback_on, [FallbackReason::Unavailable]);
    assert_eq!(p.max_lineage_elapsed_ms, 3_600_000);
    // The native profile: its own output limit and no container capability.
    assert_eq!(p.max_output_tokens, 9000);
    let ids: Vec<_> = p
        .required_capabilities
        .iter()
        .map(|c| c.id.as_str())
        .collect();
    assert_eq!(
        ids,
        [
            "aq.profiled-launch",
            "aq.terminal",
            "aq.harness.startup-context",
            "aq.harness.stop"
        ]
    );
    // Both harnesses are the native profile's; under `sandbox` only claude is.
    assert_eq!(launch.candidates.len(), 2);
    let sandboxed = snapshot.resolve("developer", "custom").unwrap();
    assert_eq!(sandboxed.candidates.len(), 1);
    assert_eq!(sandboxed.candidates[0].harnesses, ["claude"]);

    // Other defaults: the same documents resolve differently and get another digest.
    let other = SelectionDefaults {
        max_attempts: 5,
        observation_max_age_ms: 60_000,
        max_cost_microunits: Some(7),
        currency: Some("USD".into()),
        ..defaults()
    };
    let moved = compiled(&other, &extra);
    assert_ne!(moved.digest(), snapshot.digest());
    let profile = moved.intelligence("custom").unwrap();
    assert_eq!(
        profile.selection,
        SelectionOverrides {
            observation_max_age_ms: Some(30_000),
            fallback_on: Some(vec![FallbackReason::Unavailable]),
            ..Default::default()
        }
    );
    let p = moved
        .resolve("developer", "simple-low")
        .unwrap()
        .selection_policy;
    assert_eq!((p.max_attempts, p.observation_max_age_ms), (5, 60_000));
    assert_eq!(p.budget.max_cost_microunits, Some(7));
    assert_eq!(
        Snapshot::from_canonical_bytes(moved.canonical_bytes(), &limits()).unwrap(),
        moved
    );
    // A profile can state "no cost limit" against defaults that have one.
    let free = custom.replace(
        "- max_attempts: 5",
        "- max_cost_microunits: none\n- currency: none",
    );
    let moved = compiled(&other, &[(Kind::Intelligence, free.as_str())]);
    let p = moved
        .resolve("developer", "custom")
        .unwrap()
        .selection_policy;
    assert_eq!(
        (p.budget.max_cost_microunits, p.budget.currency),
        (None, None)
    );
    assert_eq!(
        Snapshot::from_canonical_bytes(moved.canonical_bytes(), &limits()).unwrap(),
        moved
    );
}
#[test]
fn an_execution_profile_bounds_its_output_tokens() {
    let mut s = set();
    s.execution[0].limits.max_output_tokens = 1_048_577;
    assert_eq!(
        problems(s),
        [
            "execution profile `sandbox`: ## Limits: `max_output_tokens` is over the selection policy's bound of 1048576"
        ]
    );
}

/// Every environment has platform text: its own file or the shipped default.
/// An empty one is `StartupError::Empty(Platform)` (above), which the server
/// refuses as `platform_text_missing`; it never launches with none.
#[test]
fn every_environment_has_platform_text() {
    for environment in [
        "aq-ms1-docker-local-claude",
        "an-environment-with-no-file",
        "",
    ] {
        assert!(!platform_text(environment).is_empty(), "{environment}");
    }
}
