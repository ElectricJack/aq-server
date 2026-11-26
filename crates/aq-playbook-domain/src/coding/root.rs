use super::*;

impl Builder {
    pub(super) fn root(
        &self,
        policy: &CodingPolicy,
        attempt: &Artifact,
    ) -> Result<Artifact, CodingError> {
        let input = self.root_input(policy)?;
        let steps = repair_steps(policy);
        let outcomes = BTreeMap::from([
            ("published".into(), self.result()?),
            ("exhausted".into(), Self::reference_only()),
            ("escalated".into(), Schema::Null),
        ]);
        self.seal(
            ArtifactDraft {
                id: ROOT,
                purpose: "Default coding policy: implement, check, cross-family review, publish, repair",
                entry: EntryPoint { input, outcomes, start: "attempt_0".into() },
                steps,
            },
            &[attempt],
            lit(Value::Null),
        )
    }
    fn root_input(&self, policy: &CodingPolicy) -> Result<Schema, CodingError> {
        let key = Schema::object(BTreeMap::from([
            ("work".into(), Schema::string(TEXT)),
            ("review".into(), Schema::string(TEXT)),
        ]));
        let keys = (0..=policy.max_repairs)
            .map(|i| (format!("a{i}"), key.clone()))
            .collect();
        let mut input = BTreeMap::from([
            ("subject".into(), self.subject()?),
            ("context".into(), Schema::string(TEXT)),
            ("base".into(), self.subject()?),
            ("destination".into(), self.subject()?),
            (
                "source".into(),
                self.input_field("check.request", "source")?,
            ),
            ("visibility".into(), Schema::string(TEXT)),
            ("keys".into(), Schema::object(keys)),
        ]);
        if policy.human_decision.is_some() {
            input.insert("decision_expires_at_ms".into(), self.expiry()?);
        }
        Ok(Schema::object(input))
    }
}

fn repair_steps(policy: &CodingPolicy) -> BTreeMap<String, Step> {
    let mut steps = BTreeMap::new();
    for i in 0..=policy.max_repairs {
        let bind = format!("attempt_{i}");
        let a = format!("a{i}");
        let (kind, context) = if i == 0 {
            ("implementation", r("input", &["context"]))
        } else {
            // Repair carries the previous attempt's failing evidence.
            ("repair", r(&format!("attempt_{}", i - 1), &["reference"]))
        };
        let next = if i == policy.max_repairs {
            "exhausted".to_string()
        } else {
            format!("attempt_{}", i + 1)
        };
        let published = format!("published_{i}");
        steps.insert(
            bind.clone(),
            call(
                ATTEMPT,
                attempt_input(policy, kind, context, &a),
                &bind,
                &[
                    ("published", &published),
                    ("rework", &next),
                    ("escalated", "failure"),
                ],
            ),
        );
        steps.insert(
            published,
            ret("published", BoundResult::Direct(&bind).projection()),
        );
    }
    let last = format!("attempt_{}", policy.max_repairs);
    steps.insert(
        "exhausted".into(),
        ret(
            "exhausted",
            fields(&[("reference", r(&last, &["reference"]))]),
        ),
    );
    steps
}

fn attempt_input(policy: &CodingPolicy, kind: &str, context: Expr, a: &str) -> Expr {
    fields(
        &[
            (
                "work",
                fields(&[
                    ("kind", lit(json!(kind))),
                    ("subject", r("input", &["subject"])),
                    ("context", context),
                    ("lifetime", lit(json!("owned"))),
                    ("logical_key", r("input", &["keys", a, "work"])),
                    ("visibility", r("input", &["visibility"])),
                ]),
            ),
            ("base", r("input", &["base"])),
            ("destination", r("input", &["destination"])),
            ("source", r("input", &["source"])),
            ("review_key", r("input", &["keys", a, "review"])),
            ("visibility", r("input", &["visibility"])),
        ]
        .into_iter()
        .chain(policy.human_decision.as_ref().map(|_| {
            (
                "decision_expires_at_ms",
                r("input", &["decision_expires_at_ms"]),
            )
        }))
        .collect::<Vec<_>>(),
    )
}
