//! The server's own check of a save (spec 0013 §4.3). An agent's outcome is
//! a claim: only its draft and its cases are taken from it. The artifact is
//! sealed again here, from the server's record, catalog, facts and document,
//! and the cases are run again on that artifact.
use crate::{Config, SourceRecord, request};
use aq_playbooks_compiler::{ChangeKind, Diagnostic, draft};
use aq_playbooks_model::{Artifact, Registry};
use aq_playbooks_simulation::cases::{Case, CaseReport, run_cases};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// Why a save was refused: `owner` is a sentence for the owner, without
/// identifiers; `detail` is for the log and for whoever debugs the agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub owner: String,
    pub detail: String,
}

impl Refusal {
    fn new(owner: &str, detail: impl Into<String>) -> Self {
        Self {
            owner: owner.to_owned(),
            detail: detail.into(),
        }
    }
}

/// What the server holds about a source when it checks a save.
#[derive(Clone, Debug)]
pub struct Inputs {
    pub record: SourceRecord,
    pub markdown: String,
    /// The pinned catalog, whole.
    pub catalog: Registry,
    pub facts: BTreeMap<String, Value>,
    pub facts_version: String,
    /// What the source's grant allows.
    pub grant_capabilities: BTreeSet<String>,
    /// The artifact in effect now, to compare with.
    pub previous: Option<Artifact>,
}

/// A save the server checked itself.
#[derive(Clone, Debug, PartialEq)]
pub struct Saved {
    pub artifact: Artifact,
    /// `{change}` against the previous artifact; `None` without one.
    pub comparison: Option<Value>,
    /// The cases as the owner reads them: `{name, situation, result, passed}`.
    pub cases: Value,
}

const DOES_NOT_CHECK: &str = "The playbook the agent saved does not pass the server's check of it.";

fn seal(draft: &Value, inputs: &Inputs) -> Result<Artifact, Refusal> {
    let record = &inputs.record;
    let catalog = request::catalog_for(record, &inputs.catalog).map_err(|e| {
        Refusal::new(
            "The server's own description of this source names a command or event it does not have.",
            e.to_string(),
        )
    })?;
    let ws = draft::Workspace {
        markdown: inputs.markdown.clone(),
        record: request::engine_record(record, &inputs.facts_version),
        catalog: catalog.clone(),
        facts: inputs.facts.clone(),
        draft: draft.clone(),
    };
    let artifact =
        draft::seal(&ws).map_err(|faults| Refusal::new(DOES_NOT_CHECK, first(&faults)))?;
    artifact
        .verify()
        .map_err(|e| Refusal::new(DOES_NOT_CHECK, e.to_string()))?;
    verify_contract(&artifact, &catalog, inputs)?;
    Ok(artifact)
}

fn verify_contract(
    artifact: &Artifact,
    catalog: &Registry,
    inputs: &Inputs,
) -> Result<(), Refusal> {
    let record = &inputs.record;
    let content = &artifact.content;
    let pinned = (content.registry.commands.iter())
        .all(|(name, c)| catalog.commands.get(name) == Some(c))
        && (content.registry.events.iter()).all(|(name, e)| catalog.events.get(name) == Some(e));
    if !pinned {
        return Err(Refusal::new(
            DOES_NOT_CHECK,
            "a contract is not the pinned catalog's",
        ));
    }
    let identity_matches =
        content.playbook.id == record.binding && content.source.header.id == record.binding;
    let scope_matches = content.playbook.scope == record.scope && content.dependencies.is_empty();
    if !identity_matches || !scope_matches {
        return Err(Refusal::new(
            DOES_NOT_CHECK,
            "identity, scope or dependencies differ from the record",
        ));
    }
    verify_capabilities(artifact, inputs)
}

fn verify_capabilities(artifact: &Artifact, inputs: &Inputs) -> Result<(), Refusal> {
    let record = &inputs.record;
    let needed = &artifact.content.playbook.required_capabilities;
    let missing: Vec<&String> = (needed.iter())
        .filter(|c| !record.capabilities.contains(*c) || !inputs.grant_capabilities.contains(*c))
        .collect();
    if !missing.is_empty() {
        return Err(Refusal::new(
            "The playbook the agent saved needs an ability this source has not been given.",
            format!("capabilities not granted: {missing:?}"),
        ));
    }
    Ok(())
}

fn first(faults: &[Diagnostic]) -> String {
    let texts: Vec<String> = (faults.iter().take(5))
        .map(|d| format!("{} at {}: {}", d.code, d.path, d.message))
        .collect();
    texts.join("; ")
}

/// Seals `draft` from the server's own inputs and checks the artifact, its
/// abilities and its cases. Blocking: it runs the engine.
pub fn check_saved(
    draft: &Value,
    cases: &[Case],
    inputs: &Inputs,
    config: &Config,
) -> Result<Saved, Refusal> {
    let artifact = seal(draft, inputs)?;
    let reports = run_cases(&artifact, cases, config.case_transitions).map_err(|e| {
        Refusal::new(
            "The example situations saved with the playbook could not be run.",
            format!("{e:#}"),
        )
    })?;
    if let Some(failed) = reports.iter().find(|r| !r.passed) {
        return Err(Refusal::new(
            &format!(
                "One of the example situations saved with the playbook does not run as it says: “{}”.",
                failed.situation
            ),
            format!("case {:?}: {}", failed.name, failed.problems.join(" ")),
        ));
    }
    let comparison = match &inputs.previous {
        None => None,
        Some(previous) => Some(compare(previous, &artifact)?),
    };
    let cases = reports.iter().map(case_view).collect();
    Ok(Saved {
        artifact,
        comparison,
        cases,
    })
}

/// `{change}`: `none`, `wording`, `behaviour`, or `abilities` when the new
/// playbook may do something the previous one could not.
fn compare(previous: &Artifact, next: &Artifact) -> Result<Value, Refusal> {
    let compared = draft::compare(previous, next).map_err(|e| {
        Refusal::new(
            "The new playbook could not be compared with the one in effect.",
            e.to_string(),
        )
    })?;
    let change = match compared.change {
        _ if compared.abilities_changed => "abilities",
        ChangeKind::Identical => "none",
        ChangeKind::Presentation => "wording",
        ChangeKind::Executable | ChangeKind::Interface => "behaviour",
    };
    Ok(json!({"change": change}))
}

fn words(name: &str) -> String {
    name.replace(['.', '_'], " ")
}

/// A value in words, when it has any: text, numbers, and lists of them.
/// Objects are mostly identities, so only their plain fields are kept.
fn value_words(value: &Value, nested: bool) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(text) if text.is_empty() => None,
        Value::String(text) => Some(text.clone()),
        Value::Bool(yes) => Some(if *yes { "yes" } else { "no" }.to_owned()),
        Value::Number(n) => Some(n.to_string()),
        Value::Array(items) => {
            let said: Vec<String> = (items.iter())
                .filter_map(|item| value_words(item, false))
                .collect();
            (!said.is_empty()).then(|| said.join(", "))
        }
        Value::Object(_) if nested => None,
        Value::Object(fields) => {
            let said: Vec<String> = (fields.iter())
                .filter_map(|(key, v)| Some(format!("{} {}", words(key), value_words(v, true)?)))
                .collect();
            (!said.is_empty()).then(|| said.join(" "))
        }
    }
}

/// What a case came to, as a sentence: the last thing the playbook did and
/// how the run ended.
pub fn result_sentence(report: &CaseReport) -> String {
    let ended = match &report.outcome {
        Some(outcome) => format!("Ends as “{}”.", words(outcome)),
        None => "Does not end.".to_owned(),
    };
    let Some(last) = report.calls.last() else {
        return format!("Nothing is done. {ended}");
    };
    let mut said: Vec<String> = vec![];
    if let Value::Object(fields) = &last.input {
        for (key, value) in fields {
            if let Some(text) = value_words(value, true) {
                said.push(format!("{}: {text}", words(key)));
            }
        }
    }
    let mut action = words(&last.command);
    if let Some(first) = action.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    match said.is_empty() {
        true => format!("{action}. {ended}"),
        false => format!("{action} ({}). {ended}", said.join("; ")),
    }
}

fn case_view(report: &CaseReport) -> Value {
    json!({
        "name": report.name,
        "situation": report.situation,
        "result": result_sentence(report),
        "passed": report.passed,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::SourceKind;
    use aq_internal_work::Purpose;
    use aq_playbooks_compiler::docs;
    use aq_playbooks_simulation::cases::Call;

    fn example(path: &str) -> &'static str {
        let path = format!("notify-when-ready/{path}");
        let found = docs::example_files().into_iter().find(|(p, _)| *p == path);
        found.unwrap().1
    }

    fn cases() -> Vec<Case> {
        (docs::example_files().into_iter())
            .filter(|(path, _)| path.starts_with("notify-when-ready/cases/"))
            .map(|(_, text)| serde_json::from_str(text).unwrap())
            .collect()
    }

    fn inputs() -> Inputs {
        let record: draft::SourceRecord = serde_json::from_str(example("record.json")).unwrap();
        let capabilities = record.capabilities.clone();
        Inputs {
            record: SourceRecord {
                id: record.id,
                kind: SourceKind::Registered,
                binding: record.binding,
                scope: record.scope,
                purpose: record.purpose,
                judgment_purpose: Purpose::PlaybookCall,
                commands: record.commands,
                events: record.events,
                capabilities: capabilities.clone(),
                facts: vec![],
                revision: 1,
            },
            markdown: example("source.md").to_owned(),
            catalog: docs::catalog_registry(),
            facts: serde_json::from_str(example("facts.json")).unwrap(),
            facts_version: record.facts_version,
            grant_capabilities: capabilities,
            previous: None,
        }
    }

    fn draft() -> Value {
        serde_json::from_str(example("draft.json")).unwrap()
    }

    #[test]
    fn the_worked_example_passes_the_servers_check() {
        let saved = check_saved(&draft(), &cases(), &inputs(), &Config::default()).unwrap();
        assert_eq!(saved.artifact.content.playbook.id, "aq.notify-when-ready");
        assert_eq!(saved.comparison, None);
        let listed = saved.cases.as_array().unwrap();
        assert_eq!(listed.len(), cases().len());
        for case in listed {
            assert_eq!(case["passed"], true);
            let result = case["result"].as_str().unwrap();
            assert!(result.contains("Ends as"), "{result}");
            assert!(!result.contains("sha256:"), "{result}");
        }
        // Sealing is the server's: the same inputs give the same artifact.
        let again = check_saved(&draft(), &cases(), &inputs(), &Config::default()).unwrap();
        assert_eq!(again.artifact.digest, saved.artifact.digest);
        // Against itself nothing changed.
        let mut same = inputs();
        same.previous = Some(saved.artifact.clone());
        let compared = check_saved(&draft(), &cases(), &same, &Config::default()).unwrap();
        assert_eq!(compared.comparison, Some(json!({"change": "none"})));
    }

    #[test]
    fn a_save_whose_case_does_not_run_as_stated_is_refused() {
        let mut cases = cases();
        cases[0].expect.outcome = "no-such-outcome".into();
        let refused = check_saved(&draft(), &cases, &inputs(), &Config::default()).unwrap_err();
        assert!(refused.owner.contains("does not run as it says"));
        assert!(refused.owner.contains(&cases[0].situation));
        // Too few cases is a refusal as well.
        let few = &cases[1..3];
        let refused = check_saved(&draft(), few, &inputs(), &Config::default()).unwrap_err();
        assert!(refused.owner.contains("could not be run"), "{refused:?}");
    }

    #[test]
    fn a_save_needing_an_ability_the_grant_does_not_give_is_refused() {
        let mut inputs = inputs();
        inputs.grant_capabilities.remove("message.send");
        let refused = check_saved(&draft(), &cases(), &inputs, &Config::default()).unwrap_err();
        assert!(refused.owner.contains("ability"), "{refused:?}");
        assert!(refused.detail.contains("message.send"));
    }

    #[test]
    fn a_draft_that_does_not_seal_is_refused_in_the_owners_words() {
        let mut draft = draft();
        draft["entries"]["main"]["start"] = json!("nowhere");
        let refused = check_saved(&draft, &cases(), &inputs(), &Config::default()).unwrap_err();
        assert!(refused.owner.contains("does not pass"));
        assert!(!refused.owner.contains("nowhere"));
        assert!(!refused.detail.is_empty());
    }

    #[test]
    fn a_result_is_a_sentence_about_the_last_call() {
        let report = |calls: Vec<Call>| CaseReport {
            name: "n".into(),
            situation: "s".into(),
            passed: true,
            outcome: Some("task_gone".into()),
            calls,
            problems: vec![],
        };
        assert_eq!(
            result_sentence(&report(vec![])),
            "Nothing is done. Ends as “task gone”."
        );
        let call = Call {
            step: "route".into(),
            command: "work.route".into(),
            input: json!({"subject": {"id": "t"}, "level": "complex-high",
                "harnesses": ["claude", "codex"], "reasons": [{"code": "risk", "detail": ""}]}),
            outcome: Some("routed".into()),
        };
        assert_eq!(
            result_sentence(&report(vec![call])),
            "Work route (harnesses: claude, codex; level: complex-high; reasons: code risk). Ends as “task gone”."
        );
    }
}
