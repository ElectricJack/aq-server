use super::*;

fn valid_id(id: &str) -> bool {
    let mut bytes = id.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_lowercase())
        && id.len() <= 63
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}
fn reference(value: &str) -> bool {
    crate::text(value).is_ok()
}

/// Collects problems for one document.
pub(super) struct Report<'a> {
    pub(super) limits: &'a DocumentLimits,
    pub(super) problems: &'a mut Vec<Problem>,
    pub(super) document: String,
}
impl Report<'_> {
    pub(super) fn add(&mut self, field: &str, message: impl Into<String>) {
        self.problems.push(Problem {
            document: self.document.clone(),
            field: field.into(),
            message: message.into(),
        });
    }
    pub(super) fn identity(&mut self, id: &str, name: &str, description: &str) {
        let limits = self.limits;
        if !valid_id(id) {
            self.add(
                "id",
                "must be a lowercase letter followed by up to 62 lowercase letters, digits or `-`",
            );
        }
        if name.trim().is_empty() || name.chars().any(char::is_control) {
            self.add("name", "must be one non-empty line");
        } else if name.len() > limits.max_name_bytes {
            self.add(
                "name",
                format!("is over the limit of {} bytes", limits.max_name_bytes),
            );
        }
        if description.chars().any(char::is_control) {
            self.add("description", "must be one line");
        } else if description.len() > limits.max_description_bytes {
            self.add(
                "description",
                format!(
                    "is over the limit of {} bytes",
                    limits.max_description_bytes
                ),
            );
        }
    }
    pub(super) fn size(&mut self, bytes: usize) {
        let limits = self.limits;
        if bytes > limits.max_document_bytes {
            self.add(
                "",
                format!(
                    "is {bytes} bytes, over the limit of {} bytes",
                    limits.max_document_bytes
                ),
            );
        }
    }
    /// A non-empty list of distinct known harnesses.
    fn harnesses(&mut self, field: &str, harnesses: &[String]) {
        if harnesses.is_empty() {
            self.add(field, "names no harness");
        }
        let mut seen = BTreeSet::new();
        for harness in harnesses {
            if !HARNESSES.contains(&harness.as_str()) {
                self.add(
                    field,
                    format!(
                        "unknown harness `{harness}`; the harnesses are {}",
                        HARNESSES.join(", ")
                    ),
                );
            } else if !seen.insert(harness) {
                self.add(field, format!("lists harness `{harness}` twice"));
            }
        }
    }
}

/// The bound `SelectionPolicy::validate` puts on a policy's output tokens,
/// which a launch takes from its execution profile.
const MAX_OUTPUT_TOKENS: u64 = 1_048_576;

pub(super) fn check_role(r: &mut Report<'_>, role: &Role, execution: &BTreeSet<&str>) {
    if !execution.contains(role.execution.as_str()) {
        r.add(
            "execution",
            format!("no execution profile `{}` exists", role.execution),
        );
    }
    check_operations(r, role);
    if role.role_md.is_empty() {
        r.add("## Role", "is empty");
    }
    for (heading, text) in [("## Role", &role.role_md), ("## Rules", &role.rules_md)] {
        if has_heading(text) {
            r.add(
                heading,
                "contains a `## ` heading line; use `###` inside a section",
            );
        }
    }
}
fn check_operations(r: &mut Report<'_>, role: &Role) {
    let mut seen = BTreeSet::new();
    for operation in &role.operations {
        if !operation_known(operation) {
            r.add(
                "operations",
                format!("`{operation}` is not an AQ operation a role can hold"),
            );
        } else if !seen.insert(operation) {
            r.add("operations", format!("lists `{operation}` twice"));
        }
    }
}
pub(super) fn check_intelligence(
    r: &mut Report<'_>,
    profile: &Intelligence,
    defaults: &SelectionDefaults,
) {
    if profile.candidates.is_empty() {
        r.add("## Candidates", "lists no candidate");
    }
    let mut seen = BTreeSet::new();
    for (index, c) in profile.candidates.iter().enumerate() {
        let field = format!("candidate {}", index + 1);
        check_candidate(r, c, &field);
        if !seen.insert(c.derived_id()) {
            r.add(
                &field,
                format!("repeats `{}` with the same reasoning", c.model),
            );
        }
    }
    // The defaults alone are reported once, for the whole set.
    if !profile.selection.is_empty()
        && let Err(message) = defaults.with(&profile.selection).check()
    {
        r.add("## Selection", message);
    }
}
fn check_candidate(r: &mut Report<'_>, c: &super::super::Candidate, field: &str) {
    for (what, value) in [
        ("provider", &c.provider),
        ("model", &c.model),
        ("reasoning", &c.reasoning),
    ] {
        if !is_token(value) || !reference(value) {
            r.add(field, format!("{what} `{value}` is malformed"));
        }
    }
    r.harnesses(field, &c.harnesses);
    check_candidate_harnesses(r, c, field);
}
fn check_candidate_harnesses(r: &mut Report<'_>, c: &super::super::Candidate, field: &str) {
    for harness in c
        .harnesses
        .iter()
        .filter(|h| HARNESSES.contains(&h.as_str()))
    {
        if !harness_supports(harness, &c.reasoning) {
            r.add(
                field,
                format!(
                    "harness `{harness}` does not accept reasoning `{}`",
                    c.reasoning
                ),
            );
        }
        if !provider_fits_harness(&c.provider, harness) {
            r.add(
                field,
                format!("harness `{harness}` does not run provider `{}`", c.provider),
            );
        }
    }
}
pub(super) fn check_execution(r: &mut Report<'_>, profile: &Execution) {
    r.harnesses("harnesses", &profile.harnesses);
    #[cfg(not(feature = "project_environments"))]
    let environment = Some(&profile.environment);
    #[cfg(feature = "project_environments")]
    let environment = profile.environment.as_ref();
    if environment.is_some_and(|e| !reference(e)) {
        r.add("environment", "must be one non-empty line");
    }
    check_credentials(r, profile);
    check_limits(r, &profile.limits);
}
fn check_credentials(r: &mut Report<'_>, profile: &Execution) {
    for harness in &profile.harnesses {
        if !profile.credentials.contains_key(harness) {
            r.add(
                "## Credentials",
                format!("names no credential for harness `{harness}`"),
            );
        }
    }
    check_credential_references(r, profile);
}
fn check_credential_references(r: &mut Report<'_>, profile: &Execution) {
    for (harness, credential) in &profile.credentials {
        if !profile.harnesses.contains(harness) {
            r.add(
                "## Credentials",
                format!("`{harness}` is not one of this profile's harnesses"),
            );
        }
        if !reference(credential) {
            r.add(
                "## Credentials",
                format!("the credential for `{harness}` must be one non-empty line"),
            );
        }
    }
}
fn check_limits(r: &mut Report<'_>, l: &super::super::Limits) {
    for (name, value) in [
        ("wall_time_ms", l.wall_time_ms),
        ("memory_bytes", l.memory_bytes),
        ("cpu_millis", l.cpu_millis),
        ("process_limit", l.process_limit),
        ("startup_context_bytes", l.startup_context_bytes),
        ("max_output_tokens", l.max_output_tokens),
    ] {
        if value == 0 || value > i64::MAX as u64 {
            r.add("## Limits", format!("`{name}` must be a positive number"));
        }
    }
    if l.max_output_tokens > MAX_OUTPUT_TOKENS {
        r.add(
            "## Limits",
            format!(
                "`max_output_tokens` is over the selection policy's bound of {MAX_OUTPUT_TOKENS}"
            ),
        );
    }
    if l.startup_context_bytes > super::super::MAX_STARTUP_BYTES as u64 {
        r.add(
            "## Limits",
            format!(
                "`startup_context_bytes` is over the renderer's bound of {} bytes",
                super::super::MAX_STARTUP_BYTES
            ),
        );
    }
}
