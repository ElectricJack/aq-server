//! Validation of all documents together and the snapshot they compile into.
//! Problems are collected, never first-error-only, and each names its
//! document and field.
use super::markdown::{has_heading, is_token};
use super::{
    Execution, HARNESSES, Intelligence, Kind, Role, SelectionDefaults, harness_supports,
    operation_known, parse_execution, parse_intelligence, parse_role, provider_fits_harness,
};
use aq_session_protocol::Digest;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// The size limits a caller applies. They are Settings, not constants: the
/// caller passes them in, and `Default` gives the values the catalog used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentLimits {
    /// One document's Markdown bytes.
    pub max_document_bytes: usize,
    /// All documents' Markdown bytes together.
    pub max_set_bytes: usize,
    /// Documents of one kind.
    pub max_documents_per_kind: usize,
    pub max_name_bytes: usize,
    pub max_description_bytes: usize,
}
impl Default for DocumentLimits {
    fn default() -> Self {
        Self {
            max_document_bytes: 65_536,
            max_set_bytes: 1_048_576,
            max_documents_per_kind: 128,
            max_name_bytes: 128,
            max_description_bytes: 2000,
        }
    }
}

/// One document as stored: its kind, a label for messages (a file name or the
/// id Settings stores it under) and its Markdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceDocument {
    pub kind: Kind,
    pub label: String,
    pub text: String,
}

/// One thing wrong with a set of documents.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Problem {
    /// For example ``role `developer` ``.
    pub document: String,
    /// The field or section at fault; empty when it is the whole document.
    pub field: String,
    pub message: String,
}
impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.field.is_empty() {
            write!(f, "{}: {}", self.document, self.message)
        } else {
            write!(f, "{}: {}: {}", self.document, self.field, self.message)
        }
    }
}

/// The parsed documents before validation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DocumentSet {
    pub roles: Vec<Role>,
    pub intelligence: Vec<Intelligence>,
    pub execution: Vec<Execution>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Content {
    schema_version: u32,
    selection_defaults: SelectionDefaults,
    roles: BTreeMap<String, Role>,
    intelligence: BTreeMap<String, Intelligence>,
    execution: BTreeMap<String, Execution>,
}

/// A validated set of roles and profiles, immutable and identified by the
/// digest of its canonical bytes. Entries carry no version; an attempt pins
/// the snapshot digest it launched with (spec 0014 §7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    content: Content,
    bytes: Vec<u8>,
    digest: String,
}
impl Snapshot {
    pub fn digest(&self) -> &str {
        &self.digest
    }
    /// Canonical JSON: the bytes the digest is taken over and a store keeps.
    pub fn canonical_bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn role(&self, id: &str) -> Option<&Role> {
        self.content.roles.get(id)
    }
    pub fn intelligence(&self, id: &str) -> Option<&Intelligence> {
        self.content.intelligence.get(id)
    }
    pub fn execution(&self, id: &str) -> Option<&Execution> {
        self.content.execution.get(id)
    }
    /// The selection defaults this set was compiled with.
    pub fn selection_defaults(&self) -> &SelectionDefaults {
        &self.content.selection_defaults
    }
    /// Roles in id order.
    pub fn roles(&self) -> impl Iterator<Item = &Role> {
        self.content.roles.values()
    }
    /// Intelligence profiles in id order.
    pub fn intelligence_profiles(&self) -> impl Iterator<Item = &Intelligence> {
        self.content.intelligence.values()
    }
    /// Execution profiles in id order.
    pub fn execution_profiles(&self) -> impl Iterator<Item = &Execution> {
        self.content.execution.values()
    }
    /// Read a snapshot as it was stored. The digest is taken over `bytes` and
    /// the content is deserialized; today's document validation is not run,
    /// so a later tightening of a rule cannot make a snapshot an attempt pins
    /// unreadable. The caller compares [`Snapshot::digest`] with the digest
    /// the bytes were stored under.
    pub fn from_stored_bytes(bytes: &[u8]) -> Result<Self, String> {
        let unreadable = |e: serde_json::Error| format!("is not readable: {e}");
        // A snapshot stored before the lineage time budgets were removed
        // still has their keys; they are left out of what is read.
        let mut stored: serde_json::Value = serde_json::from_slice(bytes).map_err(unreadable)?;
        if let Some(defaults) = stored.get_mut("selection_defaults") {
            super::drop_removed_keys(defaults);
        }
        if let Some(profiles) = stored
            .get_mut("intelligence")
            .and_then(|profiles| profiles.as_object_mut())
        {
            for selection in profiles.values_mut().filter_map(|p| p.get_mut("selection")) {
                super::drop_removed_keys(selection);
            }
        }
        let content: Content = serde_json::from_value(stored).map_err(unreadable)?;
        if content.schema_version != 1 {
            return Err("has an unknown schema version".into());
        }
        Ok(Self {
            digest: Digest::of(bytes).as_str().into(),
            content,
            bytes: bytes.to_vec(),
        })
    }
    /// Read canonical bytes back and validate them as today's rules would.
    /// The bytes must be exactly the canonical bytes of a set that is valid
    /// under `limits`. A store reads with [`Snapshot::from_stored_bytes`].
    pub fn from_canonical_bytes(
        bytes: &[u8],
        limits: &DocumentLimits,
    ) -> Result<Self, Vec<Problem>> {
        let stored = |message: String| {
            vec![Problem {
                document: "snapshot".into(),
                field: String::new(),
                message,
            }]
        };
        let content: Content =
            serde_json::from_slice(bytes).map_err(|e| stored(format!("is not readable: {e}")))?;
        if content.schema_version != 1 {
            return Err(stored("has an unknown schema version".into()));
        }
        let snapshot = validate(
            DocumentSet {
                roles: content.roles.into_values().collect(),
                intelligence: content.intelligence.into_values().collect(),
                execution: content.execution.into_values().collect(),
            },
            &content.selection_defaults,
            limits,
        )?;
        if snapshot.bytes != bytes {
            return Err(stored("bytes are not canonical".into()));
        }
        Ok(snapshot)
    }
}

/// Parse and validate all documents together (spec 0014 §7) under the
/// selection `defaults` Settings holds. Returns the snapshot, or every
/// problem found.
pub fn compile(
    sources: &[SourceDocument],
    defaults: &SelectionDefaults,
    limits: &DocumentLimits,
) -> Result<Snapshot, Vec<Problem>> {
    let mut problems = Vec::new();
    let mut set = DocumentSet::default();
    let mut total = 0usize;
    for source in sources {
        let document = format!("{} document `{}`", source.kind.label(), source.label);
        total = total.saturating_add(source.text.len());
        if source.text.len() > limits.max_document_bytes {
            problems.push(Problem {
                document,
                field: String::new(),
                message: format!(
                    "is {} bytes, over the limit of {} bytes",
                    source.text.len(),
                    limits.max_document_bytes
                ),
            });
            continue;
        }
        let parsed = match source.kind {
            Kind::Role => parse_role(&source.text).map(|d| set.roles.push(d)),
            Kind::Intelligence => {
                parse_intelligence(&source.text).map(|d| set.intelligence.push(d))
            }
            Kind::Execution => parse_execution(&source.text).map(|d| set.execution.push(d)),
        };
        if let Err(error) = parsed {
            problems.push(Problem {
                document,
                field: String::new(),
                message: error.to_string(),
            });
        }
    }
    if total > limits.max_set_bytes {
        problems.push(set_too_large(total, limits));
    }
    match check(set, defaults, limits, false) {
        Ok(snapshot) if problems.is_empty() => Ok(snapshot),
        Ok(_) => Err(problems),
        Err(more) => {
            problems.extend(more);
            Err(problems)
        }
    }
}
/// Validate parsed documents together. Returns the snapshot, or every problem
/// found.
pub fn validate(
    set: DocumentSet,
    defaults: &SelectionDefaults,
    limits: &DocumentLimits,
) -> Result<Snapshot, Vec<Problem>> {
    check(set, defaults, limits, true)
}

fn set_too_large(total: usize, limits: &DocumentLimits) -> Problem {
    Problem {
        document: "all documents".into(),
        field: String::new(),
        message: format!(
            "are {total} bytes together, over the limit of {} bytes",
            limits.max_set_bytes
        ),
    }
}
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
struct Report<'a> {
    problems: &'a mut Vec<Problem>,
    document: String,
}
impl Report<'_> {
    fn add(&mut self, field: &str, message: impl Into<String>) {
        self.problems.push(Problem {
            document: self.document.clone(),
            field: field.into(),
            message: message.into(),
        });
    }
    fn identity(&mut self, id: &str, name: &str, description: &str, limits: &DocumentLimits) {
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
    fn size(&mut self, bytes: usize, limits: &DocumentLimits) {
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

fn check_role(r: &mut Report<'_>, role: &Role, execution: &BTreeSet<&str>) {
    if !execution.contains(role.execution.as_str()) {
        r.add(
            "execution",
            format!("no execution profile `{}` exists", role.execution),
        );
    }
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
fn check_intelligence(r: &mut Report<'_>, profile: &Intelligence, defaults: &SelectionDefaults) {
    if profile.candidates.is_empty() {
        r.add("## Candidates", "lists no candidate");
    }
    let mut seen = BTreeSet::new();
    for (index, c) in profile.candidates.iter().enumerate() {
        let field = format!("candidate {}", index + 1);
        for (what, value) in [
            ("provider", &c.provider),
            ("model", &c.model),
            ("reasoning", &c.reasoning),
        ] {
            if !is_token(value) || !reference(value) {
                r.add(&field, format!("{what} `{value}` is malformed"));
            }
        }
        r.harnesses(&field, &c.harnesses);
        for harness in c
            .harnesses
            .iter()
            .filter(|h| HARNESSES.contains(&h.as_str()))
        {
            if !harness_supports(harness, &c.reasoning) {
                r.add(
                    &field,
                    format!(
                        "harness `{harness}` does not accept reasoning `{}`",
                        c.reasoning
                    ),
                );
            }
            if !provider_fits_harness(&c.provider, harness) {
                r.add(
                    &field,
                    format!("harness `{harness}` does not run provider `{}`", c.provider),
                );
            }
        }
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
fn check_execution(r: &mut Report<'_>, profile: &Execution) {
    r.harnesses("harnesses", &profile.harnesses);
    #[cfg(not(feature = "project_environments"))]
    let environment = Some(&profile.environment);
    #[cfg(feature = "project_environments")]
    let environment = profile.environment.as_ref();
    if environment.is_some_and(|e| !reference(e)) {
        r.add("environment", "must be one non-empty line");
    }
    for harness in &profile.harnesses {
        if !profile.credentials.contains_key(harness) {
            r.add(
                "## Credentials",
                format!("names no credential for harness `{harness}`"),
            );
        }
    }
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
    let l = &profile.limits;
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
    if l.startup_context_bytes > super::MAX_STARTUP_BYTES as u64 {
        r.add(
            "## Limits",
            format!(
                "`startup_context_bytes` is over the renderer's bound of {} bytes",
                super::MAX_STARTUP_BYTES
            ),
        );
    }
}

fn check(
    mut set: DocumentSet,
    defaults: &SelectionDefaults,
    limits: &DocumentLimits,
    sizes: bool,
) -> Result<Snapshot, Vec<Problem>> {
    let mut problems = Vec::new();
    if let Err(message) = defaults.check() {
        problems.push(Problem {
            document: "selection defaults".into(),
            field: String::new(),
            message,
        });
    }
    // A profile is stored with only the keys that differ from the defaults.
    for profile in &mut set.intelligence {
        profile.selection = std::mem::take(&mut profile.selection).differing(defaults);
    }
    // Prompt text is stored as the parser reads it: LF lines, no outer blank lines.
    for role in &mut set.roles {
        for text in [&mut role.role_md, &mut role.rules_md] {
            *text = text.replace("\r\n", "\n").trim().to_string();
        }
    }
    for (kind, count) in [
        (Kind::Role, set.roles.len()),
        (Kind::Intelligence, set.intelligence.len()),
        (Kind::Execution, set.execution.len()),
    ] {
        if count > limits.max_documents_per_kind {
            problems.push(Problem {
                document: "all documents".into(),
                field: String::new(),
                message: format!(
                    "hold {count} {} documents, over the limit of {}",
                    kind.label(),
                    limits.max_documents_per_kind
                ),
            });
        }
    }
    let execution_ids: BTreeSet<&str> = set.execution.iter().map(|e| e.id.as_str()).collect();
    let mut total = 0usize;
    let mut content = Content {
        schema_version: 1,
        selection_defaults: defaults.clone(),
        roles: BTreeMap::new(),
        intelligence: BTreeMap::new(),
        execution: BTreeMap::new(),
    };
    macro_rules! each {
        ($list:expr, $kind:expr, $map:ident, |$r:ident, $d:ident| $body:expr) => {
            for $d in &$list {
                let mut $r = Report {
                    problems: &mut problems,
                    document: format!("{} `{}`", $kind.label(), $d.id),
                };
                $r.identity(&$d.id, &$d.name, &$d.description, limits);
                let bytes = $d.to_markdown().len();
                total = total.saturating_add(bytes);
                if sizes {
                    $r.size(bytes, limits);
                }
                $body;
                if content.$map.insert($d.id.clone(), $d.clone()).is_some() {
                    $r.add("id", "is used by more than one document of this kind");
                }
            }
        };
    }
    each!(set.roles, Kind::Role, roles, |r, d| check_role(
        &mut r,
        d,
        &execution_ids
    ));
    each!(
        set.intelligence,
        Kind::Intelligence,
        intelligence,
        |r, d| check_intelligence(&mut r, d, defaults)
    );
    each!(set.execution, Kind::Execution, execution, |r, d| {
        check_execution(&mut r, d)
    });
    if sizes && total > limits.max_set_bytes {
        problems.push(set_too_large(total, limits));
    }
    if !set.roles.iter().any(|role| role.enabled) {
        problems.push(Problem {
            document: "all documents".into(),
            field: String::new(),
            message: "hold no enabled role".into(),
        });
    }
    if !problems.is_empty() {
        return Err(problems);
    }
    let mut value = serde_json::to_value(&content).map_err(|e| unreadable(&e))?;
    value.sort_all_objects();
    let bytes = serde_json::to_vec(&value).map_err(|e| unreadable(&e))?;
    Ok(Snapshot {
        digest: Digest::of(&bytes).as_str().into(),
        content,
        bytes,
    })
}
fn unreadable(error: &serde_json::Error) -> Vec<Problem> {
    vec![Problem {
        document: "snapshot".into(),
        field: String::new(),
        message: format!("cannot be written: {error}"),
    }]
}
