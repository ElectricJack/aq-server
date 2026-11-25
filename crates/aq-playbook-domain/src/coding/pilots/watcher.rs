use super::*;

/// Watcher pilot. Input `{partition, observation, evaluation, findings,
/// reminder_ms, page}`: the evaluator's completed scan and its findings.
pub fn watcher() -> Result<Artifact, CodingError> {
    let b = Builder::new();
    let result = b.result()?;
    let input_field = |f: &str| b.input_field("findings.reconcile", f);
    let steps = reconcile_steps(&b)?;
    let input = Schema::object(BTreeMap::from([
        ("partition".into(), input_field("partition")?),
        ("observation".into(), input_field("observation")?),
        ("evaluation".into(), input_field("evaluation")?),
        ("findings".into(), input_field("findings")?),
        ("reminder_ms".into(), input_field("reminder_ms")?),
        ("page".into(), b.input_field("context.read", "page")?),
    ]));
    let outcomes = BTreeMap::from([
        ("reconciled".into(), result.clone()),
        ("conflict".into(), result.clone()),
        ("incomplete".into(), result),
        ("escalated".into(), Builder::escalated()),
    ]);
    b.seal(
        ArtifactDraft {
            id: WATCHER,
            purpose: "Reconcile findings from one complete health scan",
            entry: EntryPoint {
                input,
                outcomes,
                start: "observe".into(),
            },
            steps,
        },
        &[],
        fields(&[]),
    )
}

fn reconcile_steps(b: &Builder) -> Result<BTreeMap<String, Step>, CodingError> {
    Ok(BTreeMap::from([
        (
            "observe".into(),
            Builder::invoke(
                b.command("context.read")?,
                read(
                    "health_observations",
                    fields(&[
                        ("id", r("input", &["partition"])),
                        ("revision", lit(json!(0))),
                    ]),
                    r("input", &["page"]),
                ),
                "health",
                &[("observed", "complete"), ("incomplete", "incomplete")],
            )?,
        ),
        (
            "complete".into(),
            decide(
                is_true(r("health", &["complete"])),
                DecisionEdges {
                    on_true: "reconcile",
                    on_false: "incomplete",
                },
            ),
        ),
        (
            "reconcile".into(),
            Builder::invoke(
                b.command("findings.reconcile")?,
                fields(&[
                    ("observation", r("input", &["observation"])),
                    ("partition", r("input", &["partition"])),
                    ("evaluation", r("input", &["evaluation"])),
                    ("findings", r("input", &["findings"])),
                    ("reminder_ms", r("input", &["reminder_ms"])),
                ]),
                "findings",
                &[("reconciled", "reconciled"), ("conflict", "conflict")],
            )?,
        ),
        (
            "reconciled".into(),
            ret("reconciled", BoundResult::Direct("findings").projection()),
        ),
        (
            "conflict".into(),
            ret("conflict", BoundResult::Direct("findings").projection()),
        ),
        (
            "incomplete".into(),
            ret("incomplete", BoundResult::Direct("health").projection()),
        ),
    ]))
}
