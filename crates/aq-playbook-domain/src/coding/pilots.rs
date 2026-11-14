//! D11 routing, watcher and report pilots, sealed from the pinned Host8
//! (catalog 2.1.0) contracts with the D9 builder. They run on the production
//! catalog 2.x (Host7/Host8) adapters; nothing here talks to a host.
//!
//! - [`routing`] (`aq.pilot.routing`): read the attempt's permitted execution
//!   options (`context.read view=execution_options`). Complete facts pin the
//!   observed selection; missing facts ask one `judgment.request` (async; the
//!   constraint names the retained `aq.judgment.completed` judgment). Either way the work's current revision
//!   is read and `work.change variant=execution_constraints` is applied only
//!   at that revision: `routed`, or `stale` on a concurrent change. Actual
//!   placement stays AQ-owned.
//! - [`watcher`] (`aq.pilot.watcher`): the trusted host evaluator has started
//!   (captured prior episode revisions) and completed a health scan; the run
//!   requires a complete page-0 `context.read view=health_observations`
//!   before `findings.reconcile`. A missing page or incomplete read returns
//!   `incomplete` and never reconciles; a stale baseline is `conflict`. The
//!   reminder cadence is the run's `reminder_ms` parameter.
//! - [`report`] (`aq.pilot.report`): freeze the project-visible context
//!   window (`context.read view=project`), `work.ensure` exactly one
//!   `supervisor_authoring` item, wait for it until the deadline, then send
//!   exactly one `message.send`: the authored result, or a deterministic
//!   fallback. A late author completion is retained by the host and engine;
//!   the run has already sent its one report.
use super::*;

pub const ROUTING: &str = "aq.pilot.routing";
pub const WATCHER: &str = "aq.pilot.watcher";
pub const REPORT: &str = "aq.pilot.report";
/// The deterministic report body used when no authored result arrives by
/// the deadline.
pub const REPORT_FALLBACK: &str = "Supervisor report: the authored summary did not arrive by the report deadline. \
     The frozen project context window is retained in AQ.";
const JUDGMENT_WAIT_MS: u64 = 600_000;

fn is_true(e: Expr) -> Expr {
    Expr::Eq {
        left: Box::new(e),
        right: Box::new(lit(json!(true))),
    }
}
fn read(view: &str, subject: Expr, page: Expr) -> Expr {
    fields(&[
        ("view", lit(json!(view))),
        ("subject", subject),
        ("page", page),
        ("snapshot", lit(json!(""))),
    ])
}

/// Routing pilot. Input `{attempt, work}`: the execution attempt id and the
/// namespace work item subject.
pub fn routing() -> Result<Artifact, CodingError> {
    let b = Builder::new();
    let result = b.result()?;
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
    let work_subject = || r("input", &["work"]);
    let steps = BTreeMap::from([
        (
            "options".into(),
            b.invoke(
                "context.read",
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
            decide(is_true(r("options", &["complete"])), "read_known", "judge"),
        ),
        (
            "judge".into(),
            b.invoke(
                "judgment.request",
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
                    ("subject", work_subject()),
                ]),
                "judgment",
                &[("accepted", "await_judgment")],
            )?,
        ),
        (
            "await_judgment".into(),
            b.wait(
                "aq.judgment.completed",
                "judgment",
                JUDGMENT_WAIT_MS,
                "assessment",
                "assessed",
            )?,
        ),
        (
            "assessed".into(),
            decide(
                eq(r("assessment", &["outcome"]), "assessed"),
                "read_judged",
                "failure",
            ),
        ),
    ]);
    let mut steps = steps;
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
            b.invoke(
                "context.read",
                read("work", work_subject(), lit(json!(0))),
                &read_bind,
                &[("observed", &format!("change_{path}"))],
            )?,
        );
        let change_bind = format!("changed_{path}");
        steps.insert(
            format!("change_{path}"),
            b.invoke(
                "work.change",
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
            ret("routed", result_of(&change_bind, &[])),
        );
        steps.insert(
            format!("stale_{path}"),
            ret("stale", result_of(&change_bind, &[])),
        );
    }
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
        ROUTING,
        "Constrain one work item to its permitted execution options",
        input,
        outcomes,
        "options",
        steps,
        &[],
        fields(&[]),
    )
}

/// Watcher pilot. Input `{partition, observation, evaluation, findings,
/// reminder_ms, page}`: the evaluator's completed scan and its findings.
pub fn watcher() -> Result<Artifact, CodingError> {
    let b = Builder::new();
    let result = b.result()?;
    let input_field = |f: &str| b.input_field("findings.reconcile", f);
    let steps = BTreeMap::from([
        (
            "observe".into(),
            b.invoke(
                "context.read",
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
                "reconcile",
                "incomplete",
            ),
        ),
        (
            "reconcile".into(),
            b.invoke(
                "findings.reconcile",
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
            ret("reconciled", result_of("findings", &[])),
        ),
        (
            "conflict".into(),
            ret("conflict", result_of("findings", &[])),
        ),
        (
            "incomplete".into(),
            ret("incomplete", result_of("health", &[])),
        ),
    ]);
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
        WATCHER,
        "Reconcile findings from one complete health scan",
        input,
        outcomes,
        "observe",
        steps,
        &[],
        fields(&[]),
    )
}

/// Report pilot waiting `deadline_ms` for its author. Input `{identity,
/// key, recipient}`: the report window identity, the author work key and the
/// granted recipient.
pub fn report(deadline_ms: u64) -> Result<Artifact, CodingError> {
    let b = Builder::new();
    let result = b.result()?;
    let send = |body: Expr| {
        fields(&[
            ("body", body),
            ("identity", r("input", &["identity"])),
            ("recipient", r("input", &["recipient"])),
            ("visibility", lit(json!("project"))),
        ])
    };
    let mut wait = b.wait(
        "aq.operation.updated",
        "author",
        deadline_ms,
        "authored",
        "assess",
    )?;
    wait.edges.insert("timeout".into(), "fallback".into());
    let steps = BTreeMap::from([
        (
            "window".into(),
            b.invoke(
                "context.read",
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
            b.invoke(
                "work.ensure",
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
                "send_authored",
                "fallback",
            ),
        ),
        (
            "send_authored".into(),
            b.invoke(
                "message.send",
                send(r("authored", &["output", "reference"])),
                "authored_message",
                &[("queued", "authored_sent")],
            )?,
        ),
        (
            "fallback".into(),
            b.invoke(
                "message.send",
                send(lit(json!(REPORT_FALLBACK))),
                "fallback_message",
                &[("queued", "fallback_sent")],
            )?,
        ),
        (
            "authored_sent".into(),
            ret("authored", result_of("authored_message", &[])),
        ),
        (
            "fallback_sent".into(),
            ret("fallback", result_of("fallback_message", &[])),
        ),
    ]);
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
        REPORT,
        "Send exactly one supervisor report: authored by the deadline, or a fallback",
        input,
        outcomes,
        "window",
        steps,
        &[],
        fields(&[]),
    )
}
