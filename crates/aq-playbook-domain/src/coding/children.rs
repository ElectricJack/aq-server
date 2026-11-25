use super::*;

struct Child {
    id: &'static str,
    purpose: &'static str,
    command: &'static str,
    binding: &'static str,
    outcomes: [&'static str; 2],
}

// Each child can finish immediately or return an operation handle.
const WORK_CHILD: Child = Child {
    id: WORK,
    purpose: "Ensure one work item and converge its sync or async completion",
    command: "work.ensure",
    binding: "work",
    outcomes: ["accepted", "rejected"],
};
const CHECK_CHILD: Child = Child {
    id: CHECK,
    purpose: "Request one required check and converge its sync or async receipt",
    command: "check.request",
    binding: "check",
    outcomes: ["passed", "failed"],
};
const DECISION_CHILD: Child = Child {
    id: DECISION,
    purpose: "Request one human decision and converge its sync or async completion",
    command: "decision.request",
    binding: "decision",
    outcomes: ["approved", "rejected"],
};

impl Builder {
    pub(super) fn work(&self) -> Result<Artifact, CodingError> {
        let mut steps = self.work_completion()?;
        steps.insert(
            "assess".into(),
            decide(
                eq(r("work", &["disposition"]), "accepted"),
                DecisionEdges {
                    on_true: "accepted",
                    on_false: "rejected",
                },
            ),
        );
        self.child(
            &WORK_CHILD,
            steps,
            &[("completed", "assess"), ("accepted", "await")],
        )
    }

    pub(super) fn check(&self) -> Result<Artifact, CodingError> {
        self.verdict_child(
            &CHECK_CHILD,
            CompletionWait {
                event: "aq.check.receipted",
                handle: "check",
                deadline_ms: CHECK_WAIT_MS,
                bind: "receipt",
                on_receive: "assess",
            },
        )
    }

    pub(super) fn decision(&self) -> Result<Artifact, CodingError> {
        // Refused, expired and cancelled are not human verdicts. They escalate.
        self.verdict_child(
            &DECISION_CHILD,
            CompletionWait {
                event: "aq.decision.completed",
                handle: "decision",
                deadline_ms: DECISION_WAIT_MS,
                bind: "completion",
                on_receive: "assess",
            },
        )
    }

    fn verdict_child(
        &self,
        child: &Child,
        wait: CompletionWait<'_>,
    ) -> Result<Artifact, CodingError> {
        let receipt = wait.bind;
        let [good, bad] = child.outcomes;
        let assess_bad = format!("assess_{bad}");
        let mut steps = BTreeMap::from([
            ("await".into(), self.wait(wait)?),
            (
                "assess".into(),
                decide(
                    eq(r(receipt, &["outcome"]), good),
                    DecisionEdges {
                        on_true: &format!("{good}_event"),
                        on_false: &assess_bad,
                    },
                ),
            ),
            (
                assess_bad,
                decide(
                    eq(r(receipt, &["outcome"]), bad),
                    DecisionEdges {
                        on_true: &format!("{bad}_event"),
                        on_false: "failure",
                    },
                ),
            ),
        ]);
        for outcome in child.outcomes {
            steps.insert(
                format!("{outcome}_event"),
                ret(outcome, BoundResult::Completion(receipt).projection()),
            );
        }
        self.child(
            child,
            steps,
            &[(good, good), (bad, bad), ("accepted", "await")],
        )
    }

    fn child(
        &self,
        child: &Child,
        mut steps: BTreeMap<String, Step>,
        edges: &[(&str, &str)],
    ) -> Result<Artifact, CodingError> {
        let command = self.command(child.command)?;
        steps.insert(
            "request".into(),
            Self::invoke(command, r("input", &[]), child.binding, edges)?,
        );
        let mut outcomes = BTreeMap::from([("escalated".into(), Self::escalated())]);
        for outcome in child.outcomes {
            steps.insert(
                outcome.into(),
                ret(outcome, BoundResult::Direct(child.binding).projection()),
            );
            outcomes.insert(outcome.into(), self.result()?);
        }
        self.seal(
            ArtifactDraft {
                id: child.id,
                purpose: child.purpose,
                entry: EntryPoint {
                    input: command.input.clone(),
                    outcomes,
                    start: "request".into(),
                },
                steps,
            },
            &[],
            fields(&[]),
        )
    }
    fn work_completion(&self) -> Result<BTreeMap<String, Step>, CodingError> {
        Ok(BTreeMap::from([
            (
                "await".into(),
                self.wait(CompletionWait {
                    event: "aq.operation.updated",
                    handle: "work",
                    deadline_ms: WORK_WAIT_MS,
                    bind: "completion",
                    on_receive: "completed",
                })?,
            ),
            (
                "completed".into(),
                decide(
                    eq(r("completion", &["outcome"]), "completed"),
                    DecisionEdges {
                        on_true: "assess_event",
                        on_false: "failure",
                    },
                ),
            ),
            (
                "assess_event".into(),
                decide(
                    eq(r("completion", &["output", "disposition"]), "accepted"),
                    DecisionEdges {
                        on_true: "accepted_event",
                        on_false: "rejected_event",
                    },
                ),
            ),
            (
                "accepted_event".into(),
                ret(
                    "accepted",
                    BoundResult::Completion("completion").projection(),
                ),
            ),
            (
                "rejected_event".into(),
                ret(
                    "rejected",
                    BoundResult::Completion("completion").projection(),
                ),
            ),
        ]))
    }
}
