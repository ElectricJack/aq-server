//! Documents shipped to the authoring agent with its workspace: the guide
//! ([`GUIDE`], written into the workspace as `GUIDE.md`), the language
//! reference ([`LANGUAGE`], `LANGUAGE.md`), the notes on the commands and
//! events the source may use ([`catalog`], `CATALOG.md`) and the worked
//! examples ([`example_files`], under `examples/`). `FORMAT.md` is the
//! engine's format specification, for engine developers, and is not shipped.
//! Every command, draft and case the documents show is run by the tests
//! (`tests/docs.rs` here, and `authoring_examples.rs` in the CLI), so they
//! cannot drift from the tools.
//!
//! [`example_files`] holds the worked example workspaces in this crate's
//! `authoring-examples` directory: complete workspaces an agent can read, and
//! run the authoring tools in, to see how a document becomes a playbook.
//! They live inside the crate, so that a copy of `crates/` builds it.
//! `aq-playbooks examples --output DIR` writes them out.
use crate::draft::retain_registry;
use aq_playbooks_model::{
    CatalogExtension, CommandContract, EventContract, Recovery, Registry, Schema,
};
use serde_json::Value;
use std::collections::BTreeSet;

/// How to turn a document into a playbook: the workflow, when to ask or give
/// up, the rules for a draft, the case format and the limits.
pub const GUIDE: &str = include_str!("../../../skills/aq-playbook-author/SKILL.md");
/// Every field, operation, expression, rule, limit and diagnostic code of a
/// draft.
pub const LANGUAGE: &str = include_str!("../LANGUAGE.md");
/// The pinned host catalog the notes describe (catalog 2.2.0).
pub const CATALOG_REGISTRY: &str = include_str!("../../../contracts/aq-host-2.2.0/registry.json");
/// What the host says about each contract of that catalog, beside its
/// registry so that no fingerprint changes.
pub const CATALOG_NOTES: &str = include_str!("../../../contracts/aq-host-2.2.0/notes.json");

/// The pinned catalog.
pub fn catalog_registry() -> Registry {
    serde_json::from_str(CATALOG_REGISTRY).expect("the pinned catalog parses")
}

/// The notes on one contract: `kind` is `"commands"` or `"events"`.
pub fn notes(kind: &str, name: &str) -> Option<Value> {
    notes_in(CATALOG_NOTES, kind, name)
}
fn notes_in(all: &str, kind: &str, name: &str) -> Option<Value> {
    let all: Value = serde_json::from_str(all).ok()?;
    contract_notes(&all, kind, name).cloned()
}

fn contract_notes<'a>(all: &'a Value, kind: &str, name: &str) -> Option<&'a Value> {
    all.get(kind)?.get(name)
}

/// Catalog 2.3.0 (Epic 0, delivery by playbook), not yet the pinned catalog.
#[cfg(feature = "delivery_playbook")]
pub const CATALOG_2_3_REGISTRY: &str =
    include_str!("../../../contracts/aq-host-2.3.0/registry.json");
/// What the host says about each contract of catalog 2.3.0.
#[cfg(feature = "delivery_playbook")]
pub const CATALOG_2_3_NOTES: &str = include_str!("../../../contracts/aq-host-2.3.0/notes.json");

/// [`catalog`] for catalog 2.3.0 and its notes.
#[cfg(feature = "delivery_playbook")]
pub fn catalog_2_3(
    commands: &BTreeSet<String>,
    events: &BTreeSet<String>,
) -> Result<String, String> {
    let registry = serde_json::from_str(CATALOG_2_3_REGISTRY)
        .map_err(|e| format!("catalog 2.3.0 does not parse: {e}"))?;
    catalog_with_extensions(&registry, CATALOG_2_3_NOTES, commands, events, &[])
}

/// `CATALOG.md` for a source that may use `commands` and `events`: every
/// contract its `catalog.json` holds (the named ones, and the reconciliation
/// command and completion event of each asynchronous command), with what the
/// host says about each input field, outcome and event. Refuses a name the
/// pinned catalog lacks, and notes that do not cover a contract exactly.
pub fn catalog(commands: &BTreeSet<String>, events: &BTreeSet<String>) -> Result<String, String> {
    catalog_with_extensions(&catalog_registry(), CATALOG_NOTES, commands, events, &[])
}

/// Render the selected base contracts and an authoring section for each
/// extension that contributes a selected contract.
pub fn catalog_with_extensions(
    base: &Registry,
    base_notes: &str,
    commands: &BTreeSet<String>,
    events: &BTreeSet<String>,
    extensions: &[CatalogExtension],
) -> Result<String, String> {
    let registry = base
        .with_extensions(extensions)
        .map_err(|e| e.to_string())?;
    let (kept, _) = retain_registry(&registry, commands, events)?;
    let base_notes =
        serde_json::from_str(base_notes).map_err(|e| format!("catalog notes do not parse: {e}"))?;
    let mut out = String::from(
        "# Catalog\n\nThe commands and events this playbook may use, and what the host says about \
         each. `aq-playbooks commands describe <name>` and `aq-playbooks events describe <name>` \
         show the same contracts as JSON, with these notes. Where a meaning says \"Not stated\", \
         the catalog does not say: do not rely on it, and ask the owner when the document \
         depends on it.\n",
    );
    let commands = kept
        .commands
        .iter()
        .filter(|(name, _)| base.commands.contains_key(*name));
    if commands.clone().next().is_some() {
        out.push_str("\n## Commands\n");
    }
    for (_, contract) in commands {
        out.push_str(&command(contract, &base_notes)?);
    }
    let events = kept
        .events
        .iter()
        .filter(|(name, _)| base.events.contains_key(*name));
    if events.clone().next().is_some() {
        out.push_str("\n## Events\n");
    }
    for (_, contract) in events {
        out.push_str(&event(contract, &base_notes)?);
    }
    let mut extensions: Vec<_> = extensions.iter().collect();
    extensions.sort_by_key(|extension| extension.name());
    for extension in extensions {
        let mut section = String::new();
        for name in extension.registry().commands.keys() {
            if let Some(contract) = kept.commands.get(name) {
                section.push_str(&command(contract, extension.notes())?);
            }
        }
        for name in extension.registry().events.keys() {
            if let Some(contract) = kept.events.get(name) {
                section.push_str(&event(contract, extension.notes())?);
            }
        }
        if !section.is_empty() {
            out.push_str(&format!("\n## Extension `{}`\n", extension.name()));
            out.push_str(&section);
        }
    }
    Ok(out)
}

fn stated(text: &str) -> &str {
    if text.trim().is_empty() {
        "Not stated."
    } else {
        text
    }
}
fn text_of<'v>(notes: &'v Value, key: &str) -> &'v str {
    notes.get(key).and_then(Value::as_str).unwrap_or("")
}
/// The names a notes map gives, which must be exactly `expected`.
fn covered(
    what: &str,
    notes: &Value,
    key: &str,
    expected: &BTreeSet<String>,
) -> Result<(), String> {
    let given: BTreeSet<String> = notes
        .get(key)
        .and_then(Value::as_object)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    if &given == expected {
        Ok(())
    } else {
        Err(format!(
            "the notes on {what} give {key} {given:?}, but the contract has {expected:?}"
        ))
    }
}
fn properties(schema: &Schema) -> BTreeSet<String> {
    match schema {
        Schema::Object { properties, .. } => properties.keys().cloned().collect(),
        _ => BTreeSet::new(),
    }
}
/// A short type: `string ≤ 64`, `one of a, b`, `integer 0..1000`,
/// `{id, revision}`, `array ≤ 8 of …`, `… or null`.
pub fn short_type(schema: &Schema) -> String {
    match schema {
        Schema::Null => "null".into(),
        Schema::Boolean => "boolean".into(),
        Schema::Integer { minimum, maximum } => format!("integer {minimum}..{maximum}"),
        Schema::String { max_length, values } if values.is_empty() => {
            format!("string ≤ {max_length} B")
        }
        Schema::String { values, .. } => format!(
            "one of {}",
            values
                .iter()
                .map(|v| format!("`{v}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Schema::Array { items, max_items } => {
            format!("array ≤ {max_items} of {}", short_type(items))
        }
        Schema::Object {
            properties,
            required,
        } => format!(
            "{{{}}}",
            properties
                .keys()
                .map(|k| if required.contains(k) {
                    k.clone()
                } else {
                    format!("{k}?")
                })
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Schema::Nullable { inner } => format!("{} or null", short_type(inner)),
        Schema::Union { variants } => variants
            .iter()
            .map(short_type)
            .collect::<Vec<_>>()
            .join(" or "),
    }
}
fn paragraphs(notes: &Value) -> String {
    notes
        .get("notes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|p| format!("\n{p}\n"))
        .collect()
}
fn command(c: &CommandContract, all_notes: &Value) -> Result<String, String> {
    let notes = contract_notes(all_notes, "commands", &c.name)
        .ok_or(format!("no notes on command {}", c.name))?;
    let what = format!("command {}", c.name);
    covered(&what, &notes, "input", &properties(&c.input))?;
    covered(
        &what,
        &notes,
        "outcomes",
        &c.outcomes.keys().cloned().collect(),
    )?;
    let mut out = format!(
        "\n### `{}` {}\n\n{}\n\n",
        c.name,
        c.version,
        stated(text_of(&notes, "purpose"))
    );
    let kind = match (&c.recovery, &c.asynchronous) {
        (Recovery::Observation, _) => "A read: safe to repeat.".to_string(),
        (_, Some(a)) => format!(
            "A change that answers later: its `accepted` edge binds `{{handle}}`, and the answer arrives as `{}`. Read it with a join.",
            a.completion_event
        ),
        (Recovery::Reconcilable, _) => "A change the host reconciles from its own receipt.".into(),
        _ => "A change, made once per call: a repeat of the same call returns the first result."
            .into(),
    };
    out.push_str(&format!(
        "{kind} The host answers within {} ms. Needs: {}.\n\n| Input field | Type | Meaning |\n| --- | --- | --- |\n",
        c.deadline_ms,
        c.capabilities.iter().map(|k| format!("`{k}`")).collect::<Vec<_>>().join(", ")
    ));
    if let Schema::Object { properties, .. } = &c.input {
        for (field, schema) in properties {
            out.push_str(&format!(
                "| `{field}` | {} | {} |\n",
                short_type(schema),
                stated(notes["input"][field].as_str().unwrap_or(""))
            ));
        }
    }
    out.push_str("\n| Outcome | Returns | When, and what next |\n| --- | --- | --- |\n");
    for (outcome, schema) in &c.outcomes {
        out.push_str(&format!(
            "| `{outcome}` | {} | {} |\n",
            short_type(schema),
            stated(notes["outcomes"][outcome].as_str().unwrap_or(""))
        ));
    }
    out.push_str(&paragraphs(&notes));
    Ok(out)
}
fn event(e: &EventContract, all_notes: &Value) -> Result<String, String> {
    let notes = contract_notes(all_notes, "events", &e.name)
        .ok_or(format!("no notes on event {}", e.name))?;
    covered(
        &format!("event {}", e.name),
        &notes,
        "payload",
        &properties(&e.payload),
    )?;
    let correlation = if e.correlation_fields.is_empty() {
        "none".to_string()
    } else {
        e.correlation_fields
            .iter()
            .map(|f| format!("`{f}`"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut out = format!(
        "\n### `{}` {}\n\n{}\n\n**Sent:** {}\n\nCorrelation fields: {correlation}. Kept {} ms.\n\n| Payload field | Type | Meaning |\n| --- | --- | --- |\n",
        e.name,
        e.version,
        stated(text_of(&notes, "purpose")),
        stated(text_of(&notes, "sent")),
        e.retention_ms
    );
    if let Schema::Object { properties, .. } = &e.payload {
        for (field, schema) in properties {
            out.push_str(&format!(
                "| `{field}` | {} | {} |\n",
                short_type(schema),
                stated(notes["payload"][field].as_str().unwrap_or(""))
            ));
        }
    }
    out.push_str(&paragraphs(&notes));
    Ok(out)
}

/// Every file of the worked examples, as `(path, content)`. A path is
/// relative to `authoring-examples`, such as `"notify-when-ready/draft.json"`;
/// its first component names the example.
pub fn example_files() -> Vec<(&'static str, &'static str)> {
    macro_rules! files {
        ($($path:literal),* $(,)?) => {
            vec![$(($path, include_str!(concat!("../authoring-examples/", $path)))),*]
        };
    }
    files![
        "escalate-on-judgment/CATALOG.md",
        "escalate-on-judgment/cases/assumed-route-refused.json",
        "escalate-on-judgment/cases/assumed-route-stale.json",
        "escalate-on-judgment/cases/failed-three-times.json",
        "escalate-on-judgment/cases/failed-twice.json",
        "escalate-on-judgment/cases/high-risk.json",
        "escalate-on-judgment/cases/judgment-fails.json",
        "escalate-on-judgment/cases/judgment-never-answers.json",
        "escalate-on-judgment/cases/judgment-refused-at-once.json",
        "escalate-on-judgment/cases/low-risk.json",
        "escalate-on-judgment/cases/medium-risk-small-and-tested.json",
        "escalate-on-judgment/cases/medium-risk.json",
        "escalate-on-judgment/cases/nothing-offers-the-level.json",
        "escalate-on-judgment/cases/owner-chose.json",
        "escalate-on-judgment/cases/risk-unknown.json",
        "escalate-on-judgment/cases/task-changed.json",
        "escalate-on-judgment/cases/task-gone.json",
        "escalate-on-judgment/cases/unroutable-refused.json",
        "escalate-on-judgment/cases/unroutable-stale.json",
        "escalate-on-judgment/catalog.json",
        "escalate-on-judgment/draft.json",
        "escalate-on-judgment/facts.json",
        "escalate-on-judgment/previous.artifact.json",
        "escalate-on-judgment/record.json",
        "escalate-on-judgment/source.md",
        "escalate-on-judgment/stuck/deep-level.json",
        "escalate-on-judgment/stuck/deep-level.md",
        "notify-when-ready/CATALOG.md",
        "notify-when-ready/cases/changed-and-still-bare.json",
        "notify-when-ready/cases/described-meanwhile.json",
        "notify-when-ready/cases/finished-during-the-wait.json",
        "notify-when-ready/cases/message-refused.json",
        "notify-when-ready/cases/still-no-description.json",
        "notify-when-ready/cases/task-cancelled.json",
        "notify-when-ready/cases/task-finished.json",
        "notify-when-ready/cases/task-gone.json",
        "notify-when-ready/cases/task-not-readable.json",
        "notify-when-ready/catalog.json",
        "notify-when-ready/draft.json",
        "notify-when-ready/facts.json",
        "notify-when-ready/previous.artifact.json",
        "notify-when-ready/record.json",
        "notify-when-ready/source.md",
        "notify-when-ready/stuck/how-long.json",
        "notify-when-ready/stuck/how-long.md",
        "notify-when-ready/stuck/phone-call.json",
        "notify-when-ready/stuck/phone-call.md",
        "notify-when-ready/stuck/urgent.json",
        "notify-when-ready/stuck/urgent.md",
    ]
}
