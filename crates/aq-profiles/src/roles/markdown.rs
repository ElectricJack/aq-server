//! The Markdown form of the three documents: a strict, deterministic parser
//! and the serializer Settings writes a document back with. The grammar is in
//! the crate README. A parser refuses what it does not recognise, so a typo
//! cannot silently drop text or a field.
use super::{Candidate, Execution, Intelligence, Limits, Role, SelectionOverrides};
use serde::{Serialize, de::DeserializeOwned};
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// Why a document was refused. `line` is 1-based; 0 means the whole document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}
impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.line == 0 {
            f.write_str(&self.message)
        } else {
            write!(f, "line {}: {}", self.line, self.message)
        }
    }
}
impl std::error::Error for ParseError {}
type Parsed<T> = Result<T, ParseError>;
fn refuse<T>(line: usize, message: impl Into<String>) -> Parsed<T> {
    Err(ParseError {
        line,
        message: message.into(),
    })
}

const CANDIDATE_COLUMNS: [&str; 4] = ["provider", "model", "reasoning", "harnesses"];

/// A bare word in a list or a table cell: no spaces, commas, quotes, brackets
/// or pipes, so it needs no escaping.
pub(super) fn is_token(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._@/:+-".contains(&b))
}
fn is_fence(line: &str) -> bool {
    let line = line.trim_start();
    line.starts_with("```") || line.starts_with("~~~")
}
fn is_heading(line: &str) -> bool {
    line.starts_with("## ") || line.trim_end() == "##"
}
/// Whether prompt text holds a `## ` heading line outside a fenced block. Such
/// a line would start a new section when the document is read back.
pub(super) fn has_heading(text: &str) -> bool {
    let mut fenced = false;
    text.lines().any(|line| {
        if is_fence(line) {
            fenced = !fenced;
        }
        !fenced && is_heading(line)
    })
}

/// `key: value` entries of the frontmatter or of one list section.
struct Fields<'a> {
    place: String,
    line: usize,
    values: BTreeMap<&'a str, (usize, &'a str)>,
}
impl<'a> Fields<'a> {
    fn new(place: impl Into<String>, line: usize) -> Self {
        Self {
            place: place.into(),
            line,
            values: BTreeMap::new(),
        }
    }
    fn insert(&mut self, line: usize, entry: &'a str, allowed: Option<&[&str]>) -> Parsed<()> {
        let Some((key, value)) = entry.split_once(':') else {
            return refuse(
                line,
                format!("{} expects `key: value`, found `{entry}`", self.place),
            );
        };
        let key = key.trim();
        if !is_token(key) {
            return refuse(line, format!("{} has a malformed key `{key}`", self.place));
        }
        if allowed.is_some_and(|allowed| !allowed.contains(&key)) {
            return refuse(line, format!("unknown {} key `{key}`", self.place));
        }
        if self.values.insert(key, (line, value.trim())).is_some() {
            return refuse(line, format!("duplicate {} key `{key}`", self.place));
        }
        Ok(())
    }
    fn raw(&self, key: &str) -> Parsed<(usize, &'a str)> {
        match self.values.get(key) {
            Some(found) => Ok(*found),
            None => refuse(self.line, format!("{} is missing `{key}`", self.place)),
        }
    }
    fn optional<T>(
        &self,
        key: &str,
        read: impl Fn(usize, &str, &str) -> Parsed<T>,
    ) -> Parsed<Option<T>> {
        match self.values.get(key) {
            Some((line, value)) => read(*line, key, value).map(Some),
            None => Ok(None),
        }
    }
    fn required<T>(&self, key: &str, read: impl Fn(usize, &str, &str) -> Parsed<T>) -> Parsed<T> {
        let (line, value) = self.raw(key)?;
        read(line, key, value)
    }
}
fn text(line: usize, key: &str, value: &str) -> Parsed<String> {
    if value.is_empty() {
        refuse(line, format!("`{key}` is empty"))
    } else if value.starts_with('"') {
        serde_json::from_str(value)
            .or_else(|_| refuse(line, format!("`{key}` has malformed quoted text")))
    } else if value.starts_with('[') {
        refuse(
            line,
            format!("`{key}` expects text, not a list; quote text that starts with `[`"),
        )
    } else {
        Ok(value.into())
    }
}
fn list(line: usize, key: &str, value: &str) -> Parsed<Vec<String>> {
    let Some(inner) = value.strip_prefix('[').and_then(|v| v.strip_suffix(']')) else {
        return refuse(line, format!("`{key}` expects a list such as `[a, b]`"));
    };
    if inner.trim().is_empty() {
        return Ok(Vec::new());
    }
    inner
        .split(',')
        .map(|item| {
            let item = item.trim();
            if is_token(item) {
                Ok(item.to_string())
            } else {
                refuse(line, format!("`{key}` has a malformed list item `{item}`"))
            }
        })
        .collect()
}
fn flag(line: usize, key: &str, value: &str) -> Parsed<bool> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => refuse(line, format!("`{key}` expects `true` or `false`")),
    }
}
fn number(line: usize, key: &str, value: &str) -> Parsed<u64> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return refuse(line, format!("`{key}` expects a whole number"));
    }
    value
        .parse()
        .or_else(|_| refuse(line, format!("`{key}` is too large")))
}
/// A value of a serde enum written by its snake_case or kebab-case name.
fn named<T: DeserializeOwned>(line: usize, key: &str, value: &str) -> Parsed<T> {
    serde_json::from_value(serde_json::Value::String(value.into()))
        .or_else(|_| refuse(line, format!("`{key}` has an unknown value `{value}`")))
}
fn name_of<T: Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(name)) => name,
        _ => String::new(),
    }
}

struct Section<'a> {
    line: usize,
    lines: Vec<(usize, &'a str)>,
}
impl<'a> Section<'a> {
    fn prose(&self) -> String {
        let lines: Vec<_> = self.lines.iter().map(|(_, line)| *line).collect();
        lines.join("\n").trim().to_string()
    }
    /// `- key: value` items; blank lines between them are ignored.
    fn fields(&self, heading: &str, allowed: Option<&[&str]>) -> Parsed<Fields<'a>> {
        let mut fields = Fields::new(format!("`## {heading}`"), self.line);
        for &(line, text) in &self.lines {
            if text.trim().is_empty() {
                continue;
            }
            let Some(entry) = text.trim().strip_prefix("- ") else {
                return refuse(
                    line,
                    format!("`## {heading}` expects `- key: value` items, found `{text}`"),
                );
            };
            fields.insert(line, entry, allowed)?;
        }
        Ok(fields)
    }
}
struct Document<'a> {
    front: Fields<'a>,
    sections: BTreeMap<&'static str, Section<'a>>,
}
impl<'a> Document<'a> {
    fn section(&self, heading: &str) -> Parsed<&Section<'a>> {
        match self.sections.get(heading) {
            Some(section) => Ok(section),
            None => refuse(0, format!("`## {heading}` is missing")),
        }
    }
}
fn split<'a>(
    source: &'a str,
    keys: &[&'static str],
    headings: &[&'static str],
) -> Parsed<Document<'a>> {
    let mut lines = source
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .enumerate()
        .map(|(index, line)| (index + 1, line));
    if lines.next().map(|(_, line)| line.trim_end()) != Some("---") {
        return refuse(1, "a document starts with a `---` line");
    }
    let mut front = Fields::new("frontmatter", 1);
    loop {
        let Some((line, text)) = lines.next() else {
            return refuse(0, "frontmatter is not closed by a `---` line");
        };
        if text.trim_end() == "---" {
            break;
        }
        if !text.trim().is_empty() {
            front.insert(line, text, Some(keys))?;
        }
    }
    let mut sections = BTreeMap::new();
    let mut current: Option<&'static str> = None;
    let mut fenced = false;
    for (line, text) in lines {
        if is_fence(text) {
            fenced = !fenced;
        }
        if !fenced && is_heading(text) {
            let title = text[2..].trim();
            let Some(heading) = headings.iter().find(|h| **h == title) else {
                let allowed: Vec<_> = headings.iter().map(|h| format!("`## {h}`")).collect();
                return refuse(
                    line,
                    format!(
                        "unknown heading `## {title}`; this document takes {}",
                        allowed.join(" and ")
                    ),
                );
            };
            let section = Section {
                line,
                lines: Vec::new(),
            };
            if sections.insert(*heading, section).is_some() {
                return refuse(line, format!("duplicate heading `## {heading}`"));
            }
            current = Some(heading);
        } else if let Some(section) = current.and_then(|heading| sections.get_mut(heading)) {
            section.lines.push((line, text));
        } else if !text.trim().is_empty() {
            return refuse(line, format!("text before the first heading: `{text}`"));
        }
    }
    Ok(Document { front, sections })
}
fn identity(front: &Fields<'_>) -> Parsed<(String, String, String)> {
    Ok((
        front.required("id", text)?,
        front.required("name", text)?,
        front.optional("description", text)?.unwrap_or_default(),
    ))
}

/// Parse a role document (spec 0014 §5.1).
pub fn parse_role(source: &str) -> Parsed<Role> {
    let document = split(
        source,
        &[
            "id",
            "name",
            "description",
            "execution",
            "operations",
            "enabled",
        ],
        &["Role", "Rules"],
    )?;
    let front = &document.front;
    let (id, name, description) = identity(front)?;
    let role = document.section("Role")?;
    let role_md = role.prose();
    if role_md.is_empty() {
        return refuse(role.line, "`## Role` is empty");
    }
    Ok(Role {
        id,
        name,
        description,
        execution: front.required("execution", text)?,
        operations: front.required("operations", list)?,
        enabled: front.optional("enabled", flag)?.unwrap_or(true),
        role_md,
        rules_md: document
            .sections
            .get("Rules")
            .map(Section::prose)
            .unwrap_or_default(),
    })
}

fn cells(line: &str) -> Option<Vec<&str>> {
    let inner = line.trim().strip_prefix('|')?.strip_suffix('|')?;
    Some(inner.split('|').map(str::trim).collect())
}
fn candidates(section: &Section<'_>) -> Parsed<Vec<Candidate>> {
    let header = format!("| {} |", CANDIDATE_COLUMNS.join(" | "));
    let mut rows = section
        .lines
        .iter()
        .filter(|(_, text)| !text.trim().is_empty());
    let starts = rows.next().and_then(|(_, text)| cells(text));
    if starts.as_deref() != Some(&CANDIDATE_COLUMNS[..]) {
        return refuse(
            section.line,
            format!("`## Candidates` expects a table that starts with `{header}`"),
        );
    }
    let ruled = rows.next().and_then(|(_, text)| cells(text));
    let rule = |cell: &&str| {
        let dashes = cell.trim_matches(':');
        !dashes.is_empty() && dashes.bytes().all(|b| b == b'-')
    };
    if !ruled.is_some_and(|cells| cells.len() == 4 && cells.iter().all(rule)) {
        return refuse(
            section.line,
            "`## Candidates` expects `|---|---|---|---|` under the table header",
        );
    }
    let mut out = Vec::new();
    for (line, text) in rows {
        let Some(row) = cells(text).filter(|row| row.len() == 4) else {
            return refuse(
                *line,
                format!("a candidate row has four cells, as `{header}`; found `{text}`"),
            );
        };
        for (column, cell) in CANDIDATE_COLUMNS.iter().zip(&row).take(3) {
            if !is_token(cell) {
                return refuse(
                    *line,
                    format!("candidate `{column}` is malformed: `{cell}`"),
                );
            }
        }
        out.push(Candidate {
            provider: row[0].into(),
            model: row[1].into(),
            reasoning: row[2].into(),
            harnesses: list(*line, "harnesses", &format!("[{}]", row[3]))?,
        });
    }
    Ok(out)
}
const SELECTION_KEYS: [&str; 10] = [
    "max_attempts",
    "fallback_on",
    "max_lineage_elapsed_ms",
    "observation_max_age_ms",
    "allow_unknown_availability",
    "unknown_cost",
    "require_complete_accounting",
    "require_hard_cost_enforcement",
    "max_cost_microunits",
    "currency",
];
/// The word that states "no cost limit" for the two cost keys.
const NONE: &str = "none";
fn selection(section: &Section<'_>) -> Parsed<SelectionOverrides> {
    let fields = section.fields("Selection", Some(&SELECTION_KEYS))?;
    let names = |line: usize, key: &str, value: &str| {
        list(line, key, value)?
            .iter()
            .map(|item| named(line, key, item))
            .collect::<Parsed<Vec<_>>>()
    };
    let attempts = |line: usize, key: &str, value: &str| {
        u32::try_from(number(line, key, value)?)
            .or_else(|_| refuse(line, format!("`{key}` is too large")))
    };
    let cost = |line: usize, key: &str, value: &str| match value {
        NONE => Ok(None),
        _ => number(line, key, value).map(Some),
    };
    let currency = |line: usize, key: &str, value: &str| match value {
        NONE => Ok(None),
        _ => text(line, key, value).map(Some),
    };
    Ok(SelectionOverrides {
        max_attempts: fields.optional("max_attempts", attempts)?,
        fallback_on: fields.optional("fallback_on", names)?,
        max_lineage_elapsed_ms: fields.optional("max_lineage_elapsed_ms", number)?,
        observation_max_age_ms: fields.optional("observation_max_age_ms", number)?,
        allow_unknown_availability: fields.optional("allow_unknown_availability", flag)?,
        unknown_cost: fields.optional("unknown_cost", named)?,
        require_complete_accounting: fields.optional("require_complete_accounting", flag)?,
        require_hard_cost_enforcement: fields.optional("require_hard_cost_enforcement", flag)?,
        max_cost_microunits: fields.optional("max_cost_microunits", cost)?,
        currency: fields.optional("currency", currency)?,
    })
}
/// Parse an intelligence profile document (spec 0014 §5.2).
pub fn parse_intelligence(source: &str) -> Parsed<Intelligence> {
    let document = split(
        source,
        &["id", "name", "description"],
        &["Candidates", "Selection"],
    )?;
    let (id, name, description) = identity(&document.front)?;
    Ok(Intelligence {
        id,
        name,
        description,
        candidates: candidates(document.section("Candidates")?)?,
        selection: match document.sections.get("Selection") {
            Some(section) => selection(section)?,
            None => SelectionOverrides::default(),
        },
    })
}

const LIMIT_KEYS: [&str; 6] = [
    "wall_time_ms",
    "memory_bytes",
    "cpu_millis",
    "process_limit",
    "startup_context_bytes",
    "max_output_tokens",
];
/// Parse an execution profile document (spec 0014 §5.3).
pub fn parse_execution(source: &str) -> Parsed<Execution> {
    let document = split(
        source,
        &[
            "id",
            "name",
            "description",
            "harnesses",
            "environment",
            "confinement",
        ],
        &["Credentials", "Limits"],
    )?;
    let front = &document.front;
    let (id, name, description) = identity(front)?;
    let mut credentials = BTreeMap::new();
    if let Some(section) = document.sections.get("Credentials") {
        let fields = section.fields("Credentials", None)?;
        for (harness, (line, value)) in &fields.values {
            credentials.insert(harness.to_string(), text(*line, harness, value)?);
        }
    }
    let limits = document
        .section("Limits")?
        .fields("Limits", Some(&LIMIT_KEYS))?;
    Ok(Execution {
        id,
        name,
        description,
        harnesses: front.required("harnesses", list)?,
        environment: front.required("environment", text)?,
        confinement: front.required("confinement", named)?,
        credentials,
        limits: Limits {
            wall_time_ms: limits.required("wall_time_ms", number)?,
            memory_bytes: limits.required("memory_bytes", number)?,
            cpu_millis: limits.required("cpu_millis", number)?,
            process_limit: limits.required("process_limit", number)?,
            startup_context_bytes: limits.required("startup_context_bytes", number)?,
            max_output_tokens: limits.required("max_output_tokens", number)?,
        },
    })
}

/// Text after `key: `: bare when it reads back unchanged, else a JSON string.
fn scalar(value: &str) -> String {
    let bare = !value.is_empty()
        && value == value.trim()
        && !value.starts_with(['"', '['])
        && !value.chars().any(char::is_control);
    if bare {
        value.into()
    } else {
        serde_json::Value::String(value.into()).to_string()
    }
}
fn front(id: &str, name: &str, description: &str) -> String {
    let mut out = format!("---\nid: {}\nname: {}\n", scalar(id), scalar(name));
    if !description.is_empty() {
        let _ = writeln!(out, "description: {}", scalar(description));
    }
    out
}
impl Role {
    /// The document [`parse_role`] reads back as this role.
    pub fn to_markdown(&self) -> String {
        let mut out = front(&self.id, &self.name, &self.description);
        let _ = write!(
            out,
            "execution: {}\noperations: [{}]\nenabled: {}\n---\n## Role\n{}\n",
            scalar(&self.execution),
            self.operations.join(", "),
            self.enabled,
            self.role_md
        );
        if !self.rules_md.is_empty() {
            let _ = write!(out, "\n## Rules\n{}\n", self.rules_md);
        }
        out
    }
}
impl Intelligence {
    /// The document [`parse_intelligence`] reads back as this profile.
    pub fn to_markdown(&self) -> String {
        let mut out = front(&self.id, &self.name, &self.description);
        let _ = write!(
            out,
            "---\n## Candidates\n| {} |\n|---|---|---|---|\n",
            CANDIDATE_COLUMNS.join(" | ")
        );
        for c in &self.candidates {
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} |",
                c.provider,
                c.model,
                c.reasoning,
                c.harnesses.join(", ")
            );
        }
        let s = &self.selection;
        if s.is_empty() {
            return out;
        }
        out.push_str("\n## Selection\n");
        let mut item = |key: &str, value: Option<String>| {
            if let Some(value) = value {
                let _ = writeln!(out, "- {key}: {value}");
            }
        };
        let number = |n: &Option<u64>| n.map(|n| n.to_string());
        let flag = |b: &Option<bool>| b.map(|b| b.to_string());
        item("max_attempts", s.max_attempts.map(|n| n.to_string()));
        item(
            "fallback_on",
            s.fallback_on.as_ref().map(|reasons| {
                let names: Vec<_> = reasons.iter().map(name_of).collect();
                format!("[{}]", names.join(", "))
            }),
        );
        item("max_lineage_elapsed_ms", number(&s.max_lineage_elapsed_ms));
        item("observation_max_age_ms", number(&s.observation_max_age_ms));
        item(
            "allow_unknown_availability",
            flag(&s.allow_unknown_availability),
        );
        item("unknown_cost", s.unknown_cost.as_ref().map(name_of));
        item(
            "require_complete_accounting",
            flag(&s.require_complete_accounting),
        );
        item(
            "require_hard_cost_enforcement",
            flag(&s.require_hard_cost_enforcement),
        );
        item(
            "max_cost_microunits",
            s.max_cost_microunits
                .map(|cost| cost.map_or(NONE.into(), |cost| cost.to_string())),
        );
        item(
            "currency",
            s.currency
                .as_ref()
                .map(|c| c.as_deref().map_or(NONE.into(), scalar)),
        );
        out
    }
}
impl Execution {
    /// The document [`parse_execution`] reads back as this profile.
    pub fn to_markdown(&self) -> String {
        let mut out = front(&self.id, &self.name, &self.description);
        let _ = write!(
            out,
            "harnesses: [{}]\nenvironment: {}\nconfinement: {}\n---\n",
            self.harnesses.join(", "),
            scalar(&self.environment),
            self.confinement.name()
        );
        if !self.credentials.is_empty() {
            out.push_str("## Credentials\n");
            for (harness, credential) in &self.credentials {
                let _ = writeln!(out, "- {harness}: {}", scalar(credential));
            }
            out.push('\n');
        }
        let l = &self.limits;
        let _ = write!(
            out,
            "## Limits\n- wall_time_ms: {}\n- memory_bytes: {}\n- cpu_millis: {}\n- process_limit: {}\n- startup_context_bytes: {}\n- max_output_tokens: {}\n",
            l.wall_time_ms,
            l.memory_bytes,
            l.cpu_millis,
            l.process_limit,
            l.startup_context_bytes,
            l.max_output_tokens
        );
        out
    }
}
