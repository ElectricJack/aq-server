use super::*;

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
    assert_eq!(p.observation_max_age_ms, 30_000);
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

    assert_native_launch(&snapshot);

    assert_changed_defaults(&snapshot, &extra, custom);
}
fn assert_native_launch(snapshot: &Snapshot) {
    let launch = snapshot.resolve("local", "custom").unwrap();
    let p = &launch.selection_policy;
    assert_eq!((p.max_attempts, p.observation_max_age_ms), (5, 30_000));
    assert_eq!(p.fallback_on, [FallbackReason::Unavailable]);
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
}
fn assert_changed_defaults(snapshot: &Snapshot, extra: &[(Kind, &str)], custom: &str) {
    // Other defaults: the same documents resolve differently and get another digest.
    let other = SelectionDefaults {
        max_attempts: 5,
        observation_max_age_ms: 60_000,
        max_cost_microunits: Some(7),
        currency: Some("USD".into()),
        ..defaults()
    };
    let moved = compiled(&other, extra);
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
