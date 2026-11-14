//! A draft workspace: an agent writes a playbook with contract names, and the
//! tool fills in everything the agent cannot compute by hand.
//!
//! A workspace is a directory of five files:
//!
//! | File | Provided by | Content |
//! | --- | --- | --- |
//! | `source.md` | the owner | the document, without any header |
//! | `record.json` | the host | [`SourceRecord`]: id, binding, scope and what the source may use |
//! | `catalog.json` | the host | the [`Registry`] identities come from |
//! | `facts.json` | the host | named fixed facts, captured as definitions |
//! | `draft.json` | the agent | the playbook, with contract names |
//!
//! `draft.json` is a [`Playbook`] with these differences: an invoke's
//! `command`, a wait's `event` and a trigger's `event` are contract names
//! (strings); `id`, `scope` and `required_capabilities` may be left out (the
//! tool writes them from `record.json` and the commands used, and refuses a
//! value that differs); `purpose` defaults to the record's; `limits` defaults
//! to [`Limits::default`]; and `source_map` maps a step name to a line of
//! `source.md`. Any other unknown field is refused where it is written. A
//! draft has at most [`MAX_DRAFT_STEPS`] steps and [`MAX_DRAFT_JSON_BYTES`]
//! bytes.
//!
//! [`fill`] resolves the names against the catalog, restricted to the names
//! the record allows, adds each asynchronous command's reconciliation command
//! and completion event, writes the `---aq-playbook` header, and captures the
//! facts. Identities come only from the catalog: a draft cannot carry a
//! fingerprint, a registry or a header of its own.
//!
//! A sealed artifact is only as trustworthy as the record, catalog, facts and
//! Markdown it was filled from. In an agent's workspace those files are
//! writable by the agent, so a host re-fills from its own copies and takes
//! only the draft from an agent.
use crate::{
    COMPILER_VERSION, ChangeKind, CompileInput, Diagnostic, Proposal, SemanticDiff, compile,
    semantic_diff, validate,
};
use aq_playbooks_model::*;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;

pub const SOURCE_FILE: &str = "source.md";
pub const RECORD_FILE: &str = "record.json";
pub const CATALOG_FILE: &str = "catalog.json";
pub const FACTS_FILE: &str = "facts.json";
pub const DRAFT_FILE: &str = "draft.json";
const MAX_SOURCE_BYTES: u64 = 4_194_304;
const MAX_RECORD_BYTES: u64 = 65_536;
const MAX_CATALOG_BYTES: u64 = 16_777_216;
const MAX_FACTS_BYTES: u64 = 1_048_576;
const MAX_DRAFT_BYTES: u64 = 4_194_304;
/// A draft may have at most this many steps. Checking a draft costs time and
/// memory that grow faster than its size, and it runs on the host at save.
pub const MAX_DRAFT_STEPS: usize = 256;
/// A draft (the `draft.json` file, or the draft serialized) may be at most
/// this many bytes: room for pretty-printed JSON, and the most at which the
/// worst draft found (a literal of one-field rows, about 2 KB a row to
/// verify) stays inside the 256 MB budget.
pub const MAX_DRAFT_JSON_BYTES: usize = 786_432;

/// What the host keeps about a source; the owner writes only the Markdown.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceRecord {
    /// The source's own id, such as `routing`.
    pub id: String,
    /// The playbook id the artifact is activated as, such as `aq.routing`.
    pub binding: String,
    pub scope: String,
    pub purpose: String,
    /// Command and event names the playbook may use.
    pub commands: BTreeSet<String>,
    pub events: BTreeSet<String>,
    /// Capabilities the commands it uses may require.
    pub capabilities: BTreeSet<String>,
    /// The version every captured fact is recorded with.
    pub facts_version: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Workspace {
    pub markdown: String,
    pub record: SourceRecord,
    pub catalog: Registry,
    pub facts: BTreeMap<String, Value>,
    pub draft: Value,
}

// `draft.json`: the model's playbook shape with names in the three contract
// positions. Every nested type is the model's own, so it stays as strict.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Draft {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    required_capabilities: Option<BTreeSet<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    purpose: Option<String>,
    entries: BTreeMap<String, EntryPoint>,
    steps: BTreeMap<String, DraftStep>,
    triggers: Vec<DraftTrigger>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    limits: Option<Limits>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    source_map: BTreeMap<String, u32>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DraftStep {
    op: DraftOperation,
    edges: BTreeMap<String, String>,
    on_error: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DraftTrigger {
    id: String,
    event: String,
    entry: String,
    input: Expr,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum DraftOperation {
    Invoke {
        command: String,
        input: Expr,
        bind: String,
        retry: Option<RetryPolicy>,
    },
    Decide {
        predicate: Expr,
    },
    Wait {
        event: Option<String>,
        correlation: BTreeMap<String, Expr>,
        minimum_revision: Option<Expr>,
        deadline_ms: u64,
        bind: String,
    },
    ForEach {
        collection: Expr,
        item: String,
        body: String,
        next: String,
        max_items: usize,
        concurrency: u32,
        on_failure: IterationFailure,
    },
    Call {
        dependency: String,
        entry: String,
        input: Expr,
        mode: CallMode,
        bind: String,
    },
    Join {
        members: Vec<Expr>,
        #[serde(default)]
        member_keys: Vec<String>,
        mode: JoinMode,
        accepted_outcomes: BTreeSet<String>,
        deadline_ms: u64,
        remaining: RemainingPolicy,
        bind: String,
    },
    Return {
        outcome: String,
        output: Expr,
    },
}

fn at(file: &str, path: &str) -> String {
    if path.is_empty() {
        file.into()
    } else {
        format!("{file}:{path}")
    }
}

/// One step of a JSON path: an object key or an array index.
#[derive(Debug, Clone, PartialEq)]
enum Seg {
    Key(String),
    Index(usize),
}
/// `steps.read.op`, `triggers[0].event`.
fn render(path: &[Seg]) -> String {
    let mut out = String::new();
    for seg in path {
        match seg {
            Seg::Key(key) => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(key);
            }
            Seg::Index(i) => out.push_str(&format!("[{i}]")),
        }
    }
    out
}
enum Frame {
    Object(Option<String>),
    Array(usize),
}
fn segments(stack: &[Frame]) -> Vec<Seg> {
    stack
        .iter()
        .filter_map(|frame| match frame {
            Frame::Object(Some(key)) => Some(Seg::Key(key.clone())),
            Frame::Array(i) => Some(Seg::Index(*i)),
            Frame::Object(None) => None,
        })
        .collect()
}
/// Walks `text` up to byte `stop`, or until the value at `target` starts.
/// Returns the path of the value being read at the stop, and the offset of
/// the first byte of the value at `target`. An error serde reports once a
/// whole object was read (it reads a tagged operation or expression whole
/// before checking it) names the object just closed.
fn scan(text: &str, stop: usize, target: Option<&[Seg]>) -> (Vec<Seg>, Option<usize>) {
    let mut stack: Vec<Frame> = vec![];
    let mut closed: Option<Vec<Seg>> = None;
    let mut previous: Option<u8> = None;
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() && i < stop {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let key = b == b'"' && matches!(stack.last(), Some(Frame::Object(None)));
        let value = !key
            && !matches!(b, b'}' | b']' | b',' | b':')
            && matches!(previous, None | Some(b':' | b'[' | b','));
        if value && target.is_some_and(|t| segments(&stack) == t) {
            return (segments(&stack), Some(i));
        }
        match b {
            b'{' => stack.push(Frame::Object(None)),
            b'[' => stack.push(Frame::Array(0)),
            b'}' | b']' => {
                closed = Some(segments(&stack[..stack.len().saturating_sub(1)]));
                stack.pop();
            }
            b',' => match stack.last_mut() {
                Some(Frame::Object(key)) => *key = None,
                Some(Frame::Array(index)) => *index += 1,
                None => {}
            },
            b'"' => {
                let start = i + 1;
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
                if key && let Some(Frame::Object(slot)) = stack.last_mut() {
                    let raw = &text[start..i.min(bytes.len())];
                    *slot = Some(serde_json::from_str(&format!("\"{raw}\"")).unwrap_or(raw.into()));
                }
            }
            _ => {}
        }
        if !matches!(b, b'}' | b']' | b',') {
            closed = None;
        }
        previous = Some(b);
        i += 1;
    }
    (closed.unwrap_or_else(|| segments(&stack)), None)
}
#[cfg(test)]
fn json_path_at(text: &str, offset: usize) -> String {
    render(&scan(text, offset, None).0)
}
/// The value at `path` inside `value`.
fn at_path<'a>(value: &'a Value, path: &[Seg]) -> Option<&'a Value> {
    path.iter().try_fold(value, |v, seg| match seg {
        Seg::Key(key) => v.get(key),
        Seg::Index(i) => v.get(i),
    })
}
/// For a tagged object (an operation, expression or schema): `Some(None)`
/// when it parses as one, `Some(Some(error))` when it does not. `None` when
/// `value` is not tagged.
fn tagged_error(value: &Value) -> Option<Option<String>> {
    let tag = |key: &str| value.get(key).is_some_and(Value::is_string);
    let error = if tag("op") {
        serde_json::from_value::<Expr>(value.clone()).err()
    } else if tag("kind") {
        serde_json::from_value::<DraftOperation>(value.clone()).err()
    } else if tag("type") {
        serde_json::from_value::<Schema>(value.clone()).err()
    } else {
        return None;
    };
    Some(error.map(|e| e.to_string()))
}
/// Extends `path` to the deepest tagged object under `value` whose own parse
/// error is `reason`, serde's message, and returns whether it found one.
/// Data is never entered: a literal's `value` and a lookup case's `when`
/// hold the owner's values, which may look like anything.
fn deepest_fault(value: &Value, reason: &str, path: &mut Vec<Seg>) -> bool {
    let literal = value.get("op").and_then(Value::as_str) == Some("literal");
    let case = value.get("when").is_some() && value.get("then").is_some();
    let children: Vec<(Seg, &Value)> = match value {
        Value::Object(map) => map
            .iter()
            .filter(|(k, _)| {
                let data = (literal && *k == "value") || (case && *k == "when");
                !data
            })
            .map(|(k, v)| (Seg::Key(k.clone()), v))
            .collect(),
        Value::Array(items) => items
            .iter()
            .enumerate()
            .map(|(i, v)| (Seg::Index(i), v))
            .collect(),
        _ => return false,
    };
    for (seg, child) in children {
        path.push(seg);
        let own = tagged_error(child);
        // A tagged object that parses holds no fault.
        if own != Some(None) && deepest_fault(child, reason, path) {
            return true;
        }
        if matches!(&own, Some(Some(error)) if error == reason) {
            return true;
        }
        path.pop();
    }
    false
}
/// 1-based line and column of byte `offset`.
fn line_column(text: &str, offset: usize) -> (usize, usize) {
    let before = &text.as_bytes()[..offset.min(text.len())];
    let line = before.iter().filter(|b| **b == b'\n').count() + 1;
    let column = before.len()
        - before
            .iter()
            .rposition(|b| *b == b'\n')
            .map_or(0, |p| p + 1)
        + 1;
    (line, column)
}

/// A contract position holding anything but a name: refused where it is
/// written, before serde, which reads an operation whole and so cannot say
/// which of its fields is wrong.
fn contract_positions(draft: &Value) -> Vec<Diagnostic> {
    let mut places = vec![];
    if let Some(steps) = draft.get("steps").and_then(Value::as_object) {
        for (name, step) in steps {
            let op = step.get("op");
            let kind = op.and_then(|op| op.get("kind")).and_then(Value::as_str);
            let (field, nullable) = match kind {
                Some("invoke") => ("command", false),
                Some("wait") => ("event", true),
                _ => continue,
            };
            if let Some(value) = op.and_then(|op| op.get(field))
                && !value.is_string()
                && (!nullable || !value.is_null())
            {
                places.push(format!("steps.{name}.op.{field}"));
            }
        }
    }
    if let Some(triggers) = draft.get("triggers").and_then(Value::as_array) {
        for (i, t) in triggers.iter().enumerate() {
            if t.get("event").is_some_and(|e| !e.is_string()) {
                places.push(format!("triggers[{i}].event"));
            }
        }
    }
    places
        .into_iter()
        .map(|path| {
            Diagnostic::error(
                "draft_format",
                &at(DRAFT_FILE, &path),
                format!(
                    "{path}: expected a contract name. Write the contract name as a string, such as \"work.read\"; the tool fills in its identity."
                ),
            )
        })
        .collect()
}

/// Strict JSON with the place of the first fault. With `positions`, line and
/// column are those of `text`, the file the agent wrote. With `parsed` (the
/// same text as a value), a fault serde reports at the end of a step is moved
/// to the operation, expression or schema inside it that is wrong.
fn parse<T: DeserializeOwned>(
    file: &str,
    text: &str,
    positions: bool,
    parsed: Option<&Value>,
) -> Result<T, Diagnostic> {
    serde_json::from_str(text).map_err(|e| {
        let (mut line, mut column) = (e.line(), e.column());
        let offset: usize = text
            .split_inclusive('\n')
            .take(line.saturating_sub(1))
            .map(str::len)
            .sum::<usize>()
            + column;
        let full = e.to_string();
        let reason = full
            .rsplit_once(" at line ")
            .map_or(full.as_str(), |(reason, _)| reason);
        let mut path = scan(text, offset, None).0;
        if let Some(inner) = parsed.and_then(|v| at_path(v, &path))
            && deepest_fault(inner, reason, &mut path)
            && let (_, Some(start)) = scan(text, text.len(), Some(&path))
        {
            (line, column) = line_column(text, start);
        }
        let path = render(&path);
        let place = match (positions, path.is_empty()) {
            (true, true) => format!("{file} line {line} column {column}"),
            (true, false) => format!("{file} line {line} column {column} ({path})"),
            (false, true) => file.to_string(),
            (false, false) => format!("{file} {path}"),
        };
        let code = format!("{}_format", file.trim_end_matches(".json"));
        let mut d = Diagnostic::error(&code, &at(file, &path), format!("{place}: {reason}."));
        if positions {
            d.line = u32::try_from(line).ok();
            d.column = u32::try_from(column).ok();
        }
        d
    })
}

/// A regular file directly inside `dir`, at most `max` bytes, as UTF-8.
fn read_file(dir: &Path, name: &str, max: u64) -> Result<String, Diagnostic> {
    let fault = |message: String| Diagnostic::error("workspace_file", name, message);
    let path = dir.join(name);
    let meta = std::fs::symlink_metadata(&path)
        .map_err(|e| fault(format!("{name}: cannot be read from the workspace ({e}).")))?;
    if meta.file_type().is_symlink() {
        return Err(fault(format!(
            "{name} is a symbolic link; the workspace accepts only regular files inside it."
        )));
    }
    if !meta.is_file() {
        return Err(fault(format!("{name} is not a regular file.")));
    }
    let inside = std::fs::canonicalize(&path)
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        == std::fs::canonicalize(dir).ok();
    if !inside {
        return Err(fault(format!("{name} is not inside the workspace.")));
    }
    let too_big = || {
        fault(format!(
            "{name} is larger than {max} bytes; keep it under that."
        ))
    };
    if meta.len() > max {
        return Err(too_big());
    }
    let unreadable = |e: std::io::Error| fault(format!("{name}: cannot be read ({e})."));
    let file = std::fs::File::open(&path).map_err(unreadable)?;
    // What was opened must be the file that was checked: a file swapped in
    // between the check and the open is refused before it is read.
    let opened = file.metadata().map_err(unreadable)?;
    if !opened.is_file() {
        return Err(fault(format!("{name} is not a regular file.")));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if (opened.dev(), opened.ino()) != (meta.dev(), meta.ino()) {
            return Err(fault(format!(
                "{name} was replaced while it was being read; write it once and run again."
            )));
        }
        if opened.nlink() != 1 {
            return Err(fault(format!(
                "{name} has other hard links; the workspace accepts only files of its own. Write it as a new file."
            )));
        }
    }
    if opened.len() > max {
        return Err(too_big());
    }
    let mut bytes = vec![];
    file.take(max + 1)
        .read_to_end(&mut bytes)
        .map_err(unreadable)?;
    if bytes.len() as u64 > max {
        return Err(too_big());
    }
    String::from_utf8(bytes).map_err(|_| fault(format!("{name} is not UTF-8 text.")))
}

fn keep<T>(result: Result<T, Diagnostic>, faults: &mut Vec<Diagnostic>) -> Option<T> {
    result.map_err(|d| faults.push(d)).ok()
}

impl Workspace {
    /// Reads `source.md`, `record.json`, `catalog.json`, `facts.json` and
    /// `draft.json` from `dir`, reporting every unreadable or malformed file.
    ///
    /// This is for the agent's own tools, in the agent's own directory. A
    /// host never calls it on a directory an agent can write: it builds the
    /// [`Workspace`] from its own record, catalog, facts and document, and
    /// takes only `draft.json` from the agent.
    pub fn read(dir: &Path) -> Result<Self, Vec<Diagnostic>> {
        let mut faults = vec![];
        let f = &mut faults;
        let markdown = keep(read_file(dir, SOURCE_FILE, MAX_SOURCE_BYTES), f);
        let record = keep(read_file(dir, RECORD_FILE, MAX_RECORD_BYTES), f)
            .and_then(|t| keep(parse::<SourceRecord>(RECORD_FILE, &t, true, None), f));
        let catalog = keep(read_file(dir, CATALOG_FILE, MAX_CATALOG_BYTES), f)
            .and_then(|t| keep(parse::<Registry>(CATALOG_FILE, &t, true, None), f));
        let facts = keep(read_file(dir, FACTS_FILE, MAX_FACTS_BYTES), f).and_then(|t| {
            keep(
                parse::<BTreeMap<String, Value>>(FACTS_FILE, &t, true, None),
                f,
            )
        });
        let draft = keep(read_file(dir, DRAFT_FILE, MAX_DRAFT_BYTES), f).and_then(|t| {
            let limits = draft_limits(0, t.len());
            if !limits.is_empty() {
                f.extend(limits);
                return None;
            }
            let value = keep(parse::<Value>(DRAFT_FILE, &t, true, None), f)?;
            let names = contract_positions(&value);
            if !names.is_empty() {
                f.extend(names);
                return None;
            }
            keep(parse::<Draft>(DRAFT_FILE, &t, true, Some(&value)), f).map(|_| value)
        });
        match (markdown, record, catalog, facts, draft) {
            (Some(markdown), Some(record), Some(catalog), Some(facts), Some(draft)) => Ok(Self {
                markdown,
                record,
                catalog,
                facts,
                draft,
            }),
            _ => Err(faults),
        }
    }
}

/// The complete source: the `---aq-playbook` header the tool writes, then the
/// Markdown unchanged. Returns it with the number of header lines, by which
/// every `source.md` line moves.
pub fn source_text(
    record: &SourceRecord,
    entries: &BTreeSet<String>,
    facts: &BTreeSet<String>,
    markdown: &str,
) -> (String, u32) {
    let header = SourceHeader {
        format_version: FORMAT_VERSION,
        id: record.binding.clone(),
        scope: record.scope.clone(),
        interfaces: entries.clone(),
        definitions: facts.clone(),
    };
    // A header of strings and sets always serializes.
    let json = serde_json::to_string(&header).unwrap_or_default();
    let head = format!("---aq-playbook\n{json}\n---\n");
    let lines = head.matches('\n').count() as u32;
    (head + markdown, lines)
}

/// The catalog restricted to `commands` and `events`, plus each asynchronous
/// command's reconciliation command and completion event, with the
/// capabilities the retained commands require. Refuses a name absent from
/// `catalog`.
pub fn retain_registry(
    catalog: &Registry,
    commands: &BTreeSet<String>,
    events: &BTreeSet<String>,
) -> Result<(Registry, BTreeSet<String>), String> {
    let mut commands = commands.clone();
    let mut events = events.clone();
    for name in commands.clone() {
        let c = catalog
            .commands
            .get(&name)
            .ok_or_else(|| format!("the catalog has no command {name}"))?;
        if let Some(a) = &c.asynchronous {
            commands.insert(a.reconciliation_command.clone());
            events.insert(a.completion_event.clone());
        }
    }
    let mut registry = Registry::default();
    for name in commands {
        let c = catalog
            .commands
            .get(&name)
            .ok_or_else(|| format!("the catalog has no command {name}"))?;
        registry.commands.insert(name, c.clone());
    }
    for name in events {
        let e = catalog
            .events
            .get(&name)
            .ok_or_else(|| format!("the catalog has no event {name}"))?;
        registry.events.insert(name, e.clone());
    }
    let capabilities = registry
        .commands
        .values()
        .flat_map(|c| c.capabilities.iter().cloned())
        .collect();
    Ok((registry, capabilities))
}

fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut previous = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let here = row[j + 1];
            row[j + 1] = (previous + usize::from(ca != *cb))
                .min(row[j] + 1)
                .min(here + 1);
            previous = here;
        }
    }
    row[b.len()]
}

/// Refuses a name the source may not use, listing the ones it may, closest first.
fn allowed_name(
    kind: &str,
    name: &str,
    allowed: &BTreeSet<&String>,
    path: &str,
    faults: &mut Vec<Diagnostic>,
) {
    if allowed.contains(&name.to_string()) {
        return;
    }
    let mut names: Vec<&&String> = allowed.iter().collect();
    names.sort_by_key(|n| (distance(name, n), (**n).clone()));
    let choice = if names.is_empty() {
        format!("This source may use no {kind}s.")
    } else {
        format!(
            "Use one of the {kind}s this source may use (closest first): {}.",
            names
                .iter()
                .map(|n| n.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    faults.push(Diagnostic::error(
        &format!("unknown_{kind}"),
        &at(DRAFT_FILE, path),
        format!("{path}: {name:?} is not a {kind} this source may use. {choice}"),
    ));
}

fn list(names: &BTreeSet<String>) -> String {
    if names.is_empty() {
        "none".into()
    } else {
        names.iter().cloned().collect::<Vec<_>>().join(", ")
    }
}

/// A draft past the step or byte bound, refused before any other work.
fn draft_limits(steps: usize, bytes: usize) -> Vec<Diagnostic> {
    let advice = "Shorten it: one step per decision, with `lookup` and `let` for tables instead of chains of steps. A draft cannot call other playbooks, so if the document needs more than that, ask the owner to split the document.";
    let mut faults = vec![];
    if steps > MAX_DRAFT_STEPS {
        faults.push(Diagnostic::error(
            "draft_limit",
            &at(DRAFT_FILE, "steps"),
            format!(
                "steps: the draft has {steps} steps; a draft may have at most {MAX_DRAFT_STEPS}. {advice}"
            ),
        ));
    }
    if bytes > MAX_DRAFT_JSON_BYTES {
        faults.push(Diagnostic::error(
            "draft_limit",
            DRAFT_FILE,
            format!(
                "{DRAFT_FILE} is {bytes} bytes; a draft may be at most {MAX_DRAFT_JSON_BYTES} bytes. {advice}"
            ),
        ));
    }
    faults
}

/// Builds the compile input: identities from the catalog, the header and
/// definitions from the record and facts.
pub fn fill(ws: &Workspace) -> Result<CompileInput, Vec<Diagnostic>> {
    // Bounded first: everything after this grows faster than the draft.
    let steps = ws
        .draft
        .get("steps")
        .and_then(Value::as_object)
        .map_or(0, |m| m.len());
    let bytes = serde_json::to_string(&ws.draft).map_or(usize::MAX, |t| t.len());
    let limits = draft_limits(steps, bytes);
    if !limits.is_empty() {
        return Err(limits);
    }
    let names = contract_positions(&ws.draft);
    if !names.is_empty() {
        return Err(names);
    }
    let pretty = serde_json::to_string_pretty(&ws.draft).unwrap_or_default();
    let draft: Draft = parse(DRAFT_FILE, &pretty, false, Some(&ws.draft)).map_err(|d| vec![d])?;
    let record = &ws.record;
    let mut faults = vec![];
    let commands_allowed: BTreeSet<&String> = record
        .commands
        .iter()
        .filter(|n| ws.catalog.commands.contains_key(*n))
        .collect();
    let events_allowed: BTreeSet<&String> = record
        .events
        .iter()
        .filter(|n| ws.catalog.events.contains_key(*n))
        .collect();
    let mut commands = BTreeSet::new();
    let mut events = BTreeSet::new();
    for (name, step) in &draft.steps {
        match &step.op {
            DraftOperation::Invoke { command, .. } => {
                let path = format!("steps.{name}.op.command");
                allowed_name("command", command, &commands_allowed, &path, &mut faults);
                commands.insert(command.clone());
            }
            DraftOperation::Wait {
                event: Some(event), ..
            } => {
                let path = format!("steps.{name}.op.event");
                allowed_name("event", event, &events_allowed, &path, &mut faults);
                events.insert(event.clone());
            }
            DraftOperation::Call { dependency, .. } => {
                let path = format!("steps.{name}.op");
                faults.push(Diagnostic::error(
                    "call",
                    &at(DRAFT_FILE, &path),
                    format!(
                        "{path}: calls playbook {dependency:?}, but this source is offered no callable playbooks. Use the commands it may use instead: {}.",
                        list(&commands_allowed.iter().map(|n| (*n).clone()).collect())
                    ),
                ));
            }
            _ => {}
        }
    }
    for (i, t) in draft.triggers.iter().enumerate() {
        let path = format!("triggers[{i}].event");
        allowed_name("event", &t.event, &events_allowed, &path, &mut faults);
        events.insert(t.event.clone());
    }
    if !faults.is_empty() {
        return Err(faults);
    }
    let (registry, capabilities) = retain_registry(&ws.catalog, &commands, &events)
        .map_err(|e| vec![Diagnostic::error("catalog", CATALOG_FILE, format!("{e}."))])?;
    // Each invoked command, with what its asynchronous completion adds, may
    // need only capabilities the record allows.
    for (name, step) in &draft.steps {
        if let DraftOperation::Invoke { command, .. } = &step.op {
            let c = &registry.commands[command];
            let mut needed = c.capabilities.clone();
            if let Some(a) = &c.asynchronous {
                needed.extend(
                    registry.commands[&a.reconciliation_command]
                        .capabilities
                        .clone(),
                );
            }
            let missing: BTreeSet<String> =
                needed.difference(&record.capabilities).cloned().collect();
            if !missing.is_empty() {
                let path = format!("steps.{name}.op.command");
                faults.push(Diagnostic::error(
                    "capability",
                    &at(DRAFT_FILE, &path),
                    format!(
                        "{path}: {command} needs the capabilities {}, which this source may not use (it may use: {}). Use another command.",
                        list(&missing),
                        list(&record.capabilities)
                    ),
                ));
            }
        }
    }
    let identity = |field: &str, found: String, expected: String, from: &str| {
        Diagnostic::error(
            "draft_identity",
            &at(DRAFT_FILE, field),
            format!(
                "{field}: the draft says {found}, but {from} is {expected}. Leave {field} out: the tool writes it."
            ),
        )
    };
    if let Some(id) = draft.id.as_ref().filter(|id| **id != record.binding) {
        faults.push(identity(
            "id",
            format!("{id:?}"),
            format!("{:?}", record.binding),
            "this source's playbook id (record.json binding)",
        ));
    }
    if let Some(scope) = draft.scope.as_ref().filter(|s| **s != record.scope) {
        faults.push(identity(
            "scope",
            format!("{scope:?}"),
            format!("{:?}", record.scope),
            "this source's scope",
        ));
    }
    if let Some(c) = draft
        .required_capabilities
        .as_ref()
        .filter(|c| **c != capabilities)
    {
        faults.push(identity(
            "required_capabilities",
            list(c),
            list(&capabilities),
            "what the commands it uses require",
        ));
    }
    let (source, header_lines) = source_text(
        record,
        &draft.entries.keys().cloned().collect(),
        &ws.facts.keys().cloned().collect(),
        &ws.markdown,
    );
    let markdown_lines = ws.markdown.lines().count();
    let mut source_map = BTreeMap::new();
    for (step, line) in &draft.source_map {
        let path = format!("source_map.{step}");
        if !draft.steps.contains_key(step) {
            faults.push(Diagnostic::error(
                "source_map",
                &at(DRAFT_FILE, &path),
                format!(
                    "{path}: there is no step {step:?}; map a step name (one of: {}) to a line of source.md.",
                    list(&draft.steps.keys().cloned().collect())
                ),
            ));
        } else if *line == 0 || *line as usize > markdown_lines {
            faults.push(Diagnostic::error(
                "source_map",
                &at(DRAFT_FILE, &path),
                format!(
                    "{path}: line {line} is not a line of source.md, which has lines 1 to {markdown_lines}."
                ),
            ));
        } else {
            source_map.insert(
                format!("steps.{step}"),
                SourceLocation {
                    line: line + header_lines,
                    column: 1,
                },
            );
        }
    }
    let mut definitions = BTreeMap::new();
    for (name, value) in &ws.facts {
        match digest(value) {
            Ok(digest) => {
                definitions.insert(
                    name.clone(),
                    CapturedDefinition {
                        version: record.facts_version.clone(),
                        digest,
                        value: value.clone(),
                    },
                );
            }
            Err(e) => faults.push(Diagnostic::error(
                "facts_format",
                &at(FACTS_FILE, name),
                format!("{FACTS_FILE} {name}: {e}."),
            )),
        }
    }
    let mut reference = |name: &str, fingerprint: Result<Hash, ModelError>| match fingerprint {
        Ok(fingerprint) => ContractRef {
            name: name.into(),
            fingerprint,
        },
        Err(e) => {
            faults.push(Diagnostic::error(
                "catalog",
                CATALOG_FILE,
                format!("{CATALOG_FILE}: contract {name} has no identity: {e}."),
            ));
            ContractRef {
                name: name.into(),
                fingerprint: String::new(),
            }
        }
    };
    let mut steps = BTreeMap::new();
    for (name, step) in draft.steps {
        let op = match step.op {
            DraftOperation::Invoke {
                command,
                input,
                bind,
                retry,
            } => Operation::Invoke {
                command: reference(&command, registry.commands[&command].fingerprint()),
                input,
                bind,
                retry,
            },
            DraftOperation::Decide { predicate } => Operation::Decide { predicate },
            DraftOperation::Wait {
                event,
                correlation,
                minimum_revision,
                deadline_ms,
                bind,
            } => Operation::Wait {
                event: event.map(|e| reference(&e, registry.events[&e].fingerprint())),
                correlation,
                minimum_revision,
                deadline_ms,
                bind,
            },
            DraftOperation::ForEach {
                collection,
                item,
                body,
                next,
                max_items,
                concurrency,
                on_failure,
            } => Operation::ForEach {
                collection,
                item,
                body,
                next,
                max_items,
                concurrency,
                on_failure,
            },
            DraftOperation::Call {
                dependency,
                entry,
                input,
                mode,
                bind,
            } => Operation::Call {
                dependency,
                entry,
                input,
                mode,
                bind,
            },
            DraftOperation::Join {
                members,
                member_keys,
                mode,
                accepted_outcomes,
                deadline_ms,
                remaining,
                bind,
            } => Operation::Join {
                members,
                member_keys,
                mode,
                accepted_outcomes,
                deadline_ms,
                remaining,
                bind,
            },
            DraftOperation::Return { outcome, output } => Operation::Return { outcome, output },
        };
        steps.insert(
            name,
            Step {
                op,
                edges: step.edges,
                on_error: step.on_error,
            },
        );
    }
    let triggers = draft
        .triggers
        .into_iter()
        .map(|t| Trigger {
            event: reference(&t.event, registry.events[&t.event].fingerprint()),
            id: t.id,
            entry: t.entry,
            input: t.input,
        })
        .collect();
    if !faults.is_empty() {
        return Err(faults);
    }
    Ok(CompileInput {
        source,
        proposal: Proposal {
            playbook: Playbook {
                id: record.binding.clone(),
                purpose: draft.purpose.unwrap_or_else(|| record.purpose.clone()),
                scope: record.scope.clone(),
                entries: draft.entries,
                steps,
                triggers,
                limits: draft.limits.unwrap_or_default(),
                required_capabilities: capabilities,
            },
            source_map,
        },
        registry,
        dependencies: BTreeMap::new(),
        definitions,
        compiler: COMPILER_VERSION.into(),
    })
}

/// A validation diagnostic placed in the workspace file it is about.
fn place(mut d: Diagnostic, header_lines: u32) -> Diagnostic {
    let path = d.path.clone();
    d.path = if let Some(rest) = path.strip_prefix("playbook.") {
        at(DRAFT_FILE, rest)
    } else if path == "playbook" {
        DRAFT_FILE.into()
    } else if let Some(name) = path.strip_prefix("definitions.") {
        at(FACTS_FILE, name)
    } else if path == "source" || path.starts_with("source.") {
        SOURCE_FILE.into()
    } else if ["steps.", "entries.", "triggers.", "source_map."]
        .iter()
        .any(|p| path.starts_with(p))
    {
        at(DRAFT_FILE, &path)
    } else {
        path
    };
    // A line and column are always positions in draft.json. A step mapped
    // to the document names its source.md line in the message instead.
    if let Some(line) = d.line.take() {
        let written = line.saturating_sub(header_lines);
        d.message = format!(
            "{} (the step is mapped to source.md line {written})",
            d.message
        );
        d.column = None;
    }
    d
}

fn placed(ws: &Workspace, diagnostics: Vec<Diagnostic>) -> Vec<Diagnostic> {
    let (_, header_lines) = source_text(&ws.record, &BTreeSet::new(), &BTreeSet::new(), "");
    diagnostics
        .into_iter()
        .map(|d| place(d, header_lines))
        .collect()
}

/// Every diagnostic for the workspace's draft: filling, then validation.
/// While the draft does not fill (a name, identity, capability, call or
/// source mapping is wrong), only those faults are reported; validation runs
/// once it fills.
pub fn check(ws: &Workspace) -> Vec<Diagnostic> {
    match fill(ws) {
        Ok(input) => placed(ws, validate(&input)),
        Err(faults) => faults,
    }
}

/// The sealed artifact, or every diagnostic that prevents it.
pub fn seal(ws: &Workspace) -> Result<Artifact, Vec<Diagnostic>> {
    let input = fill(ws)?;
    compile(&input).map_err(|d| placed(ws, d))
}

/// The draft an artifact was filled from: names in place of identities, no
/// id, scope or capabilities, default limits left out, and `source.md` lines.
/// Used for worked examples and for the previous playbook.
pub fn unfill(artifact: &Artifact) -> Value {
    let c = &artifact.content;
    let p = &c.playbook;
    let header = &c.source.header;
    let record = SourceRecord {
        id: header.id.clone(),
        binding: header.id.clone(),
        scope: header.scope.clone(),
        purpose: String::new(),
        commands: BTreeSet::new(),
        events: BTreeSet::new(),
        capabilities: BTreeSet::new(),
        facts_version: String::new(),
    };
    let (_, header_lines) = source_text(&record, &header.interfaces, &header.definitions, "");
    let steps = p
        .steps
        .iter()
        .map(|(name, step)| {
            let op = match step.op.clone() {
                Operation::Invoke {
                    command,
                    input,
                    bind,
                    retry,
                } => DraftOperation::Invoke {
                    command: command.name,
                    input,
                    bind,
                    retry,
                },
                Operation::Decide { predicate } => DraftOperation::Decide { predicate },
                Operation::Wait {
                    event,
                    correlation,
                    minimum_revision,
                    deadline_ms,
                    bind,
                } => DraftOperation::Wait {
                    event: event.map(|e| e.name),
                    correlation,
                    minimum_revision,
                    deadline_ms,
                    bind,
                },
                Operation::ForEach {
                    collection,
                    item,
                    body,
                    next,
                    max_items,
                    concurrency,
                    on_failure,
                } => DraftOperation::ForEach {
                    collection,
                    item,
                    body,
                    next,
                    max_items,
                    concurrency,
                    on_failure,
                },
                Operation::Call {
                    dependency,
                    entry,
                    input,
                    mode,
                    bind,
                } => DraftOperation::Call {
                    dependency,
                    entry,
                    input,
                    mode,
                    bind,
                },
                Operation::Join {
                    members,
                    member_keys,
                    mode,
                    accepted_outcomes,
                    deadline_ms,
                    remaining,
                    bind,
                } => DraftOperation::Join {
                    members,
                    member_keys,
                    mode,
                    accepted_outcomes,
                    deadline_ms,
                    remaining,
                    bind,
                },
                Operation::Return { outcome, output } => DraftOperation::Return { outcome, output },
            };
            (
                name.clone(),
                DraftStep {
                    op,
                    edges: step.edges.clone(),
                    on_error: step.on_error.clone(),
                },
            )
        })
        .collect();
    let draft = Draft {
        id: None,
        scope: None,
        required_capabilities: None,
        purpose: Some(p.purpose.clone()),
        entries: p.entries.clone(),
        steps,
        triggers: p
            .triggers
            .iter()
            .map(|t| DraftTrigger {
                id: t.id.clone(),
                event: t.event.name.clone(),
                entry: t.entry.clone(),
                input: t.input.clone(),
            })
            .collect(),
        limits: Some(p.limits.clone()).filter(|l| *l != Limits::default()),
        source_map: c
            .source_map
            .iter()
            .filter_map(|(key, loc)| {
                let step = key.strip_prefix("steps.")?;
                let line = loc.line.checked_sub(header_lines).filter(|l| *l > 0)?;
                Some((step.to_string(), line))
            })
            .collect(),
    };
    // The draft is built from model values, which always serialize.
    serde_json::to_value(draft).unwrap_or(Value::Null)
}

/// How a new playbook differs from the previous one, in the owner's terms.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Comparison {
    /// Identical, wording only (`Presentation`), behaviour (`Executable`) or interface.
    pub change: ChangeKind,
    /// A capability was added, or a command or event contract changed.
    pub abilities_changed: bool,
    pub diff: SemanticDiff,
}

pub fn compare(previous: &Artifact, next: &Artifact) -> Result<Comparison, ModelError> {
    let diff = semantic_diff(previous, next)?;
    Ok(Comparison {
        change: diff.kind.clone(),
        abilities_changed: !diff.capabilities_added.is_empty()
            || !diff.changed_commands.is_empty()
            || !diff.changed_events.is_empty(),
        diff,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_paths_name_object_keys_and_array_items() {
        let text = "{\"steps\": {\"a\": {\"op\": 1}}, \"triggers\": [{\"id\": 1}, {\"event\": 2}]}";
        let offset = |needle: &str| text.find(needle).unwrap() + needle.len();
        assert_eq!(json_path_at(text, offset("\"op\"")), "steps.a.op");
        assert_eq!(json_path_at(text, offset("\"event\"")), "triggers[1].event");
        assert_eq!(json_path_at(text, 1), "");
        // Escaped quotes inside keys do not end them early.
        let text = "{\"a\\\"b\": {\"c\": 1}}";
        assert_eq!(json_path_at(text, text.find("1").unwrap()), "a\"b.c");
        // Malformed input never panics.
        for text in ["{\"a\\", "{\"é\\é", "}}]]", "{\"a\": [\"x\\\"] "] {
            json_path_at(text, text.len() + 5);
        }
        let d = parse::<Value>(DRAFT_FILE, "{\"a\": [1, \"é\\", true, None).unwrap_err();
        assert_eq!(d.code, "draft_format");
    }

    #[test]
    fn a_fault_inside_an_operation_is_placed_at_the_deepest_wrong_object() {
        let text = "{\"steps\": {\"a\": {\"op\": {\"kind\": \"return\", \"outcome\": \"x\",\n  \"output\": {\"op\": \"not\", \"valu\": {\"op\": \"literal\", \"value\": true}}},\n \"edges\": {}, \"on_error\": null}}}";
        let value: Value = serde_json::from_str(text).unwrap();
        #[derive(Debug, Deserialize)]
        #[allow(dead_code)]
        struct Steps {
            steps: BTreeMap<String, DraftStep>,
        }
        let d = parse::<Steps>(DRAFT_FILE, text, true, Some(&value)).unwrap_err();
        assert_eq!(d.path, "draft.json:steps.a.op.output", "{d:#?}");
        assert_eq!((d.line, d.column), (Some(2), Some(13)), "{d:#?}");
        assert!(d.message.contains("unknown field `valu`"), "{d:#?}");
        let mut path = vec![];
        assert!(deepest_fault(
            &value,
            "unknown field `valu`, expected `value`",
            &mut path
        ));
        assert_eq!(render(&path), "steps.a.op.output");
        assert_eq!(
            scan(text, text.len(), Some(&path)).1,
            Some(text.find("{\"op\": \"not\"").unwrap())
        );
    }

    #[test]
    fn names_are_offered_closest_first() {
        assert_eq!(distance("work.reed", "work.read"), 1);
        assert!(distance("work.reed", "work.route") > 1);
        let allowed = ["work.route".to_string(), "work.read".to_string()];
        let mut faults = vec![];
        allowed_name(
            "command",
            "work.reed",
            &allowed.iter().collect(),
            "steps.a.op.command",
            &mut faults,
        );
        assert!(faults[0].message.ends_with("work.read, work.route."));
        let mut faults = vec![];
        allowed_name(
            "event",
            "x",
            &BTreeSet::new(),
            "triggers[0].event",
            &mut faults,
        );
        assert!(
            faults[0]
                .message
                .ends_with("This source may use no events.")
        );
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("aq-draft-unit-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn workspace_files_must_be_regular_bounded_and_inside() {
        let dir = scratch("files");
        let missing = Workspace::read(&dir).unwrap_err();
        assert_eq!(missing.len(), 5, "{missing:#?}");
        assert!(missing.iter().all(|d| d.code == "workspace_file"));
        std::fs::write(dir.join(SOURCE_FILE), "# Doc\n").unwrap();
        std::fs::write(dir.join(RECORD_FILE), "{\"id\": \"x\"}").unwrap();
        std::fs::write(dir.join(CATALOG_FILE), "{\"commands\": {}, \"events\": {}}").unwrap();
        std::fs::write(
            dir.join(FACTS_FILE),
            vec![b' '; MAX_FACTS_BYTES as usize + 1],
        )
        .unwrap();
        std::fs::create_dir(dir.join(DRAFT_FILE)).unwrap();
        let faults = Workspace::read(&dir).unwrap_err();
        let by = |path: &str| faults.iter().find(|d| d.path.starts_with(path)).unwrap();
        assert_eq!(by(RECORD_FILE).code, "record_format");
        assert!(
            by(RECORD_FILE).message.contains("missing field"),
            "{faults:#?}"
        );
        assert!(
            by(FACTS_FILE).message.contains("larger than"),
            "{faults:#?}"
        );
        assert!(
            by(DRAFT_FILE).message.contains("not a regular file"),
            "{faults:#?}"
        );
        #[cfg(unix)]
        {
            let outside = scratch("outside");
            std::fs::write(
                outside.join("catalog.json"),
                "{\"commands\": {}, \"events\": {}}",
            )
            .unwrap();
            std::fs::remove_file(dir.join(CATALOG_FILE)).unwrap();
            std::os::unix::fs::symlink(outside.join("catalog.json"), dir.join(CATALOG_FILE))
                .unwrap();
            let faults = Workspace::read(&dir).unwrap_err();
            let link = faults.iter().find(|d| d.path == CATALOG_FILE).unwrap();
            assert!(link.message.contains("symbolic link"), "{faults:#?}");
            // A hard link to a file elsewhere is refused too.
            std::fs::remove_file(dir.join(CATALOG_FILE)).unwrap();
            std::fs::hard_link(outside.join("catalog.json"), dir.join(CATALOG_FILE)).unwrap();
            let faults = Workspace::read(&dir).unwrap_err();
            let link = faults.iter().find(|d| d.path == CATALOG_FILE).unwrap();
            assert!(link.message.contains("hard links"), "{faults:#?}");
            std::fs::remove_dir_all(outside).unwrap();
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn validation_paths_are_placed_in_workspace_files() {
        let d = |path: &str, line: Option<u32>| Diagnostic {
            line,
            ..Diagnostic::error("x", path, "m")
        };
        assert_eq!(place(d("steps.a", None), 3).path, "draft.json:steps.a");
        assert_eq!(
            place(d("playbook.limits", None), 3).path,
            "draft.json:limits"
        );
        assert_eq!(place(d("playbook", None), 3).path, "draft.json");
        assert_eq!(
            place(d("definitions.levels", None), 3).path,
            "facts.json:levels"
        );
        assert_eq!(
            place(d("source.header.interfaces", None), 3).path,
            "source.md"
        );
        assert_eq!(place(d("registry", None), 3).path, "registry");
        let mapped = place(
            Diagnostic {
                column: Some(1),
                ..d("steps.a", Some(15))
            },
            3,
        );
        assert_eq!(
            mapped.message,
            "m (the step is mapped to source.md line 12)"
        );
        assert_eq!((mapped.line, mapped.column), (None, None));
    }
}
