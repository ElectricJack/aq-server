use super::*;

/// Report pilot waiting `deadline_ms` for its author. Input `{identity,
/// key, recipient}`: the report window identity, the author work key and the
/// granted recipient.
pub fn report(deadline_ms: u64) -> Result<Artifact, CodingError> {
    let b = Builder::new();
    let result = b.result()?;
    let mut steps = request_authorship(&b, deadline_ms)?;
    steps.extend(send_report(&b)?);
    let input = Schema::object(BTreeMap::from([
        ("identity".into(), Schema::string(TEXT)),
        ("key".into(), Schema::string(TEXT)),
        ("recipient".into(), Schema::string(TEXT)),
    ]));
    let outcomes = BTreeMap::from([
        ("authored".into(), result.clone()),
        ("fallback".into(), result),
        ("escalated".into(), Builder::escalated()),
    ]);
    b.seal(
        ArtifactDraft {
            id: REPORT,
            purpose: "Send exactly one supervisor report: authored by the deadline, or a fallback",
            entry: EntryPoint {
                input,
                outcomes,
                start: "window".into(),
            },
            steps,
        },
        &[],
        fields(&[]),
    )
}

fn request_authorship(
    b: &Builder,
    deadline_ms: u64,
) -> Result<BTreeMap<String, Step>, CodingError> {
    let mut wait = b.wait(CompletionWait {
        event: "aq.operation.updated",
        handle: "author",
        deadline_ms,
        bind: "authored",
        on_receive: "assess",
    })?;
    wait.edges.insert("timeout".into(), "fallback".into());
    Ok(BTreeMap::from([
        (
            "window".into(),
            Builder::invoke(
                b.command("context.read")?,
                read(
                    "project",
                    fields(&[("id", lit(json!("project"))), ("revision", lit(json!(0)))]),
                    lit(json!(0)),
                ),
                "window",
                &[("observed", "author")],
            )?,
        ),
        (
            "author".into(),
            Builder::invoke(
                b.command("work.ensure")?,
                fields(&[
                    ("kind", lit(json!("supervisor_authoring"))),
                    ("lifetime", lit(json!("owned"))),
                    ("logical_key", r("input", &["key"])),
                    ("context", r("window", &["reference"])),
                    ("subject", r("window", &["subject"])),
                    ("visibility", r("window", &["visibility"])),
                ]),
                "author",
                &[("accepted", "await")],
            )?,
        ),
        ("await".into(), wait),
        (
            "assess".into(),
            decide(
                Expr::And {
                    values: vec![
                        eq(r("authored", &["outcome"]), "completed"),
                        eq(r("authored", &["output", "disposition"]), "accepted"),
                    ],
                },
                DecisionEdges {
                    on_true: "send_authored",
                    on_false: "fallback",
                },
            ),
        ),
    ]))
}
fn send_report(b: &Builder) -> Result<BTreeMap<String, Step>, CodingError> {
    let send = |body: Expr| {
        fields(&[
            ("body", body),
            ("identity", r("input", &["identity"])),
            ("recipient", r("input", &["recipient"])),
            ("visibility", lit(json!("project"))),
        ])
    };
    Ok(BTreeMap::from([
        (
            "send_authored".into(),
            Builder::invoke(
                b.command("message.send")?,
                send(r("authored", &["output", "reference"])),
                "authored_message",
                &[("queued", "authored_sent")],
            )?,
        ),
        (
            "fallback".into(),
            Builder::invoke(
                b.command("message.send")?,
                send(lit(json!(REPORT_FALLBACK))),
                "fallback_message",
                &[("queued", "fallback_sent")],
            )?,
        ),
        (
            "authored_sent".into(),
            ret(
                "authored",
                BoundResult::Direct("authored_message").projection(),
            ),
        ),
        (
            "fallback_sent".into(),
            ret(
                "fallback",
                BoundResult::Direct("fallback_message").projection(),
            ),
        ),
    ]))
}
