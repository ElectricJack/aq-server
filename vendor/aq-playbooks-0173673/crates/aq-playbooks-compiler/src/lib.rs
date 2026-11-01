//! Deterministic compilation accepts an explicit proposal; it never interprets prose.
use aq_playbooks_model::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const COMPILER_VERSION: &str = concat!("aq-playbooks-compiler/", env!("CARGO_PKG_VERSION"));
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Proposal {
    pub playbook: Playbook,
    pub source_map: BTreeMap<String, SourceLocation>,
    pub questions: Vec<String>,
    pub unsupported_features: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompileInput {
    pub source: String,
    pub proposal: Proposal,
    pub registry: Registry,
    pub dependencies: BTreeMap<String, CallableDependency>,
    pub definitions: BTreeMap<String, CapturedDefinition>,
    pub compiler: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Question,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub severity: Severity,
    pub code: String,
    pub path: String,
    pub message: String,
    pub line: Option<u32>,
    pub column: Option<u32>,
}
impl Diagnostic {
    fn error(code: &str, path: &str, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Error,
            code: code.into(),
            path: path.into(),
            message: message.into(),
            line: None,
            column: None,
        }
    }
}
pub fn normalize_source(source: &str) -> String {
    source.replace("\r\n", "\n").replace('\r', "\n")
}
pub fn parse_source_header(source: &str) -> Result<SourceHeader, Diagnostic> {
    let source = normalize_source(source);
    let rest = source.strip_prefix("---aq-playbook\n").ok_or_else(|| {
        Diagnostic::error(
            "source_header",
            "source",
            "source must begin with ---aq-playbook JSON header",
        )
    })?;
    let end = rest
        .find("\n---\n")
        .or_else(|| rest.strip_suffix("\n---").map(|x| x.len()))
        .ok_or_else(|| {
            Diagnostic::error("source_header", "source", "missing closing --- header line")
        })?;
    if end > 65_536 {
        return Err(Diagnostic::error(
            "source_header",
            "source",
            "header exceeds 64 KiB",
        ));
    }
    let header: SourceHeader = serde_json::from_str(&rest[..end])
        .map_err(|e| Diagnostic::error("source_header", "source", e.to_string()))?;
    if header.format_version != FORMAT_VERSION {
        return Err(Diagnostic::error(
            "unsupported_format",
            "source",
            "unsupported source format",
        ));
    }
    Ok(header)
}
fn content(input: &CompileInput, header: SourceHeader) -> ArtifactContent {
    ArtifactContent {
        format_version: FORMAT_VERSION,
        compiler: input.compiler.clone(),
        source: SourceBinding {
            digest: hash_bytes(normalize_source(&input.source).as_bytes()),
            header,
        },
        playbook: input.proposal.playbook.clone(),
        registry: input.registry.clone(),
        dependencies: input.dependencies.clone(),
        definitions: input.definitions.clone(),
        source_map: input.proposal.source_map.clone(),
    }
}
pub fn validate(input: &CompileInput) -> Vec<Diagnostic> {
    let mut issues = vec![];
    let header = match parse_source_header(&input.source) {
        Ok(h) => Some(h),
        Err(e) => {
            issues.push(e);
            None
        }
    };
    for question in &input.proposal.questions {
        issues.push(Diagnostic {
            severity: Severity::Question,
            code: "unresolved_interpretation".into(),
            path: "proposal.questions".into(),
            message: question.clone(),
            line: None,
            column: None,
        });
    }
    for feature in &input.proposal.unsupported_features {
        issues.push(Diagnostic::error(
            "unsupported_feature",
            "proposal.unsupported_features",
            format!("unsupported feature: {feature}"),
        ));
    }
    if input.source.len() > 4_194_304 {
        issues.push(Diagnostic::error(
            "source_limit",
            "source",
            "source exceeds 4 MiB",
        ));
    }
    if input.compiler.is_empty() {
        issues.push(Diagnostic::error(
            "compiler_build",
            "compiler",
            "compiler build identifier required",
        ));
    }
    if let Some(header) = header {
        let c = content(input, header);
        for issue in c.validate_structure() {
            let mut d = Diagnostic::error(&issue.code, &issue.path, issue.message);
            if let Some(loc) = c.source_map.get(&issue.path) {
                d.line = Some(loc.line);
                d.column = Some(loc.column);
            }
            issues.push(d);
        }
    }
    issues
}
pub fn compile(input: &CompileInput) -> Result<Artifact, Vec<Diagnostic>> {
    let diagnostics = validate(input);
    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }
    let header = parse_source_header(&input.source).map_err(|e| vec![e])?;
    Artifact::seal(content(input, header))
        .map_err(|e| vec![Diagnostic::error("artifact", "artifact", e.to_string())])
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Identical,
    Presentation,
    Executable,
    Interface,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SemanticDiff {
    pub kind: ChangeKind,
    pub source_changed: bool,
    pub changed_steps: Vec<String>,
    pub changed_interfaces: Vec<String>,
    pub changed_commands: Vec<String>,
    pub changed_events: Vec<String>,
    pub changed_dependencies: Vec<String>,
    pub capabilities_added: BTreeSet<String>,
    pub capabilities_removed: BTreeSet<String>,
}
fn changed<T: PartialEq>(a: &BTreeMap<String, T>, b: &BTreeMap<String, T>) -> Vec<String> {
    a.keys()
        .chain(b.keys())
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|k| a.get(k) != b.get(k))
        .collect()
}
pub fn semantic_diff(before: &Artifact, after: &Artifact) -> Result<SemanticDiff, ModelError> {
    before.verify()?;
    after.verify()?;
    let a = &before.content;
    let b = &after.content;
    let changed_interfaces = changed(&a.playbook.entries, &b.playbook.entries);
    Ok(SemanticDiff {
        kind: if before.digest == after.digest {
            ChangeKind::Identical
        } else if before.executable_fingerprint == after.executable_fingerprint {
            ChangeKind::Presentation
        } else if !changed_interfaces.is_empty() {
            ChangeKind::Interface
        } else {
            ChangeKind::Executable
        },
        source_changed: a.source != b.source,
        changed_steps: changed(&a.playbook.steps, &b.playbook.steps),
        changed_interfaces,
        changed_commands: changed(&a.registry.commands, &b.registry.commands),
        changed_events: changed(&a.registry.events, &b.registry.events),
        changed_dependencies: changed(&a.dependencies, &b.dependencies),
        capabilities_added: b
            .playbook
            .required_capabilities
            .difference(&a.playbook.required_capabilities)
            .cloned()
            .collect(),
        capabilities_removed: a
            .playbook
            .required_capabilities
            .difference(&b.playbook.required_capabilities)
            .cloned()
            .collect(),
    })
}
pub fn diff(before: &Artifact, after: &Artifact) -> Result<SemanticDiff, ModelError> {
    semantic_diff(before, after)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    fn fixture() -> CompileInput {
        let header = SourceHeader {
            format_version: 1,
            id: "example".into(),
            scope: "project".into(),
            interfaces: BTreeSet::from(["main".into()]),
            definitions: BTreeSet::new(),
        };
        CompileInput {
            source: format!(
                "---aq-playbook\n{}\n---\n# Example policy\n",
                serde_json::to_string(&header).unwrap()
            ),
            proposal: Proposal {
                playbook: Playbook {
                    id: "example".into(),
                    purpose: "A bounded example".into(),
                    scope: "project".into(),
                    entries: BTreeMap::from([(
                        "main".into(),
                        EntryPoint {
                            input: Schema::Null,
                            outcomes: BTreeMap::from([("done".into(), Schema::Null)]),
                            start: "finish".into(),
                        },
                    )]),
                    steps: BTreeMap::from([(
                        "finish".into(),
                        Step {
                            op: Operation::Return {
                                outcome: "done".into(),
                                output: Expr::literal(Value::Null),
                            },
                            edges: BTreeMap::new(),
                            on_error: None,
                        },
                    )]),
                    triggers: vec![],
                    limits: Limits::default(),
                    required_capabilities: BTreeSet::new(),
                },
                source_map: BTreeMap::new(),
                questions: vec![],
                unsupported_features: vec![],
            },
            registry: Registry::default(),
            dependencies: BTreeMap::new(),
            definitions: BTreeMap::new(),
            compiler: COMPILER_VERSION.into(),
        }
    }
    fn command() -> CommandContract {
        CommandContract {
            name: "test.observe".into(),
            version: "1.0.0".into(),
            input: Schema::Null,
            outcomes: BTreeMap::from([
                ("ok".into(), Schema::Null),
                ("unavailable".into(), Schema::Null),
            ]),
            capabilities: BTreeSet::new(),
            scopes: BTreeSet::from(["project".into()]),
            effect: Effect::Observation,
            recovery: Recovery::Observation,
            asynchronous: None,
            cancellation: false,
            public_result_fields: BTreeSet::new(),
            sensitive_input_fields: BTreeSet::new(),
            sensitive_output_fields: BTreeSet::new(),
            max_payload_bytes: 1024,
            deadline_ms: 1000,
        }
    }
    fn invocation_fixture() -> CompileInput {
        let mut f = fixture();
        let c = command();
        f.proposal.playbook.entries.get_mut("main").unwrap().start = "observe".into();
        f.proposal.playbook.steps.insert(
            "observe".into(),
            Step {
                op: Operation::Invoke {
                    command: ContractRef {
                        name: c.name.clone(),
                        fingerprint: c.fingerprint().unwrap(),
                    },
                    input: Expr::literal(Value::Null),
                    bind: "observed".into(),
                    retry: None,
                },
                edges: BTreeMap::from([
                    ("ok".into(), "finish".into()),
                    ("unavailable".into(), "finish".into()),
                ]),
                on_error: Some("finish".into()),
            },
        );
        f.registry.commands.insert(c.name.clone(), c);
        f
    }
    fn codes(f: &CompileInput) -> BTreeSet<String> {
        validate(f).into_iter().map(|d| d.code).collect()
    }
    #[test]
    fn t1_reproducible_artifact_and_narrow_source_normalization() {
        let f = invocation_fixture();
        let a = compile(&f).unwrap();
        let b = compile(&f).unwrap();
        assert_eq!(canonical_bytes(&a).unwrap(), canonical_bytes(&b).unwrap());
        assert!(a.verify().is_ok());
        let mut crlf = f.clone();
        crlf.source = f.source.replace('\n', "\r\n");
        assert_eq!(compile(&crlf).unwrap(), a);
        let mut whitespace = f.clone();
        whitespace.source.push('\n');
        let c = compile(&whitespace).unwrap();
        assert_ne!(a.digest, c.digest);
        assert_eq!(a.executable_fingerprint, c.executable_fingerprint);
        assert_eq!(
            semantic_diff(&a, &c).unwrap().kind,
            ChangeKind::Presentation
        );
        let mut build = f.clone();
        build.compiler = "compiler/new-build".into();
        let d = compile(&build).unwrap();
        assert_ne!(d.digest, a.digest);
        assert_eq!(d.executable_fingerprint, a.executable_fingerprint);
    }
    #[test]
    fn t1_integrity_rejects_replaced_bytes_noncanonical_and_sealed_invalid() {
        let a = compile(&fixture()).unwrap();
        let bytes = canonical_bytes(&a).unwrap();
        assert_eq!(Artifact::from_canonical_bytes(&bytes).unwrap(), a);
        let mut bad = a.clone();
        bad.content.playbook.purpose = "changed".into();
        assert!(bad.verify().is_err());
        let pretty = serde_json::to_vec_pretty(&a).unwrap();
        assert!(Artifact::from_canonical_bytes(&pretty).is_err());
        let mut malformed = a.content.clone();
        malformed.playbook.entries.get_mut("main").unwrap().start = "nonexistent".into();
        let sealed = Artifact::seal(malformed).unwrap();
        assert!(sealed.verify().is_err());
    }
    #[test]
    fn t2_unknown_commands_events_and_exact_fingerprints() {
        let mut f = invocation_fixture();
        f.registry.commands.clear();
        assert!(codes(&f).contains("unknown_command"));
        let mut f = invocation_fixture();
        if let Operation::Invoke { command, .. } =
            &mut f.proposal.playbook.steps.get_mut("observe").unwrap().op
        {
            command.fingerprint = "sha256:wrong".into();
        }
        assert!(codes(&f).contains("contract_fingerprint"));
        let mut f = fixture();
        f.proposal.playbook.triggers.push(Trigger {
            id: "start".into(),
            event: ContractRef {
                name: "unregistered".into(),
                fingerprint: "absent".into(),
            },
            entry: "main".into(),
            input: Expr::literal(Value::Null),
        });
        assert!(codes(&f).contains("unknown_event"));
    }
    #[test]
    fn t2_incompatible_schema_unhandled_outcome_and_errors() {
        let mut f = invocation_fixture();
        if let Operation::Invoke { input, .. } =
            &mut f.proposal.playbook.steps.get_mut("observe").unwrap().op
        {
            *input = Expr::literal("wrong");
        }
        assert!(codes(&f).contains("schema_compatibility"));
        let mut f = invocation_fixture();
        f.proposal
            .playbook
            .steps
            .get_mut("observe")
            .unwrap()
            .edges
            .remove("unavailable");
        assert!(codes(&f).contains("unhandled_outcome"));
        f.proposal
            .playbook
            .steps
            .get_mut("observe")
            .unwrap()
            .on_error = None;
        assert!(codes(&f).contains("unhandled_error"));
    }
    #[test]
    fn t2_questions_and_unsupported_features_never_package() {
        let mut f = fixture();
        f.proposal.questions.push("Who may approve?".into());
        assert!(compile(&f).is_err());
        assert!(
            validate(&f)
                .iter()
                .any(|d| d.severity == Severity::Question)
        );
        f.proposal.questions.clear();
        f.proposal
            .unsupported_features
            .push("embedded_shell".into());
        assert!(codes(&f).contains("unsupported_feature"));
        let unknown = json!({"kind":"eval_shell","script":"whoami"});
        assert!(serde_json::from_value::<Operation>(unknown).is_err());
    }
    #[test]
    fn t2_unbounded_cycles_and_recursive_calls_rejected() {
        let mut f = invocation_fixture();
        f.proposal
            .playbook
            .steps
            .get_mut("observe")
            .unwrap()
            .edges
            .insert("ok".into(), "observe".into());
        assert!(codes(&f).contains("unbounded_cycle"));
        let mut f = fixture();
        let e = f.proposal.playbook.entries["main"].clone();
        let hash = format!("sha256:{}", "a".repeat(64));
        f.dependencies.insert(
            "self".into(),
            CallableDependency {
                selection: DependencySelection::Exact,
                binding: "example".into(),
                artifact: hash.clone(),
                interfaces: BTreeMap::from([("main".into(), e)]),
                capabilities: BTreeSet::new(),
                scope: "project".into(),
                dependencies: BTreeSet::from([hash]),
            },
        );
        assert!(codes(&f).contains("recursive_call"));
    }
    #[test]
    fn explicit_unknown_branch_and_typed_references() {
        let mut f = fixture();
        f.proposal.playbook.entries.get_mut("main").unwrap().input =
            Schema::object(BTreeMap::from([("ready".into(), Schema::Boolean)]));
        f.proposal.playbook.entries.get_mut("main").unwrap().start = "choose".into();
        f.proposal.playbook.steps.insert(
            "choose".into(),
            Step {
                op: Operation::Decide {
                    predicate: Expr::Ref {
                        root: "input".into(),
                        path: vec!["ready".into()],
                    },
                },
                edges: ["true", "false", "unknown"]
                    .map(|k| (k.into(), "finish".into()))
                    .into(),
                on_error: None,
            },
        );
        assert!(compile(&f).is_ok());
        f.proposal
            .playbook
            .steps
            .get_mut("choose")
            .unwrap()
            .edges
            .remove("unknown");
        assert!(codes(&f).contains("unhandled_outcome"));
        if let Operation::Decide { predicate } =
            &mut f.proposal.playbook.steps.get_mut("choose").unwrap().op
        {
            *predicate = Expr::Ref {
                root: "input".into(),
                path: vec!["missing".into()],
            };
        }
        assert!(codes(&f).contains("expression_type"));
    }
    #[test]
    fn finite_foreach_and_explicit_parallel_limitation() {
        let mut f = fixture();
        f.proposal.playbook.entries.get_mut("main").unwrap().start = "loop".into();
        f.proposal.playbook.steps.insert(
            "loop".into(),
            Step {
                op: Operation::ForEach {
                    collection: Expr::literal(json!([1, 2])),
                    item: "number".into(),
                    body: "body".into(),
                    next: "finish".into(),
                    max_items: 2,
                    concurrency: 1,
                    on_failure: IterationFailure::Stop,
                },
                edges: BTreeMap::new(),
                on_error: Some("finish".into()),
            },
        );
        f.proposal.playbook.steps.insert(
            "body".into(),
            Step {
                op: Operation::Decide {
                    predicate: Expr::Eq {
                        left: Box::new(Expr::reference("number")),
                        right: Box::new(Expr::literal(1)),
                    },
                },
                edges: ["true", "false", "unknown"]
                    .map(|k| (k.into(), "loop".into()))
                    .into(),
                on_error: None,
            },
        );
        assert!(compile(&f).is_ok(), "{:?}", validate(&f));
        if let Operation::ForEach { concurrency, .. } =
            &mut f.proposal.playbook.steps.get_mut("loop").unwrap().op
        {
            *concurrency = 2;
        }
        assert!(codes(&f).contains("unsupported_feature"));
    }
    #[test]
    fn captured_definitions_and_source_identity_are_verified() {
        let mut f = fixture();
        f.definitions.insert(
            "config".into(),
            CapturedDefinition {
                version: "v1".into(),
                digest: digest(&json!({"n":1})).unwrap(),
                value: json!({"n":1}),
            },
        );
        assert!(codes(&f).contains("source_definitions"));
        let mut header = parse_source_header(&f.source).unwrap();
        header.definitions.insert("config".into());
        f.source = format!(
            "---aq-playbook\n{}\n---\n",
            serde_json::to_string(&header).unwrap()
        );
        assert!(compile(&f).is_ok());
        f.definitions.get_mut("config").unwrap().value = json!({"n":2});
        assert!(codes(&f).contains("definition"));
    }
    #[test]
    fn semantic_diff_exposes_contract_capability_and_interface_changes() {
        let a = compile(&fixture()).unwrap();
        let mut f = fixture();
        f.proposal.playbook.entries.get_mut("main").unwrap().input = Schema::Boolean;
        let b = compile(&f).unwrap();
        let d = semantic_diff(&a, &b).unwrap();
        assert_eq!(d.kind, ChangeKind::Interface);
        assert_eq!(d.changed_interfaces, vec!["main"]);
    }
    #[test]
    fn outcome_edges_narrow_nullable_and_incompatible_results() {
        let mut f = invocation_fixture();
        let output = Schema::object(BTreeMap::from([("subject".into(), Schema::string(8))]));
        let c = f.registry.commands.get_mut("test.observe").unwrap();
        c.outcomes.insert("ok".into(), output);
        c.outcomes.insert("unavailable".into(), Schema::Null);
        let fingerprint = c.fingerprint().unwrap();
        let entry = f.proposal.playbook.entries.get_mut("main").unwrap();
        entry.outcomes.insert("done".into(), Schema::string(8));
        entry.outcomes.insert("failed".into(), Schema::Null);
        let observe = f.proposal.playbook.steps.get_mut("observe").unwrap();
        if let Operation::Invoke { command, .. } = &mut observe.op {
            command.fingerprint = fingerprint;
        }
        observe.edges.insert("unavailable".into(), "failed".into());
        observe.on_error = Some("failed".into());
        f.proposal.playbook.steps.get_mut("finish").unwrap().op = Operation::Return {
            outcome: "done".into(),
            output: Expr::Ref {
                root: "observed".into(),
                path: vec!["subject".into()],
            },
        };
        f.proposal.playbook.steps.insert(
            "failed".into(),
            Step {
                op: Operation::Return {
                    outcome: "failed".into(),
                    output: Expr::literal(Value::Null),
                },
                edges: BTreeMap::new(),
                on_error: None,
            },
        );
        assert!(compile(&f).is_ok(), "{:?}", validate(&f));
        if let Operation::Return { output, .. } =
            &mut f.proposal.playbook.steps.get_mut("finish").unwrap().op
        {
            *output = Expr::Ref {
                root: "vars".into(),
                path: vec!["observed".into(), "subject".into()],
            };
        }
        assert!(compile(&f).is_ok(), "{:?}", validate(&f));
        // Converging success and failure at a field access must stay invalid.
        f.proposal
            .playbook
            .steps
            .get_mut("observe")
            .unwrap()
            .edges
            .insert("unavailable".into(), "finish".into());
        assert!(codes(&f).contains("expression_type"));
    }
    #[test]
    fn any_completion_rejects_ignored_outcome_qualifier() {
        let mut f = fixture();
        f.proposal.playbook.entries.get_mut("main").unwrap().start = "join".into();
        f.proposal.playbook.steps.insert(
            "join".into(),
            Step {
                op: Operation::Join {
                    member_keys: vec![],
                    members: vec![Expr::literal("child-id")],
                    mode: JoinMode::AnyCompletion,
                    accepted_outcomes: BTreeSet::from(["done".into()]),
                    deadline_ms: 100,
                    remaining: RemainingPolicy::Track,
                    bind: "joined".into(),
                },
                edges: ["completed", "timeout", "no_match"]
                    .map(|outcome| (outcome.into(), "finish".into()))
                    .into(),
                on_error: Some("finish".into()),
            },
        );
        assert!(codes(&f).contains("join_qualifier"));
        if let Operation::Join {
            accepted_outcomes, ..
        } = &mut f.proposal.playbook.steps.get_mut("join").unwrap().op
        {
            accepted_outcomes.clear();
        }
        assert!(!codes(&f).contains("join_qualifier"));
    }
    fn named_join_fixture() -> CompileInput {
        let mut f = fixture();
        let review_output = Schema::object(BTreeMap::from([("subject".into(), Schema::string(8))]));
        let final_output = Schema::object(BTreeMap::from([
            ("subject".into(), Schema::string(8)),
            ("checked".into(), Schema::Boolean),
        ]));
        let entry = f.proposal.playbook.entries.get_mut("main").unwrap();
        entry.start = "review".into();
        entry.outcomes.insert("done".into(), final_output);
        entry.outcomes.insert("failed".into(), Schema::Null);
        for (name, output) in [("review", review_output), ("check", Schema::Boolean)] {
            f.dependencies.insert(
                name.into(),
                CallableDependency {
                    binding: format!("{name}-workflow"),
                    selection: DependencySelection::Exact,
                    artifact: hash_bytes(name.as_bytes()),
                    interfaces: BTreeMap::from([(
                        "main".into(),
                        EntryPoint {
                            input: Schema::Null,
                            outcomes: BTreeMap::from([
                                ("ok".into(), output),
                                ("unavailable".into(), Schema::Null),
                            ]),
                            start: "child-start".into(),
                        },
                    )]),
                    capabilities: BTreeSet::new(),
                    scope: "project".into(),
                    dependencies: BTreeSet::new(),
                },
            );
            f.proposal.playbook.steps.insert(
                name.into(),
                Step {
                    op: Operation::Call {
                        dependency: name.into(),
                        entry: "main".into(),
                        input: Expr::literal(Value::Null),
                        mode: CallMode::OwnedAsync,
                        bind: format!("{name}_handle"),
                    },
                    edges: BTreeMap::from([(
                        "started".into(),
                        if name == "review" {
                            "check".into()
                        } else {
                            "join".into()
                        },
                    )]),
                    on_error: Some("failed".into()),
                },
            );
        }
        f.proposal.playbook.steps.insert(
            "join".into(),
            Step {
                op: Operation::Join {
                    members: vec![
                        Expr::reference("review_handle"),
                        Expr::reference("check_handle"),
                    ],
                    member_keys: vec!["review".into(), "check".into()],
                    mode: JoinMode::All,
                    accepted_outcomes: BTreeSet::from(["ok".into()]),
                    deadline_ms: 100,
                    remaining: RemainingPolicy::Track,
                    bind: "joined".into(),
                },
                edges: BTreeMap::from([
                    ("completed".into(), "finish".into()),
                    ("timeout".into(), "failed".into()),
                    ("no_match".into(), "failed".into()),
                ]),
                on_error: Some("failed".into()),
            },
        );
        f.proposal.playbook.steps.insert(
            "failed".into(),
            Step {
                op: Operation::Return {
                    outcome: "failed".into(),
                    output: Expr::literal(Value::Null),
                },
                edges: BTreeMap::new(),
                on_error: None,
            },
        );
        f.proposal.playbook.steps.get_mut("finish").unwrap().op = Operation::Return {
            outcome: "done".into(),
            output: Expr::Object {
                fields: BTreeMap::from([
                    (
                        "subject".into(),
                        Expr::Ref {
                            root: "joined".into(),
                            path: vec!["review".into(), "output".into(), "subject".into()],
                        },
                    ),
                    (
                        "checked".into(),
                        Expr::Ref {
                            root: "joined".into(),
                            path: vec!["check".into(), "output".into()],
                        },
                    ),
                ]),
            },
        };
        f
    }
    #[test]
    fn named_join_heterogeneous_results_feed_typed_downstream_outputs() {
        let f = named_join_fixture();
        assert!(compile(&f).is_ok(), "{:?}", validate(&f));
        let artifact = compile(&f).unwrap();
        assert!(artifact.verify().is_ok());
        let mut malformed = f.clone();
        if let Operation::Join { member_keys, .. } = &mut malformed
            .proposal
            .playbook
            .steps
            .get_mut("join")
            .unwrap()
            .op
        {
            member_keys[1] = "review".into();
        }
        assert!(codes(&malformed).contains("join_member_keys"));
        let mut unowned = f.clone();
        if let Operation::Call { mode, .. } = &mut unowned
            .proposal
            .playbook
            .steps
            .get_mut("review")
            .unwrap()
            .op
        {
            *mode = CallMode::Independent;
        }
        assert!(codes(&unowned).contains("join_member_type"));
    }
    #[test]
    fn named_join_no_match_does_not_inherit_success_output_refinement() {
        let mut f = named_join_fixture();
        f.proposal
            .playbook
            .steps
            .get_mut("join")
            .unwrap()
            .edges
            .insert("no_match".into(), "finish".into());
        assert!(codes(&f).contains("expression_type"));
    }
    #[test]
    fn named_catalog_selection_is_explicit_and_bounded() {
        let mut f = named_join_fixture();
        f.dependencies.get_mut("review").unwrap().selection = DependencySelection::Catalog {
            allowed_artifacts: BTreeSet::new(),
        };
        assert!(compile(&f).is_ok());
        if let DependencySelection::Catalog { allowed_artifacts } =
            &mut f.dependencies.get_mut("review").unwrap().selection
        {
            allowed_artifacts.insert("unversioned-name".into());
        }
        assert!(codes(&f).contains("dependency_selection"));
    }
    #[test]
    fn public_projection_supports_mixed_business_outcomes_without_exposing_protected_fields() {
        let mut f = invocation_fixture();
        let c = f.registry.commands.get_mut("test.observe").unwrap();
        c.outcomes.insert(
            "ok".into(),
            Schema::object(BTreeMap::from([("reference".into(), Schema::string(32))])),
        );
        c.public_result_fields.insert("reference".into());
        let fingerprint = c.fingerprint().unwrap();
        if let Operation::Invoke { command, .. } =
            &mut f.proposal.playbook.steps.get_mut("observe").unwrap().op
        {
            command.fingerprint = fingerprint;
        }
        assert!(compile(&f).is_ok(), "{:?}", validate(&f));
        f.registry
            .commands
            .get_mut("test.observe")
            .unwrap()
            .sensitive_output_fields
            .insert("reference".into());
        assert!(codes(&f).contains("sensitive_field"));
    }
    #[test]
    fn sensitive_event_fields_are_validated_and_fingerprinted_with_legacy_default() {
        let mut f = fixture();
        let mut event = EventContract {
            sensitive_payload_fields: BTreeSet::new(),
            name: "private.changed".into(),
            version: "1".into(),
            payload: Schema::object(BTreeMap::from([("token".into(), Schema::string(128))])),
            trusted_producers: BTreeSet::from(["host".into()]),
            scopes: BTreeSet::from(["project".into()]),
            correlation_fields: BTreeSet::new(),
            subject_field: None,
            revision_field: None,
            max_payload_bytes: 1024,
            retention_ms: 1000,
        };
        let public_fingerprint = event.fingerprint().unwrap();
        let mut legacy = serde_json::to_value(&event).unwrap();
        legacy
            .as_object_mut()
            .unwrap()
            .remove("sensitive_payload_fields");
        assert_eq!(
            serde_json::from_value::<EventContract>(legacy).unwrap(),
            event
        );
        event.sensitive_payload_fields.insert("token".into());
        assert_ne!(event.fingerprint().unwrap(), public_fingerprint);
        f.registry.events.insert(event.name.clone(), event.clone());
        assert!(compile(&f).is_ok());
        event
            .sensitive_payload_fields
            .insert("not_in_schema".into());
        f.registry.events.insert(event.name.clone(), event);
        assert!(codes(&f).contains("sensitive_field"));
    }
}
