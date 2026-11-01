//! Explicit synthetic AQ pilot contracts. Production hosts must bind their own implementations.
use aq_playbooks_model::*;
use std::collections::BTreeSet;

pub fn strings(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).into()).collect()
}
pub fn object(fields: &[(&str, Schema)]) -> Schema {
    Schema::object(fields.iter().cloned().map(|(k, v)| (k.into(), v)).collect())
}
pub fn enumeration(values: &[&str]) -> Schema {
    Schema::String {
        max_length: 128,
        values: strings(values),
    }
}
pub fn text() -> Schema {
    Schema::string(4096)
}
pub fn revision() -> Schema {
    Schema::Integer {
        minimum: 0,
        maximum: i64::MAX,
    }
}
pub fn subject() -> Schema {
    object(&[("id", text()), ("revision", revision())])
}
pub fn result() -> Schema {
    object(&[("reference", text()), ("subject", subject())])
}
pub fn work_result() -> Schema {
    object(&[
        ("reference", text()),
        ("subject", subject()),
        (
            "disposition",
            enumeration(&["accepted", "changes_requested", "failed"]),
        ),
    ])
}
pub fn list(item: Schema) -> Schema {
    Schema::Array {
        items: Box::new(item),
        max_items: 100,
    }
}
pub const CONTEXT_VIEWS: &[&str] = &[
    "work",
    "project",
    "execution_options",
    "evidence",
    "health_observations",
];
pub const WORK_KINDS: &[&str] = &[
    "implementation",
    "investigation",
    "planning",
    "review",
    "repair",
    "supervisor_authoring",
    "spec_ingestion",
    "fidelity_review",
    "onboarding",
];
pub const CHANGE_VARIANTS: &[&str] = &[
    "requirements",
    "priority",
    "dependencies",
    "execution_constraints",
    "hold",
    "release_hold",
];

pub fn catalog() -> Registry {
    let mut registry = Registry::default();
    let context = object(&[
        ("view", enumeration(CONTEXT_VIEWS)),
        ("subject", subject()),
        ("snapshot", text()),
        ("page", revision()),
    ]);
    let snapshot = object(&[
        ("reference", text()),
        ("subject", subject()),
        ("snapshot", text()),
        ("complete", Schema::Boolean),
        (
            "next_page",
            Schema::Nullable {
                inner: Box::new(revision()),
            },
        ),
        ("visibility", text()),
        ("observed_at_ms", revision()),
        ("expires_at_ms", revision()),
    ]);
    let work = object(&[
        ("kind", enumeration(WORK_KINDS)),
        ("subject", subject()),
        ("logical_key", text()),
        ("lifetime", enumeration(&["owned", "independent"])),
        ("context", text()),
        ("visibility", text()),
    ]);
    let change = object(&[
        ("variant", enumeration(CHANGE_VARIANTS)),
        ("subject", subject()),
        ("value", text()),
        (
            "hold_id",
            Schema::Nullable {
                inner: Box::new(text()),
            },
        ),
    ]);
    let judgment = object(&[
        ("subject", subject()),
        ("context", text()),
        ("question", text()),
        ("choices", list(text())),
    ]);
    let decision = object(&[
        ("subject", subject()),
        ("question", text()),
        ("choices", list(text())),
        ("responders", list(text())),
        ("expires_at_ms", revision()),
    ]);
    let check = object(&[
        ("subject", subject()),
        (
            "suite",
            enumeration(&["unit", "integration", "documentation", "compatibility"]),
        ),
        ("execution_context", text()),
    ]);
    let prepare = object(&[("sources", list(subject())), ("base", subject())]);
    let publish = object(&[
        ("candidate", subject()),
        ("destination", subject()),
        ("evidence", list(subject())),
    ]);
    let message = object(&[
        ("identity", text()),
        ("recipient", text()),
        ("visibility", text()),
        ("body", text()),
    ]);
    let finding = object(&[
        ("key", text()),
        ("evidence", text()),
        ("expected_revision", revision()),
    ]);
    let findings = object(&[
        ("partition", text()),
        ("observation", text()),
        ("evaluation", text()),
        ("findings", list(finding)),
        ("reminder_ms", revision()),
    ]);
    let handle = object(&[("handle", text())]);
    let definitions: Vec<(&str, Schema, Schema, &[&str], Recovery)> = vec![
        (
            "context.read",
            context,
            snapshot,
            &["observed", "incomplete", "expired", "refused"],
            Recovery::Observation,
        ),
        (
            "work.ensure",
            work,
            result(),
            &["ensured", "refused"],
            Recovery::KeyedIdempotent,
        ),
        (
            "work.change",
            change,
            result(),
            &["changed", "conflict", "refused"],
            Recovery::KeyedIdempotent,
        ),
        (
            "judgment.request",
            judgment,
            result(),
            &["assessed", "refused"],
            Recovery::KeyedIdempotent,
        ),
        (
            "decision.request",
            decision,
            result(),
            &["approved", "rejected", "expired", "refused"],
            Recovery::KeyedIdempotent,
        ),
        (
            "check.request",
            check,
            result(),
            &["passed", "failed", "infrastructure_failure", "refused"],
            Recovery::KeyedIdempotent,
        ),
        (
            "candidate.prepare",
            prepare,
            result(),
            &["prepared", "conflict", "refused"],
            Recovery::KeyedIdempotent,
        ),
        (
            "candidate.publish",
            publish,
            result(),
            &["published", "conflict", "refused"],
            Recovery::Reconcilable,
        ),
        (
            "message.send",
            message,
            result(),
            &["accepted", "refused"],
            Recovery::KeyedIdempotent,
        ),
        (
            "findings.reconcile",
            findings,
            result(),
            &["reconciled", "conflict", "refused"],
            Recovery::KeyedIdempotent,
        ),
        (
            "operation.get",
            handle.clone(),
            result(),
            &[
                "running",
                "succeeded",
                "failed",
                "cancelled",
                "indeterminate",
                "refused",
            ],
            Recovery::Observation,
        ),
        (
            "operation.cancel",
            handle,
            result(),
            &[
                "accepted",
                "refused",
                "too_late",
                "cancelled",
                "indeterminate",
            ],
            Recovery::KeyedIdempotent,
        ),
    ];
    for (name, input, output, outcomes, recovery) in definitions {
        let contract = CommandContract {
            name: name.into(),
            version: "1.0.0".into(),
            input,
            outcomes: outcomes
                .iter()
                .map(|o| ((*o).into(), output.clone()))
                .collect(),
            capabilities: strings(&[name]),
            scopes: strings(&["project"]),
            effect: if recovery == Recovery::Observation {
                Effect::Observation
            } else {
                Effect::Mutation
            },
            recovery,
            asynchronous: None,
            cancellation: false,
            public_result_fields: strings(&["reference", "subject"]),
            sensitive_input_fields: BTreeSet::new(),
            sensitive_output_fields: BTreeSet::new(),
            max_payload_bytes: 65_536,
            deadline_ms: 60_000,
        };
        registry.commands.insert(name.into(), contract);
    }
    for (name, producer) in [
        ("work.changed", "aq.work"),
        ("artifact.recorded", "aq.evidence"),
        ("operation.changed", "aq.operations"),
        ("check.completed", "aq.checks"),
        ("decision.resolved", "aq.decisions"),
        ("integration.changed", "aq.integration"),
        ("signal.reported", "aq.signals"),
    ] {
        registry.events.insert(
            name.into(),
            EventContract {
                sensitive_payload_fields: BTreeSet::new(),
                name: name.into(),
                version: "1.0.0".into(),
                payload: object(&[
                    ("reference", text()),
                    ("subject", subject()),
                    ("outcome", text()),
                ]),
                trusted_producers: strings(&[producer]),
                scopes: strings(&["project"]),
                correlation_fields: strings(&["operation"]),
                subject_field: Some("subject.id".into()),
                revision_field: Some("subject.revision".into()),
                max_payload_bytes: 65_536,
                retention_ms: 86_400_000,
            },
        );
    }
    let work_completion = object(&[
        ("outcome", enumeration(&["completed", "refused"])),
        ("output", work_result()),
    ]);
    let event = registry.events.get_mut("operation.changed").unwrap();
    event.payload = work_completion.clone();
    event.subject_field = Some("output.subject.id".into());
    event.revision_field = Some("output.subject.revision".into());
    let work = registry.commands.get_mut("work.ensure").unwrap();
    work.outcomes.insert(
        "accepted".into(),
        object(&[("handle", Schema::string(1024))]),
    );
    work.outcomes.insert("completed".into(), work_result());
    work.outcomes.insert("refused".into(), work_result());
    work.public_result_fields.clear();
    work.cancellation = true;
    work.asynchronous = Some(AsyncContract {
        completion_event: "operation.changed".into(),
        completion: work_completion,
        correlation_field: "operation".into(),
        reconciliation_command: "operation.get".into(),
    });
    registry
        .commands
        .get_mut("judgment.request")
        .unwrap()
        .outcomes
        .insert(
            "assessed".into(),
            object(&[
                ("reference", text()),
                ("subject", subject()),
                ("choice", text()),
            ]),
        );
    registry
}
pub fn describe_surface() -> serde_json::Value {
    serde_json::json!({"kind":"synthetic_pilot_contracts","commands":catalog().commands.keys().collect::<Vec<_>>(),"context_views":CONTEXT_VIEWS,"work_kinds":WORK_KINDS,"work_change_variants":CHANGE_VARIANTS,"production_adapters":false})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn catalog_is_exact_and_closed() {
        let r = catalog();
        assert_eq!(r.commands.len(), 12);
        assert!(!r.commands.contains_key("run_command"));
        for c in r.commands.values() {
            c.input.validate_definition().unwrap();
            for schema in c.outcomes.values() {
                schema.validate_definition().unwrap();
            }
        }
        let mut value = serde_json::json!({"view":"arbitrary_sql","subject":{"id":"work","revision":1},"snapshot":"scan","page":0});
        assert!(r.commands["context.read"].input.validate(&value).is_err());
        value["view"] = "health_observations".into();
        assert!(r.commands["context.read"].input.validate(&value).is_ok());
    }
}
