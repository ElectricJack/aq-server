use super::*;

impl Builder {
    pub(super) fn expiry(&self) -> Result<Schema, CodingError> {
        self.input_field("decision.request", "expires_at_ms")
    }

    fn attempt_input(&self, policy: &CodingPolicy) -> Result<Schema, CodingError> {
        let mut properties = BTreeMap::from([
            ("work".into(), self.command("work.ensure")?.input.clone()),
            ("base".into(), self.subject()?),
            ("destination".into(), self.subject()?),
            (
                "source".into(),
                self.input_field("check.request", "source")?,
            ),
            ("review_key".into(), Schema::string(TEXT)),
            ("visibility".into(), Schema::string(TEXT)),
        ]);
        if policy.human_decision.is_some() {
            properties.insert("decision_expires_at_ms".into(), self.expiry()?);
        }
        Ok(Schema::object(properties))
    }

    pub(super) fn attempt(
        &self,
        policy: &CodingPolicy,
        children: &[&Artifact],
    ) -> Result<Artifact, CodingError> {
        let mut flow = AttemptSteps::prepare(self, policy)?;
        flow.checks(&policy.checks);
        flow.review(policy);
        if let Some(decision) = &policy.human_decision {
            flow.decision(decision);
        }
        let steps = flow.publish(self)?;
        let outcomes = BTreeMap::from([
            ("published".into(), self.result()?),
            ("rework".into(), Self::reference_only()),
            ("escalated".into(), Self::escalated()),
        ]);
        self.seal(
            ArtifactDraft {
                id: ATTEMPT,
                purpose: "One coding attempt: work, prepare, checks, cross-family review, publish",
                entry: EntryPoint {
                    input: self.attempt_input(policy)?,
                    outcomes,
                    start: "work".into(),
                },
                steps,
            },
            children,
            fields(&[]),
        )
    }
}

struct AttemptSteps {
    steps: BTreeMap<String, Step>,
    evidence: Vec<Expr>,
}
fn rework(bind: &str) -> Step {
    ret("rework", fields(&[("reference", r(bind, &["reference"]))]))
}
impl AttemptSteps {
    fn prepare(builder: &Builder, policy: &CodingPolicy) -> Result<Self, CodingError> {
        let mut steps = BTreeMap::from([
            (
                "work".into(),
                call(
                    WORK,
                    r("input", &["work"]),
                    "work",
                    &[
                        ("accepted", "prepare"),
                        ("rejected", "rework_work"),
                        ("escalated", "failure"),
                    ],
                ),
            ),
            ("rework_work".into(), rework("work")),
        ]);
        let first = if policy.checks.is_empty() {
            "review".to_string()
        } else {
            "check_0".to_string()
        };
        steps.insert(
            "prepare".into(),
            Builder::invoke(
                builder.command("candidate.prepare")?,
                fields(&[
                    ("base", r("input", &["base"])),
                    (
                        "sources",
                        Expr::Array {
                            items: vec![r("work", &["subject"])],
                        },
                    ),
                ]),
                "candidate",
                &[("prepared", &first), ("conflict", "rework_prepare")],
            )?,
        );
        steps.insert("rework_prepare".into(), rework("candidate"));
        Ok(Self {
            steps,
            evidence: Vec::new(),
        })
    }
    fn checks(&mut self, checks: &[RequiredCheck]) {
        for (i, c) in checks.iter().enumerate() {
            let bind = format!("check_{i}");
            let next = if i + 1 == checks.len() {
                "review".to_string()
            } else {
                format!("check_{}", i + 1)
            };
            let failed = format!("rework_check_{i}");
            self.steps.insert(
                bind.clone(),
                call(
                    CHECK,
                    fields(&[
                        (
                            "check",
                            lit(json!({"id": c.id, "validator": {"id": c.validator_id,
                                "version": c.validator_version, "digest": c.validator_digest}})),
                        ),
                        // The source the host prepared (2.1.0 output), not a caller input.
                        ("source", r("candidate", &["source"])),
                        ("subject", r("candidate", &["subject"])),
                    ]),
                    &bind,
                    &[
                        ("passed", &next),
                        ("failed", &failed),
                        ("escalated", "failure"),
                    ],
                ),
            );
            self.steps.insert(failed, rework(&bind));
            self.evidence.push(fields(&[
                ("id", r(&bind, &["reference"])),
                ("revision", r("candidate", &["subject", "revision"])),
            ]));
        }
    }
    fn review(&mut self, policy: &CodingPolicy) {
        self.steps.insert(
            "review".into(),
            call(
                WORK,
                fields(&[
                    ("kind", lit(json!("review"))),
                    ("subject", r("candidate", &["subject"])),
                    ("context", lit(json!(policy.review_context()))),
                    ("lifetime", lit(json!("owned"))),
                    ("logical_key", r("input", &["review_key"])),
                    ("visibility", r("input", &["visibility"])),
                ]),
                "review",
                &[
                    (
                        "accepted",
                        if policy.human_decision.is_some() {
                            "decision"
                        } else {
                            "publish"
                        },
                    ),
                    ("rejected", "rework_review"),
                    ("escalated", "failure"),
                ],
            ),
        );
        self.steps.insert("rework_review".into(), rework("review"));
        self.evidence.push(fields(&[
            ("id", r("review", &["reference"])),
            ("revision", r("candidate", &["subject", "revision"])),
        ]));
    }
    fn decision(&mut self, d: &HumanDecision) {
        // The human decision follows, and never replaces, the review.
        self.steps.insert(
            "decision".into(),
            call(
                DECISION,
                fields(&[
                    ("question", lit(json!(d.question))),
                    ("choices", lit(json!(DECISION_CHOICES))),
                    ("responders", lit(json!(d.responders))),
                    ("subject", r("candidate", &["subject"])),
                    ("expires_at_ms", r("input", &["decision_expires_at_ms"])),
                ]),
                "decision",
                &[
                    ("approved", "publish"),
                    ("rejected", "rework_decision"),
                    ("escalated", "failure"),
                ],
            ),
        );
        self.steps
            .insert("rework_decision".into(), rework("decision"));
        self.evidence.push(fields(&[
            ("id", r("decision", &["reference"])),
            ("revision", r("candidate", &["subject", "revision"])),
        ]));
    }
    fn publish(mut self, builder: &Builder) -> Result<BTreeMap<String, Step>, CodingError> {
        self.steps.insert(
            "publish".into(),
            Builder::invoke(
                builder.command("candidate.publish")?,
                fields(&[
                    ("candidate", r("candidate", &["subject"])),
                    ("destination", r("input", &["destination"])),
                    (
                        "evidence",
                        Expr::Array {
                            items: self.evidence,
                        },
                    ),
                ]),
                "publication",
                &[("published", "published"), ("conflict", "rework_publish")],
            )?,
        );
        self.steps.insert(
            "published".into(),
            ret("published", BoundResult::Direct("publication").projection()),
        );
        self.steps
            .insert("rework_publish".into(), rework("publication"));
        Ok(self.steps)
    }
}
