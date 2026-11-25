use super::*;

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
    assert!(snapshot.role("missing").is_none());
}

#[test]
fn default_intelligence_profiles_pin_their_model_and_reasoning() {
    let snapshot = compile(&default_documents(), &defaults(), &limits()).unwrap();
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
}
#[test]
fn default_execution_profile_pins_the_sandbox_and_limits() {
    let snapshot = compile(&default_documents(), &defaults(), &limits()).unwrap();
    assert_eq!(snapshot.execution_profiles().count(), 1);
    let sandbox = snapshot.execution("sandbox").unwrap();
    assert_eq!(sandbox.harnesses, ["claude"]);
    assert_eq!(sandbox.environment, env("aq-ms1-docker-local-claude"));
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
fn every_harness_has_an_explicit_set_of_reasoning_levels() {
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
}
fn candidate(provider: &str, reasoning: &str, harnesses: &[&str]) -> Candidate {
    Candidate {
        provider: provider.into(),
        model: "m".into(),
        reasoning: reasoning.into(),
        harnesses: harnesses.iter().map(|h| h.to_string()).collect(),
    }
}
fn check_candidates(candidates: Vec<Candidate>) -> Vec<String> {
    let mut s = set();
    s.intelligence[0].candidates = candidates;
    validate(s, &defaults(), &limits())
        .map(|_| Vec::new())
        .unwrap_or_else(|p| p.iter().map(Problem::to_string).collect::<Vec<_>>())
}
#[test]
fn candidates_are_checked_against_their_harness() {
    assert!(check_candidates(vec![candidate("openai", "medium", &["codex"])]).is_empty());
    assert!(check_candidates(vec![candidate("anthropic", "off", &["claude"])]).is_empty());
    let p = "intelligence profile `simple-low`: ";
    let cases = [
        (
            vec![],
            vec![format!("{p}## Candidates: lists no candidate")],
        ),
        (
            vec![candidate("openai", "high", &["claude"])],
            vec![format!(
                "{p}candidate 1: harness `claude` does not run provider `openai`"
            )],
        ),
        (
            vec![candidate("anthropic", "high", &["codex"])],
            vec![format!(
                "{p}candidate 1: harness `codex` does not run provider `anthropic`"
            )],
        ),
        (
            vec![candidate("anthropic", "minimal", &["claude"])],
            vec![format!(
                "{p}candidate 1: harness `claude` does not accept reasoning `minimal`"
            )],
        ),
        (
            vec![candidate("google", "high", &["gemini", "opencode"])],
            vec![format!(
                "{p}candidate 1: harness `gemini` does not accept reasoning `high`"
            )],
        ),
        (
            vec![candidate("openai", "xhigh", &["codex", "opencode"])],
            vec![
                format!("{p}candidate 1: harness `codex` does not accept reasoning `xhigh`"),
                format!("{p}candidate 1: harness `opencode` does not accept reasoning `xhigh`"),
            ],
        ),
    ];
    for (input, expected) in cases {
        assert_eq!(check_candidates(input), expected);
    }

    // gemini takes only its own setting; opencode takes what codex does.

    assert!(
        check_candidates(vec![candidate(
            "google",
            "default",
            &["gemini", "opencode"]
        )])
        .is_empty()
    );
    assert!(check_candidates(vec![candidate("openai", "default", &["codex"])]).is_empty());
    for harness in HARNESSES {
        assert!(harness_supports(harness, "default"), "{harness}");
    }
    assert!(!harness_supports("pi", "default"));
}
#[test]
fn duplicate_or_malformed_candidates_report_each_field() {
    let p = "intelligence profile `simple-low`: ";
    let cases = [
        (
            vec![candidate("anthropic", "high", &["claude", "claude", "pi"])],
            vec![
                format!("{p}candidate 1: lists harness `claude` twice"),
                format!(
                    "{p}candidate 1: unknown harness `pi`; the harnesses are claude, codex, gemini, opencode"
                ),
            ],
        ),
        (
            vec![candidate("anthropic", "high", &[])],
            vec![format!("{p}candidate 1: names no harness")],
        ),
        (
            vec![
                candidate("anthropic", "high", &["claude"]),
                candidate("anthropic", "high", &["claude"]),
            ],
            vec![format!(
                "{p}candidate 2: repeats `m` with the same reasoning"
            )],
        ),
        (
            vec![candidate("an thropic", "high", &["claude"])],
            vec![
                format!("{p}candidate 1: provider `an thropic` is malformed"),
                format!("{p}candidate 1: harness `claude` does not run provider `an thropic`"),
            ],
        ),
    ];
    for (input, expected) in cases {
        assert_eq!(check_candidates(input), expected);
    }
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
    type ExecutionCase = (fn(&mut Execution), Vec<String>);
    let cases: [ExecutionCase; 5] = [
        (
            |e| e.harnesses.push("codex".into()),
            vec![format!(
                "{p}## Credentials: names no credential for harness `codex`"
            )],
        ),
        (
            |e| {
                e.credentials.insert("codex".into(), "x".into());
            },
            vec![format!(
                "{p}## Credentials: `codex` is not one of this profile's harnesses"
            )],
        ),
        (
            |e| e.harnesses = vec!["docker".into()],
            vec![
                format!(
                    "{p}harnesses: unknown harness `docker`; the harnesses are claude, codex, gemini, opencode"
                ),
                format!("{p}## Credentials: names no credential for harness `docker`"),
                format!("{p}## Credentials: `claude` is not one of this profile's harnesses"),
            ],
        ),
        (
            |e| e.environment = env(""),
            vec![format!("{p}environment: must be one non-empty line")],
        ),
        (
            |e| e.limits.startup_context_bytes = 1_048_577,
            vec![format!(
                "{p}## Limits: `startup_context_bytes` is over the renderer's bound of 1048576 bytes"
            )],
        ),
    ];
    for (input, expected) in cases {
        assert_eq!(check(input), expected);
    }
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
    let [parse, validation] = found.as_slice() else {
        panic!("expected both parse and validation problems: {found:#?}");
    };
    assert!(
        parse.starts_with(
            "role document `roles/developer.md`: line 21: unknown heading `## Rulez`;"
        ),
        "{found:#?}"
    );
    assert_eq!(
        validation,
        "role `reviewer`: execution: no execution profile `nowhere` exists"
    );
}
