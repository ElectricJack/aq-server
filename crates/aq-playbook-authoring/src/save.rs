//! The server's own check of a save (spec 0013 §4.3). An agent's outcome is
//! a claim: only its draft and its cases are taken from it. The artifact is
//! sealed again here, from the server's record, catalog, facts and document,
//! and the cases are run again on that artifact.
use crate::{Config, SourceRecord, request};
use aq_playbooks_compiler::{ChangeKind, Diagnostic, draft};
use aq_playbooks_model::{Artifact, Registry};
use aq_playbooks_simulation::cases::{Case, CaseReport, MAX_CASES, MIN_CASES, run_cases};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// Why a save was refused: `owner` is a sentence for the owner, without
/// identifiers; `detail` is for the log and for whoever debugs the agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub owner: String,
    pub detail: String,
    /// Set when a case the previous artifact proved no longer passes: the
    /// run is not refused but ends as a question to the owner, whose `owner`
    /// text names the cases.
    pub regressed: Vec<String>,
}

impl Refusal {
    pub(crate) fn new(owner: &str, detail: impl Into<String>) -> Self {
        Self {
            owner: owner.to_owned(),
            detail: detail.into(),
            regressed: vec![],
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
    /// The case files the previous artifact was sealed with.
    pub previous_cases: Vec<Case>,
}

/// A save the server checked itself.
#[derive(Clone, Debug, PartialEq)]
pub struct Saved {
    pub artifact: Artifact,
    /// `{change, cases: {kept, changed, dropped}}` against the previous
    /// artifact; `None` without one.
    pub comparison: Option<Value>,
    /// The cases as the owner reads them: `{name, situation, result, passed}`.
    /// The previous artifact's cases the run did not write again come last.
    pub cases: Value,
    /// The case files the artifact is sealed with: the run's own, then the
    /// previous artifact's cases the run did not write again.
    pub case_files: Vec<Case>,
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

/// What a run did with the cases of the previous artifact, by name.
#[derive(Default)]
struct CaseChanges {
    kept: Vec<String>,
    changed: Vec<String>,
    /// Not written again by the run: they are run against the new draft.
    dropped: Vec<Case>,
}

impl CaseChanges {
    fn between(previous: &[Case], written: &[Case]) -> Self {
        let mut changes = Self::default();
        for old in previous {
            match written.iter().find(|c| c.name == old.name) {
                Some(new) if new == old => changes.kept.push(old.name.clone()),
                Some(_) => changes.changed.push(old.name.clone()),
                None => changes.dropped.push(old.clone()),
            }
        }
        changes
    }

    fn view(&self) -> Value {
        let dropped: Vec<&str> = self.dropped.iter().map(|c| c.name.as_str()).collect();
        json!({"kept": self.kept, "changed": self.changed, "dropped": dropped})
    }
}

/// The question for the owner when cases the previous artifact proved no
/// longer pass: it names each of them.
fn regressed(failed: &[&CaseReport]) -> Refusal {
    const LISTED: usize = 10;
    let mut named: Vec<String> = (failed.iter().take(LISTED))
        .map(|r| format!("“{}” ({})", r.name, r.situation))
        .collect();
    if failed.len() > LISTED {
        named.push(format!("and {} more", failed.len() - LISTED));
    }
    let owner = format!(
        "The new playbook no longer does what the previous one was shown to do in: {}. Should this change alter that? If so, say what the playbook must do there now; if not, the document must keep it.",
        named.join(", ")
    );
    let detail = (failed.iter())
        .map(|r| format!("case {:?}: {}", r.name, r.problems.join(" ")))
        .collect::<Vec<_>>()
        .join("; ");
    Refusal {
        owner,
        detail,
        regressed: failed.iter().map(|r| r.name.clone()).collect(),
    }
}

/// Runs the previous artifact's cases the run did not write again against
/// the new `artifact`, within the total transition budget of each batch.
/// A batch is padded to the engine's least number of cases with cases of the
/// run itself, which already passed.
fn run_dropped(
    artifact: &Artifact,
    dropped: &[Case],
    written: &[Case],
    config: &Config,
) -> Result<Vec<CaseReport>, Refusal> {
    let mut reports = vec![];
    for batch in dropped.chunks(MAX_CASES) {
        let mut cases = batch.to_vec();
        cases.extend(
            written
                .iter()
                .take(MIN_CASES.saturating_sub(batch.len()))
                .cloned(),
        );
        let ran = run_cases(artifact, &cases, config.case_transitions).map_err(|e| {
            Refusal::new(
                "The example situations of the previous playbook could not be run again.",
                format!("{e:#}"),
            )
        })?;
        reports.extend(ran.into_iter().take(batch.len()));
    }
    Ok(reports)
}

/// Seals `draft` from the server's own inputs and checks the artifact, its
/// abilities and its cases. Every case of the previous artifact stays a case
/// of this one: the ones the run did not write again are run against the new
/// draft and kept, and one that no longer passes refuses the save as a
/// question. Blocking: it runs the engine.
pub fn check_saved(
    draft: &Value,
    cases: &[Case],
    inputs: &Inputs,
    config: &Config,
) -> Result<Saved, Refusal> {
    let artifact = seal(draft, inputs)?;
    let previous_cases: &[Case] = match &inputs.previous {
        Some(_) => &inputs.previous_cases,
        None => &[],
    };
    let reports = run_cases(&artifact, cases, config.case_transitions).map_err(|e| {
        Refusal::new(
            "The example situations saved with the playbook could not be run.",
            format!("{e:#}"),
        )
    })?;
    let failed: Vec<&CaseReport> = reports.iter().filter(|r| !r.passed).collect();
    // A case the previous artifact proved, written again unchanged, is a
    // regression; a case the run wrote anew is the agent's claim to correct.
    let (regressions, own): (Vec<&CaseReport>, Vec<&CaseReport>) =
        failed.into_iter().partition(|r| {
            (previous_cases.iter()).any(|old| old.name == r.name && cases.contains(old))
        });
    if let Some(failed) = own.first() {
        return Err(Refusal::new(
            &format!(
                "One of the example situations saved with the playbook does not run as it says: “{}”.",
                failed.situation
            ),
            format!("case {:?}: {}", failed.name, failed.problems.join(" ")),
        ));
    }
    let changes = CaseChanges::between(previous_cases, cases);
    if changes.dropped.len() > config.regression_cases {
        return Err(Refusal::new(
            "The playbook leaves out more example situations of the previous playbook than the server keeps. Write the ones that still matter again.",
            format!(
                "{} previous cases not written again; regression_cases is {}",
                changes.dropped.len(),
                config.regression_cases
            ),
        ));
    }
    let carried = run_dropped(&artifact, &changes.dropped, cases, config)?;
    let carried_failed: Vec<&CaseReport> = carried.iter().filter(|r| !r.passed).collect();
    let mut regressed_cases: Vec<&CaseReport> = regressions;
    regressed_cases.extend(carried_failed);
    if !regressed_cases.is_empty() {
        return Err(regressed(&regressed_cases));
    }
    let comparison = match &inputs.previous {
        None => None,
        Some(previous) => Some(compare(previous, &artifact, &changes)?),
    };
    let cases_view = reports.iter().chain(&carried).map(case_view).collect();
    let mut case_files = cases.to_vec();
    case_files.extend(changes.dropped);
    Ok(Saved {
        artifact,
        comparison,
        cases: cases_view,
        case_files,
    })
}

/// `{change, cases}`: `change` is `none`, `wording`, `behaviour`, or
/// `abilities` when the new playbook may do something the previous one could
/// not; `cases` lists the previous cases kept, changed and dropped.
fn compare(previous: &Artifact, next: &Artifact, cases: &CaseChanges) -> Result<Value, Refusal> {
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
    Ok(json!({"change": change, "cases": cases.view()}))
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
            previous_cases: vec![],
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
        same.previous_cases = cases();
        let compared = check_saved(&draft(), &cases(), &same, &Config::default()).unwrap();
        let kept: Vec<String> = cases().into_iter().map(|c| c.name).collect();
        let expected =
            json!({"change": "none", "cases": {"kept": kept, "changed": [], "dropped": []}});
        assert_eq!(compared.comparison, Some(expected));
        assert_eq!(compared.case_files, cases());
    }

    /// An inputs whose previous artifact was sealed with every worked case.
    fn after_a_save() -> Inputs {
        let mut after = inputs();
        let saved = check_saved(&draft(), &cases(), &after, &Config::default()).unwrap();
        after.previous = Some(saved.artifact);
        after.previous_cases = cases();
        after
    }

    #[test]
    fn cases_the_new_run_did_not_write_are_run_again_and_kept() {
        let after = after_a_save();
        let all = cases();
        assert!(all.len() > MIN_CASES);
        let fewer = &all[..MIN_CASES];
        let saved = check_saved(&draft(), fewer, &after, &Config::default()).unwrap();
        // The new artifact lists every case of the previous one, and so do its files.
        let listed: Vec<&str> = (saved.cases.as_array().unwrap().iter())
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        let names: Vec<&str> = all.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(listed, names);
        assert!(
            saved
                .cases
                .as_array()
                .unwrap()
                .iter()
                .all(|c| c["passed"] == true)
        );
        assert_eq!(saved.case_files, all);
        let comparison = saved.comparison.unwrap();
        assert_eq!(comparison["change"], "none");
        let named = |key: &str| -> Vec<String> {
            serde_json::from_value(comparison["cases"][key].clone()).unwrap()
        };
        assert_eq!(named("kept"), names[..MIN_CASES]);
        assert_eq!(named("changed"), Vec::<String>::new());
        assert_eq!(named("dropped"), names[MIN_CASES..]);
        // A case written again with another expectation is changed, not dropped.
        let mut altered = all.clone();
        altered[0].situation = "Another way to say it".into();
        let saved =
            check_saved(&draft(), &altered[..MIN_CASES], &after, &Config::default()).unwrap();
        assert_eq!(
            saved.comparison.unwrap()["cases"]["changed"],
            json!([all[0].name])
        );
    }

    #[test]
    fn a_previous_case_that_no_longer_passes_is_a_question_naming_it() {
        let mut after = after_a_save();
        let all = cases();
        // A dropped case that the new draft does not satisfy.
        let last = after.previous_cases.len() - 1;
        after.previous_cases[last].expect.outcome = "no-such-outcome".into();
        let asked =
            check_saved(&draft(), &all[..MIN_CASES], &after, &Config::default()).unwrap_err();
        assert_eq!(asked.regressed, [all[last].name.clone()]);
        assert!(asked.owner.contains(&all[last].name), "{}", asked.owner);
        // The same case written again with a new expectation is the run's claim.
        let mut written = all[..MIN_CASES].to_vec();
        let mut again = all[last].clone();
        again.expect.outcome = "another-no-such-outcome".into();
        written.push(again);
        let refused = check_saved(&draft(), &written, &after, &Config::default());
        assert!(refused.unwrap_err().regressed.is_empty());
        // A case written again unchanged that fails is a regression as well.
        let mut broken = all.clone();
        broken[0].expect.outcome = "no-such-outcome".into();
        after.previous_cases = broken.clone();
        let asked = check_saved(&draft(), &broken, &after, &Config::default()).unwrap_err();
        assert_eq!(asked.regressed, [all[0].name.clone()]);
    }

    #[test]
    fn leaving_out_more_cases_than_the_server_carries_is_refused() {
        let after = after_a_save();
        let config = Config {
            regression_cases: 1,
            ..Config::default()
        };
        let refused = check_saved(&draft(), &cases()[..MIN_CASES], &after, &config).unwrap_err();
        assert!(refused.regressed.is_empty());
        assert!(refused.detail.contains("regression_cases"), "{refused:?}");
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
