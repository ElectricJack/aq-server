use super::*;

/// Routing pilot. Input `{attempt, work}`: the execution attempt id and the
/// namespace work item subject.
pub fn routing() -> Result<Artifact, CodingError> {
    let b = Builder::new();
    let result = b.result()?;
    let mut steps = routing_options(&b)?;
    add_routing_outcomes(&b, &mut steps)?;
    let input = Schema::object(BTreeMap::from([
        ("attempt".into(), Schema::string(TEXT)),
        ("work".into(), b.subject()?),
    ]));
    let outcomes = BTreeMap::from([
        ("routed".into(), result.clone()),
        ("stale".into(), result),
        ("escalated".into(), Builder::escalated()),
    ]);
    b.seal(
        ArtifactDraft {
            id: ROUTING,
            purpose: "Constrain one work item to its permitted execution options",
            entry: EntryPoint {
                input,
                outcomes,
                start: "options".into(),
            },
            steps,
        },
        &[],
        fields(&[]),
    )
}

fn routing_options(b: &Builder) -> Result<BTreeMap<String, Step>, CodingError> {
    Ok(BTreeMap::from([
        (
            "options".into(),
            Builder::invoke(
                b.command("context.read")?,
                read(
                    "execution_options",
                    fields(&[
                        ("id", r("input", &["attempt"])),
                        ("revision", lit(json!(0))),
                    ]),
                    lit(json!(0)),
                ),
                "options",
                &[("observed", "known"), ("incomplete", "judge")],
            )?,
        ),
        (
            "known".into(),
            decide(
                is_true(r("options", &["complete"])),
                DecisionEdges {
                    on_true: "read_known",
                    on_false: "judge",
                },
            ),
        ),
        (
            "judge".into(),
            Builder::invoke(
                b.command("judgment.request")?,
                assessment_input(),
                "judgment",
                &[("accepted", "await_judgment")],
            )?,
        ),
        (
            "await_judgment".into(),
            b.wait(CompletionWait {
                event: "aq.judgment.completed",
                handle: "judgment",
                deadline_ms: JUDGMENT_WAIT_MS,
                bind: "assessment",
                on_receive: "assessed",
            })?,
        ),
        (
            "assessed".into(),
            decide(
                eq(r("assessment", &["outcome"]), "assessed"),
                DecisionEdges {
                    on_true: "read_judged",
                    on_false: "failure",
                },
            ),
        ),
    ]))
}
fn assessment_input() -> Expr {
    fields(&[
        (
            "budget",
            lit(json!({"deadline_ms": 60000, "max_cost_microunits": 50000,
                            "max_input_tokens": 8192, "max_output_tokens": 512})),
        ),
        (
            "context",
            fields(&[
                ("artifacts", lit(json!([]))),
                (
                    "text",
                    lit(json!("Execution options for this attempt are incomplete.")),
                ),
            ]),
        ),
        (
            "output",
            lit(json!({"choices": ["permitted-default", "isolated"],
                            "rationale_max_bytes": 512})),
        ),
        (
            "question",
            lit(json!(
                "Which permitted execution constraint fits this work item?"
            )),
        ),
        ("subject", r("input", &["work"])),
    ])
}

fn add_routing_outcomes(
    b: &Builder,
    steps: &mut BTreeMap<String, Step>,
) -> Result<(), CodingError> {
    let change = |value: Expr, read_bind: &str| {
        fields(&[
            ("variant", lit(json!("execution_constraints"))),
            (
                "subject",
                fields(&[
                    ("id", r("input", &["work", "id"])),
                    ("revision", r(read_bind, &["subject", "revision"])),
                ]),
            ),
            ("value", value),
            ("hold_id", lit(Value::Null)),
        ])
    };
    for (path, value) in [
        ("known", r("options", &["snapshot"])),
        // The completion output is a union whose variants share only
        // `reference`/`subject`, so the constraint names the retained
        // judgment (whose choice AQ holds), never a caller-asserted value.
        ("judged", r("assessment", &["output", "reference"])),
    ] {
        let read_bind = format!("work_{path}");
        steps.insert(
            format!("read_{path}"),
            Builder::invoke(
                b.command("context.read")?,
                read("work", r("input", &["work"]), lit(json!(0))),
                &read_bind,
                &[("observed", &format!("change_{path}"))],
            )?,
        );
        let change_bind = format!("changed_{path}");
        steps.insert(
            format!("change_{path}"),
            Builder::invoke(
                b.command("work.change")?,
                change(value, &read_bind),
                &change_bind,
                &[
                    ("changed", &format!("routed_{path}")),
                    ("conflict", &format!("stale_{path}")),
                ],
            )?,
        );
        steps.insert(
            format!("routed_{path}"),
            ret("routed", BoundResult::Direct(&change_bind).projection()),
        );
        steps.insert(
            format!("stale_{path}"),
            ret("stale", BoundResult::Direct(&change_bind).projection()),
        );
    }
    Ok(())
}
