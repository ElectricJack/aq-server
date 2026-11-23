//! The workspace an authoring agent is given, and the internal work request
//! that carries it (spec 0013 §4.2). Everything here is the server's own
//! copy; nothing is read back from the agent's directory except its outcome.
use crate::{Config, Error, Result, SourceRecord};
use aq_internal_work::{Affected, AgentWork, AnswerShape, Purpose, Request};
use aq_playbooks_compiler::{docs, draft};
use aq_playbooks_model::{Artifact, Registry};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// What every authoring agent is told, whatever the source is about.
pub const INSTRUCTIONS: &str = "\
You turn a document its owner wrote in plain language into a playbook that does what the document says.

Your workspace holds everything you need:
- source.md is the owner's document. It is the only statement of what the playbook must do.
- GUIDE.md says how to work, step by step. Read it first and follow it exactly.
- LANGUAGE.md describes every part of a draft; CATALOG.md describes every command and event you may use.
- record.json, catalog.json and facts.json are the host's; never change them.
- examples/ holds complete worked workspaces.
- previous.artifact.json and previous/ are there when a playbook from an earlier version of the document is in effect.

Write draft.json and the cases under cases/, and check them with the aq-playbooks tool, run from this directory. \
There is nobody to ask while you work. When the document can be read in two ways that would act differently, or names \
something the host does not offer, do not guess: end with `aq-playbooks ask`. When the document cannot be turned into a \
playbook at all, end with `aq-playbooks unable`. Otherwise end with `aq-playbooks save`. End exactly once, and only \
through one of those three commands; never write out/outcome.json yourself.";

/// JSON Schema of `out/outcome.json` as internal work checks it. The engine's
/// own strict reading of the outcome follows, on the server.
pub const OUTCOME_SCHEMA: &str = r#"{"type":"object","required":["kind"],"properties":{"kind":{"enum":["saved","question","unable"]}}}"#;

/// The playbook in effect when a run starts, in the terms an agent reads.
#[derive(Clone, Debug, PartialEq)]
pub struct PreviousPlaybook {
    pub artifact: Artifact,
    /// The document it was authored from, when the server still has it.
    pub markdown: Option<String>,
    /// The cases it was saved with.
    pub cases: Vec<Value>,
}

/// The record as the engine's tools read it.
pub fn engine_record(record: &SourceRecord, facts_version: &str) -> draft::SourceRecord {
    draft::SourceRecord {
        id: record.id.clone(),
        binding: record.binding.clone(),
        scope: record.scope.clone(),
        purpose: record.purpose.clone(),
        commands: record.commands.clone(),
        events: record.events.clone(),
        capabilities: record.capabilities.clone(),
        facts_version: facts_version.to_owned(),
    }
}

/// The catalog a source is authored against, whole: catalog 2.3.0 for the
/// built-in `delivery` source (spec 0016 §6), the pinned catalog 2.2.0 for
/// every other. With spec 0017, 2.5.0 for both built-in sources.
pub fn pinned_catalog(record: &SourceRecord) -> Result<Registry> {
    #[cfg(all(feature = "delivery_playbook", feature = "project_environments"))]
    if record.id == crate::builtin::DELIVERY {
        return catalog_2_5_delivery();
    }
    #[cfg(all(feature = "delivery_playbook", feature = "project_environments"))]
    if record.id == crate::builtin::ROUTING {
        return catalog_2_5();
    }
    #[cfg(feature = "delivery_playbook")]
    if record.id == crate::builtin::DELIVERY {
        return catalog_2_3();
    }
    let _ = record;
    Ok(docs::catalog_registry())
}

#[cfg(feature = "delivery_playbook")]
pub fn catalog_2_3() -> Result<Registry> {
    serde_json::from_str(docs::CATALOG_2_3_REGISTRY)
        .map_err(|_| Error::Invalid("catalog 2.3.0 does not parse".into()))
}

/// Catalog 2.5.0 (spec 0017).
#[cfg(all(feature = "delivery_playbook", feature = "project_environments"))]
pub const CATALOG_2_5_REGISTRY: &str =
    include_str!("../../../vendor/aq-host-catalog-2.5.0/registry.json");
#[cfg(all(feature = "delivery_playbook", feature = "project_environments"))]
const CATALOG_2_5_NOTES: &str = include_str!("../../../vendor/aq-host-catalog-2.5.0/notes.json");

/// Catalog 2.5.0 whole: the routing source's (spec 0017 §5.4), whose
/// `work.read` and `work.route` carry the environment.
#[cfg(all(feature = "delivery_playbook", feature = "project_environments"))]
pub fn catalog_2_5() -> Result<Registry> {
    serde_json::from_str(CATALOG_2_5_REGISTRY)
        .map_err(|_| Error::Invalid("catalog 2.5.0 does not parse".into()))
}

/// The delivery source's catalog 2.5.0: the environment commands and events
/// added, every 2.3.0 contract it uses unchanged (its `work.read` is 2.3.0's;
/// the 2.5.0 one adds the routing facts of spec 0017 §5.4).
#[cfg(all(feature = "delivery_playbook", feature = "project_environments"))]
pub fn catalog_2_5_delivery() -> Result<Registry> {
    let mut catalog = catalog_2_5()?;
    let read = catalog_2_3()?.commands.remove("work.read");
    catalog
        .commands
        .extend(read.map(|c| ("work.read".to_owned(), c)));
    Ok(catalog)
}

/// `CATALOG.md` of the catalog the workspace actually uses.
fn catalog_notes(record: &SourceRecord, _catalog: &Registry) -> Result<String> {
    #[cfg(all(feature = "delivery_playbook", feature = "project_environments"))]
    if record.id == crate::builtin::DELIVERY {
        // 2.5.0's notes, with 2.3.0's on the 2.3.0 work.read.
        let parse = |text: &str| serde_json::from_str::<Value>(text).map_err(|_| Error::Corrupt);
        let mut notes = parse(CATALOG_2_5_NOTES)?;
        notes["commands"]["work.read"] =
            parse(docs::CATALOG_2_3_NOTES)?["commands"]["work.read"].take();
        let mut text = docs::catalog_with_extensions(
            _catalog,
            &notes.to_string(),
            &record.commands,
            &record.events,
            &[],
        )
        .map_err(Error::Invalid)?;
        text.push_str(include_str!("delivery_review_contract.md"));
        return Ok(text);
    }
    #[cfg(all(feature = "delivery_playbook", feature = "project_environments"))]
    if record.id == crate::builtin::ROUTING {
        let (commands, events) = (&record.commands, &record.events);
        return docs::catalog_with_extensions(_catalog, CATALOG_2_5_NOTES, commands, events, &[])
            .map_err(Error::Invalid);
    }
    #[cfg(feature = "delivery_playbook")]
    if _catalog
        .commands
        .get("work.read")
        .is_some_and(|c| c.version == "2.3.0")
    {
        let mut notes =
            docs::catalog_2_3(&record.commands, &record.events).map_err(Error::Invalid)?;
        if record.id == crate::builtin::DELIVERY {
            notes.push_str(include_str!("delivery_review_contract.md"));
        }
        return Ok(notes);
    }
    docs::catalog(&record.commands, &record.events).map_err(Error::Invalid)
}

/// `catalog` kept to what the record allows (and what those commands need).
pub fn catalog_for(record: &SourceRecord, catalog: &Registry) -> Result<Registry> {
    draft::retain_registry(catalog, &record.commands, &record.events)
        .map(|(kept, _)| kept)
        .map_err(Error::Invalid)
}

fn pretty<T: serde::Serialize>(value: &T) -> Result<String> {
    let mut text = serde_json::to_string_pretty(value).map_err(|_| Error::Corrupt)?;
    text.push('\n');
    Ok(text)
}

/// A file name for a case: its place in the list and its name in letters,
/// digits and dashes.
fn case_file(index: usize, case: &Value) -> String {
    let name = case.get("name").and_then(Value::as_str).unwrap_or("case");
    let slug: String = name
        .chars()
        .map(|c| match c.is_ascii_alphanumeric() {
            true => c.to_ascii_lowercase(),
            false => '-',
        })
        .take(64)
        .collect();
    format!("{:02}-{slug}.json", index + 1)
}

/// Every file of the workspace, by relative path: the document, the host's
/// record, catalog and facts, the guide, the language reference, the catalog
/// in words, the worked examples and, when one is in effect, the previous
/// playbook. `facts_version` is recorded with every fact the draft reads.
pub fn workspace_files(
    record: &SourceRecord,
    markdown: &str,
    catalog: &Registry,
    facts: &BTreeMap<String, Value>,
    facts_version: &str,
    previous: Option<&PreviousPlaybook>,
) -> Result<BTreeMap<String, String>> {
    let mut files = BTreeMap::new();
    let mut put = |path: &str, text: String| files.insert(path.to_owned(), text);
    put(draft::SOURCE_FILE, markdown.to_owned());
    put(
        draft::RECORD_FILE,
        pretty(&engine_record(record, facts_version))?,
    );
    put(draft::CATALOG_FILE, pretty(&catalog_for(record, catalog)?)?);
    put(draft::FACTS_FILE, pretty(facts)?);
    put("GUIDE.md", docs::GUIDE.to_owned());
    put("LANGUAGE.md", docs::LANGUAGE.to_owned());
    put("CATALOG.md", catalog_notes(record, catalog)?);
    for (path, text) in docs::example_files() {
        put(&format!("examples/{path}"), text.to_owned());
    }
    if let Some(previous) = previous {
        put("previous.artifact.json", pretty(&previous.artifact)?);
        put(
            "previous/draft.json",
            pretty(&draft::unfill(&previous.artifact))?,
        );
        if let Some(markdown) = &previous.markdown {
            put("previous/source.md", markdown.clone());
        }
        for (index, case) in previous.cases.iter().enumerate() {
            put(
                &format!("previous/cases/{}", case_file(index, case)),
                pretty(case)?,
            );
        }
    }
    Ok(files)
}

/// The internal work request of one authoring run of `record`.
pub fn request(record: &SourceRecord, files: BTreeMap<String, String>, config: &Config) -> Request {
    Request {
        purpose: Purpose::PlaybookCompilation,
        instructions: INSTRUCTIONS.to_owned(),
        input: String::new(),
        answer: AnswerShape::Json {
            schema: serde_json::from_str(OUTCOME_SCHEMA).unwrap_or_else(|_| json!({})),
        },
        blocking: false,
        affected: Affected {
            kind: "playbook_source".into(),
            id: record.id.clone(),
            project_id: None,
        },
        deadline_ms: config.deadline_ms,
        max_output_bytes: config.outcome_bytes,
        escalate: true,
        profile_override: None,
        agent: Some(AgentWork {
            files,
            max_turns: config.max_turns,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SourceKind;
    use std::collections::BTreeSet;

    fn names(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    pub(crate) fn notify() -> SourceRecord {
        SourceRecord {
            id: "notify-when-ready".into(),
            kind: SourceKind::Registered,
            binding: "aq.notify-when-ready".into(),
            scope: "project".into(),
            purpose: "Ask for a description when a task has none".into(),
            judgment_purpose: Purpose::PlaybookCall,
            commands: names(&["message.send", "work.read"]),
            events: names(&["aq.work.changed"]),
            capabilities: names(&["message.send", "work.read"]),
            facts: vec![],
            revision: 1,
        }
    }

    fn files(previous: Option<&PreviousPlaybook>) -> BTreeMap<String, String> {
        let facts = BTreeMap::from([("owner".to_string(), json!("user:owner-1"))]);
        let catalog = docs::catalog_registry();
        workspace_files(&notify(), "Ask for it.\n", &catalog, &facts, "v1", previous).unwrap()
    }

    #[test]
    fn the_workspace_offers_only_the_commands_the_record_allows() {
        let files = files(None);
        let catalog: Registry = serde_json::from_str(&files["catalog.json"]).unwrap();
        let commands: Vec<&str> = catalog.commands.keys().map(String::as_str).collect();
        assert_eq!(commands, ["message.send", "work.read"]);
        let events: Vec<&str> = catalog.events.keys().map(String::as_str).collect();
        assert_eq!(events, ["aq.work.changed"]);
        let record: draft::SourceRecord = serde_json::from_str(&files["record.json"]).unwrap();
        assert_eq!(record.facts_version, "v1");
        assert_eq!(files["source.md"], "Ask for it.\n");
        for path in ["GUIDE.md", "LANGUAGE.md", "CATALOG.md", "facts.json"] {
            assert!(files.contains_key(path), "{path}");
        }
        assert!(files["CATALOG.md"].contains("work.read"));
        assert!(!files["CATALOG.md"].contains("work.route"));
        assert!(!files.contains_key("FORMAT.md"));
        assert!(files.contains_key("examples/notify-when-ready/draft.json"));
        assert!(!files.keys().any(|path| path.starts_with("previous")));
        // A name the pinned catalog lacks is refused, not left out.
        let mut wrong = notify();
        wrong.commands.insert("work.delete".into());
        let made = workspace_files(
            &wrong,
            "",
            &docs::catalog_registry(),
            &BTreeMap::new(),
            "v1",
            None,
        );
        assert!(matches!(made, Err(Error::Invalid(_))));
    }

    #[test]
    fn the_instructions_never_mention_routing() {
        assert!(!INSTRUCTIONS.to_lowercase().contains("rout"));
        let schema: Value = serde_json::from_str(OUTCOME_SCHEMA).unwrap();
        assert_eq!(
            schema["properties"]["kind"]["enum"],
            json!(["saved", "question", "unable"])
        );
    }

    #[test]
    fn a_second_run_gets_the_previous_playbook_and_cases() {
        let example = |name: &str| {
            let path = format!("notify-when-ready/{name}");
            let found = docs::example_files().into_iter().find(|(p, _)| *p == path);
            found.unwrap().1
        };
        let artifact: Artifact = serde_json::from_str(example("previous.artifact.json")).unwrap();
        let previous = PreviousPlaybook {
            artifact: artifact.clone(),
            markdown: Some("Before.\n".into()),
            cases: vec![json!({"name": "Task gone!"}), json!({"name": "ok"})],
        };
        let files = files(Some(&previous));
        let kept: Artifact = serde_json::from_str(&files["previous.artifact.json"]).unwrap();
        assert_eq!(kept, artifact);
        assert_eq!(files["previous/source.md"], "Before.\n");
        let draft: Value = serde_json::from_str(&files["previous/draft.json"]).unwrap();
        assert!(draft.get("entries").is_some() && draft.get("id").is_none());
        assert!(files.contains_key("previous/cases/01-task-gone-.json"));
        assert!(files.contains_key("previous/cases/02-ok.json"));
    }

    #[test]
    fn the_request_is_agent_work_internal_work_accepts() {
        let request = request(&notify(), files(None), &Config::default());
        request.validate().unwrap();
        assert_eq!(request.purpose, Purpose::PlaybookCompilation);
        assert_eq!(request.deadline_ms, 1_800_000);
        assert_eq!(request.max_output_bytes, 4_194_304);
        assert!(request.escalate && !request.blocking);
        assert_eq!(request.affected.kind, "playbook_source");
        assert_eq!(request.affected.id, "notify-when-ready");
        assert_eq!(request.agent.unwrap().max_turns, 200);
        assert!(Config::default().valid());
    }
}
