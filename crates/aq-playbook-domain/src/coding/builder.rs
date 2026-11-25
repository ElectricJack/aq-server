use super::*;

pub(super) struct CompletionWait<'a> {
    pub event: &'a str,
    pub handle: &'a str,
    pub deadline_ms: u64,
    pub bind: &'a str,
    pub on_receive: &'a str,
}

pub(super) struct Builder {
    pub(super) registry: Registry,
}
impl Builder {
    pub(super) fn new() -> Self {
        Self {
            registry: CatalogProfile::Host8.registry(),
        }
    }
    pub(super) fn command(&self, name: &str) -> Result<&CommandContract, CodingError> {
        self.registry
            .commands
            .get(name)
            .ok_or_else(|| CodingError::Invalid(format!("Host8 lacks {name}")))
    }
    pub(super) fn input_field(&self, command: &str, field: &str) -> Result<Schema, CodingError> {
        match &self.command(command)?.input {
            Schema::Object { properties, .. } => properties
                .get(field)
                .cloned()
                .ok_or_else(|| CodingError::Invalid(format!("{command} lacks {field}"))),
            _ => Err(CodingError::Invalid(format!("{command} input"))),
        }
    }
    pub(super) fn subject(&self) -> Result<Schema, CodingError> {
        self.input_field("work.ensure", "subject")
    }
    /// `{reference, subject}`: the uniform child success output.
    pub(super) fn result(&self) -> Result<Schema, CodingError> {
        Ok(Schema::object(BTreeMap::from([
            ("reference".into(), Schema::string(TEXT)),
            ("subject".into(), self.subject()?),
        ])))
    }
    pub(super) fn reference_only() -> Schema {
        Schema::object(BTreeMap::from([("reference".into(), Schema::string(TEXT))]))
    }
    /// Escalation is an empty object so a Call bind stays an object union.
    pub(super) fn escalated() -> Schema {
        Schema::object(BTreeMap::new())
    }
    pub(super) fn invoke(
        c: &CommandContract,
        input: Expr,
        bind: &str,
        success: &[(&str, &str)],
    ) -> Result<Step, CodingError> {
        let fingerprint = c
            .fingerprint()
            .map_err(|e| CodingError::Invalid(e.to_string()))?;
        let mut s = edges(
            Operation::Invoke {
                command: ContractRef {
                    name: c.name.clone(),
                    fingerprint,
                },
                input,
                bind: bind.into(),
                retry: None,
            },
            &[],
        );
        for outcome in c.outcomes.keys() {
            s.edges.insert(outcome.clone(), "failure".into());
        }
        for (a, b) in success {
            s.edges.insert((*a).into(), (*b).into());
        }
        Ok(s)
    }
    pub(super) fn event(&self, name: &str) -> Result<ContractRef, CodingError> {
        let e = self
            .registry
            .events
            .get(name)
            .ok_or_else(|| CodingError::Invalid(format!("Host8 lacks {name}")))?;
        Ok(ContractRef {
            name: name.into(),
            fingerprint: e
                .fingerprint()
                .map_err(|e| CodingError::Invalid(e.to_string()))?,
        })
    }
    pub(super) fn wait(&self, completion: CompletionWait<'_>) -> Result<Step, CodingError> {
        Ok(edges(
            Operation::Wait {
                event: Some(self.event(completion.event)?),
                correlation: BTreeMap::from([(
                    "operation".into(),
                    r(completion.handle, &["handle"]),
                )]),
                minimum_revision: Some(lit(json!(0))),
                deadline_ms: completion.deadline_ms,
                bind: completion.bind.into(),
            },
            &[("received", completion.on_receive), ("timeout", "failure")],
        ))
    }
}
