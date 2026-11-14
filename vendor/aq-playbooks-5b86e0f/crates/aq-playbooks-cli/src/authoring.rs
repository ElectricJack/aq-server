//! The authoring agent's tools: `check`, `seal`, `try`, `compare`, `facts`,
//! `save`, `ask` and `unable`. They work offline, on the workspace the host
//! wrote, and print JSON for an agent that has no one to ask.
//!
//! Paths. The workspace is the current directory, and only the workspace
//! directory itself: a command run anywhere else, a subdirectory of the
//! workspace included, refuses to run. Every path a command takes is
//! relative to the workspace and stays inside it: absolute paths, `..`,
//! symbolic links (at any depth) and files with other hard links are
//! refused, and a file is checked again after it is opened.
//!
//! Outcome. `save`, `ask` and `unable` each write only `out/outcome.json`
//! (at most `MAX_OUTCOME_BYTES`, 4 MiB), whole, and nothing else. A run
//! ends once: each of them refuses when `out/outcome.json` already exists. The outcome is data the host checks again,
//! never an instruction.
use anyhow::{Context, Result, anyhow, bail};
use aq_playbooks_compiler::Diagnostic;
use aq_playbooks_compiler::draft::{
    self, CATALOG_FILE, DRAFT_FILE, FACTS_FILE, RECORD_FILE, SOURCE_FILE, SourceRecord, Workspace,
};
use aq_playbooks_model::{Artifact, Registry, canonical_bytes};
use aq_playbooks_simulation::{
    CASES_TRANSITION_BUDGET, Case, CaseReport, HOST_FILES, MAX_CASES, MAX_OUTCOME_BYTES,
    MAX_QUESTION_BYTES, MAX_REASON_BYTES, MAX_SENTENCES, MIN_CASES, OUTCOME_FILE, Outcome,
    Sentence, facts_digest, quote, run_case, run_cases,
};
use clap::Subcommand;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

const CASES_DIR: &str = "cases";
/// What `seal --output` never writes over: the host's files and the outcome.
const HOST_INPUTS: [&str; 9] = [
    SOURCE_FILE,
    RECORD_FILE,
    CATALOG_FILE,
    FACTS_FILE,
    DRAFT_FILE,
    PREVIOUS_FILE,
    "previous",
    CASES_DIR,
    "out",
];
const PREVIOUS_FILE: &str = "previous.artifact.json";
const MAX_CASE_BYTES: u64 = 65_536;
const MAX_SOURCE_BYTES: u64 = 4_194_304;
const MAX_RECORD_BYTES: u64 = 65_536;
const MAX_FACTS_BYTES: u64 = 1_048_576;
const MAX_CATALOG_BYTES: u64 = 16_777_216;
const MAX_ARTIFACT_BYTES: u64 = 16_777_216;
const ORDER: &str = "Faults come in two stages. First, the faults that keep the tool from filling in the draft (unknown command or event names, identities, capabilities, calls, source_map lines); only those are listed while there are any. Once the draft fills, every validation fault is listed at once.";
const FINISHED: &str = "The run is finished: end your work now. The host checks the outcome again.";

#[derive(Subcommand)]
pub enum Authoring {
    /// Check draft.json against the workspace. Lists the faults that keep the
    /// draft from being filled in first; once it fills, every validation
    /// fault. Exits 1 when there is any.
    Check,
    /// Seal the draft into an artifact and print its digest; with --output,
    /// also write it (a path inside the workspace).
    Seal {
        #[arg(long)]
        output: Option<String>,
    },
    /// Run one case file (such as cases/bugfix.json) against the draft with
    /// a fake host, and print what happened. Exits 1 when the case fails.
    Try { case: String },
    /// Compare the draft with previous.artifact.json, the playbook active
    /// now, in the owner's terms; {"previous": null} when there is none.
    Compare,
    /// Print the fixed facts the host placed in facts.json, with their digest.
    Facts,
    /// Check, seal, run every cases/*.json (5 to 40) and, when all pass,
    /// write out/outcome.json. A refused save writes nothing.
    Save,
    /// Ask the owner a question about the document and end. Name each
    /// sentence it is about by its source.md line (`--line N`) or, when it
    /// spans lines, by its first and last line (`--lines A-B`, at most 8
    /// lines): 1 to 16 sentences, recorded in document order.
    Ask {
        #[arg(long = "line")]
        line: Vec<u32>,
        #[arg(long = "lines")]
        lines: Vec<String>,
        question: String,
    },
    /// Give up: the document cannot be turned into a playbook because of the
    /// sentence on this source.md line (`--line N`) or these lines
    /// (`--lines A-B`), for this reason. For a fault in one of the host's
    /// own files, name that file instead (`--file catalog.json`).
    Unable {
        #[arg(long = "line")]
        line: Option<u32>,
        #[arg(long = "lines")]
        lines: Option<String>,
        #[arg(long = "file")]
        file: Option<String>,
        reason: String,
    },
}

/// Runs an authoring command, prints its JSON, and exits 1 when it refused.
pub fn run(command: Authoring) -> Result<()> {
    let (ok, value) = match execute(command) {
        Ok(result) => result,
        Err(error) => (false, json!({"error": format!("{error:#}")})),
    };
    println!("{}", serde_json::to_string_pretty(&value)?);
    if !ok {
        std::process::exit(1);
    }
    Ok(())
}

fn execute(command: Authoring) -> Result<(bool, Value)> {
    let root = workspace_root()?;
    match command {
        Authoring::Check => Ok(match read_workspace(&root) {
            Ok(ws) => {
                let (stage, diagnostics) = match draft::fill(&ws) {
                    Err(faults) => ("filling", faults),
                    Ok(_) => ("validation", draft::check(&ws)),
                };
                checked(stage, diagnostics)
            }
            Err(refusal) => refusal,
        }),
        Authoring::Seal { output } => {
            let (_, artifact) = match sealed(&root) {
                Ok(sealed) => sealed,
                Err(refusal) => return Ok(refusal),
            };
            if let Some(output) = &output {
                let target = inside(&root, output)?;
                let given = target.strip_prefix(&root).unwrap_or(&target);
                anyhow::ensure!(
                    !HOST_INPUTS.iter().any(|input| given.starts_with(input)),
                    "{output}: seal does not overwrite what the host gave the workspace or what save, ask and unable write ({}); write the artifact to a new name, such as sealed.artifact.json.",
                    HOST_INPUTS.join(", ")
                );
                write_inside(&root, output, &canonical_bytes(&artifact)?)?;
            }
            let registry = &artifact.content.registry;
            Ok((
                true,
                json!({
                    "artifact": artifact.digest,
                    "executable_fingerprint": artifact.executable_fingerprint,
                    "required_capabilities": artifact.content.playbook.required_capabilities,
                    "commands": registry.commands.keys().collect::<Vec<_>>(),
                    "events": registry.events.keys().collect::<Vec<_>>(),
                    "written": output,
                }),
            ))
        }
        Authoring::Try { case } => {
            let parsed = read_case(&root, &case)?;
            let (_, artifact) = match sealed(&root) {
                Ok(sealed) => sealed,
                Err(refusal) => return Ok(refusal),
            };
            let report = run_case(&artifact, &parsed)?;
            Ok((report.passed, serde_json::to_value(report)?))
        }
        Authoring::Compare => {
            let (_, next) = match sealed(&root) {
                Ok(sealed) => sealed,
                Err(refusal) => return Ok(refusal),
            };
            if !exists(&root, PREVIOUS_FILE) {
                return Ok((true, json!({"previous": null})));
            }
            let previous: Artifact = parse_json(
                PREVIOUS_FILE,
                &read_inside(&root, PREVIOUS_FILE, MAX_ARTIFACT_BYTES)?,
            )?;
            previous
                .verify()
                .map_err(|e| anyhow!("{PREVIOUS_FILE}: is not a sound artifact: {e}"))?;
            Ok((
                true,
                serde_json::to_value(draft::compare(&previous, &next)?)?,
            ))
        }
        Authoring::Facts => {
            let record: SourceRecord = parse_json(
                RECORD_FILE,
                &read_inside(&root, RECORD_FILE, MAX_RECORD_BYTES)?,
            )?;
            let facts: BTreeMap<String, Value> = parse_json(
                FACTS_FILE,
                &read_inside(&root, FACTS_FILE, MAX_FACTS_BYTES)?,
            )?;
            Ok((
                true,
                json!({
                    "facts_version": record.facts_version,
                    "digest": facts_digest(&facts)?,
                    "facts": facts,
                    "use": "Read a fact in the draft as {\"op\": \"ref\", \"root\": \"definitions\", \"path\": [\"<name>\", …]}.",
                }),
            ))
        }
        Authoring::Save => save(&root),
        Authoring::Ask {
            line,
            lines,
            question,
        } => {
            let named = line.len() + lines.len();
            anyhow::ensure!(
                (1..=MAX_SENTENCES).contains(&named),
                "--line and --lines name {named} sentences; a question names 1 to {MAX_SENTENCES} sentences of source.md. Name only the sentences the question is about."
            );
            bounded("the question", &question, MAX_QUESTION_BYTES)?;
            let mut ranges: Vec<(u32, Option<u32>)> = line.iter().map(|n| (*n, None)).collect();
            for range in &lines {
                ranges.push(parse_range(range)?);
            }
            ranges.sort();
            for pair in ranges.windows(2) {
                let ((a, a_end), (b, b_end)) = (pair[0], pair[1]);
                anyhow::ensure!(
                    b > a_end.unwrap_or(a),
                    "{} and {} overlap; name each sentence once, by lines of its own.",
                    shown(a, a_end),
                    shown(b, b_end)
                );
            }
            let source = read_inside(&root, SOURCE_FILE, MAX_SOURCE_BYTES)?;
            let sentences = ranges
                .into_iter()
                .map(|(line, end)| {
                    let sentence = quote(&source, line, end).map_err(anyhow::Error::msg)?;
                    whole(&source, line, end)?;
                    Ok(sentence)
                })
                .collect::<Result<Vec<_>>>()?;
            not_ended(&root)?;
            finish(
                &root,
                Outcome::Question {
                    question,
                    sentences,
                },
            )
        }
        Authoring::Unable {
            line,
            lines,
            file,
            reason,
        } => {
            bounded("the reason", &reason, MAX_REASON_BYTES)?;
            if let Some(file) = file {
                anyhow::ensure!(
                    line.is_none() && lines.is_none(),
                    "--file names a fault in one of the host's files; do not name a sentence too."
                );
                anyhow::ensure!(
                    HOST_FILES.contains(&file.as_str()),
                    "--file {file}: name one of the host's files ({}); a fault in your draft or cases is yours to fix.",
                    HOST_FILES.join(", ")
                );
                not_ended(&root)?;
                return finish(
                    &root,
                    Outcome::Unable {
                        reason,
                        line: None,
                        end_line: None,
                        sentence: None,
                        file: Some(file),
                    },
                );
            }
            let (line, end) = match (line, lines) {
                (Some(line), None) => (line, None),
                (None, Some(range)) => parse_range(&range)?,
                _ => bail!(
                    "name the sentence once: --line N for one line, or --lines A-B for a sentence over lines A to B (or --file for a fault in one of the host's files)."
                ),
            };
            let source = read_inside(&root, SOURCE_FILE, MAX_SOURCE_BYTES)?;
            let Sentence {
                line,
                end_line,
                text,
            } = quote(&source, line, end).map_err(anyhow::Error::msg)?;
            whole(&source, line, end)?;
            not_ended(&root)?;
            finish(
                &root,
                Outcome::Unable {
                    reason,
                    line: Some(line),
                    end_line,
                    sentence: Some(text),
                    file: None,
                },
            )
        }
    }
}

fn save(root: &Path) -> Result<(bool, Value)> {
    not_ended(root)?;
    let refused = |reasons: Vec<String>, extra: Value| {
        let mut value = json!({
            "saved": false,
            "reasons": reasons,
            "next": "Nothing was written. Fix each reason, check and try again, then run `aq-playbooks save` again.",
        });
        if let (Some(value), Value::Object(extra)) = (value.as_object_mut(), extra) {
            value.extend(extra);
        }
        Ok((false, value))
    };
    let (ws, artifact) = match sealed(root) {
        Ok(sealed) => sealed,
        Err((_, check)) => {
            return refused(
                vec!["the draft does not check; the faults are under `diagnostics`.".into()],
                check,
            );
        }
    };
    let mut reasons = vec![];
    let needed = &artifact.content.playbook.required_capabilities;
    let missing: Vec<&String> = needed.difference(&ws.record.capabilities).collect();
    if !missing.is_empty() {
        reasons.push(format!(
            "the playbook needs the capabilities {missing:?}, which record.json does not give this source ({:?}). Use only commands whose capabilities it gives.",
            ws.record.capabilities
        ));
    }
    let (files, cases) = match read_cases(root) {
        Ok(read) => read,
        Err(faults) => {
            reasons.extend(faults);
            (vec![], vec![])
        }
    };
    let mut reports: Vec<CaseReport> = vec![];
    if reasons.is_empty() {
        match run_cases(&artifact, &cases, CASES_TRANSITION_BUDGET) {
            Ok(done) => reports = done,
            Err(e) => reasons.push(format!("{e:#}")),
        }
    }
    for (file, report) in files.iter().zip(&reports) {
        if !report.passed {
            reasons.push(format!(
                "case {:?} ({file}) does not run as it says: {}",
                report.name,
                report.problems.join(" ")
            ));
        }
    }
    if !reasons.is_empty() {
        return refused(reasons, json!({"cases": reports}));
    }
    let outcome = Outcome::Saved {
        draft: ws.draft.clone(),
        cases,
        artifact: artifact.digest.clone(),
        facts_digest: facts_digest(&ws.facts)?,
    };
    let bytes = match outcome.to_bytes() {
        Ok(bytes) => bytes,
        Err(e) => return refused(vec![format!("{e:#}")], json!({})),
    };
    write_inside(root, OUTCOME_FILE, &bytes)?;
    let table: Vec<Value> = reports
        .iter()
        .map(|r| json!({"name": r.name, "situation": r.situation, "outcome": r.outcome}))
        .collect();
    Ok((
        true,
        json!({
            "saved": true,
            "written": OUTCOME_FILE,
            "artifact": artifact.digest,
            "facts_digest": facts_digest(&ws.facts)?,
            "cases": table,
            "next": FINISHED,
        }),
    ))
}

/// Refuses a second ending: `save`, `ask` and `unable` each end the run, and
/// only the first one counts.
fn not_ended(root: &Path) -> Result<()> {
    if exists(root, OUTCOME_FILE) {
        let kind = read_inside(root, OUTCOME_FILE, MAX_OUTCOME_BYTES as u64)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|v| v["kind"].as_str().map(String::from))
            .unwrap_or_else(|| "an outcome".into());
        bail!(
            "{OUTCOME_FILE} already holds this run's ending ({kind}); a run ends once. {FINISHED}"
        );
    }
    Ok(())
}

/// Writes the outcome and says so.
fn finish(root: &Path, outcome: Outcome) -> Result<(bool, Value)> {
    write_inside(root, OUTCOME_FILE, &outcome.to_bytes()?)?;
    Ok((
        true,
        json!({"written": OUTCOME_FILE, "outcome": outcome, "next": FINISHED}),
    ))
}

fn checked(stage: &str, diagnostics: Vec<Diagnostic>) -> (bool, Value) {
    let ok = diagnostics.is_empty();
    let next = if ok {
        "The draft checks. Try it against each case with `aq-playbooks try cases/<name>.json`, then `aq-playbooks save`."
    } else {
        "Fix each fault where its `path` points (file:JSON path; `line` and `column` are positions in that file) and run `aq-playbooks check` again."
    };
    (
        ok,
        json!({"ok": ok, "stage": stage, "order": ORDER, "diagnostics": diagnostics, "next": next}),
    )
}
fn read_workspace(root: &Path) -> std::result::Result<Workspace, (bool, Value)> {
    Workspace::read(root).map_err(|faults| checked("reading", faults))
}
/// The workspace and its sealed draft, or the check output that refuses it.
fn sealed(root: &Path) -> std::result::Result<(Workspace, Artifact), (bool, Value)> {
    let ws = read_workspace(root)?;
    match draft::fill(&ws) {
        Err(faults) => Err(checked("filling", faults)),
        Ok(_) => match draft::seal(&ws) {
            Ok(artifact) => Ok((ws, artifact)),
            Err(faults) => Err(checked("validation", faults)),
        },
    }
}

/// The current directory, when it is an authoring workspace.
fn workspace_root() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("the current directory cannot be read")?;
    if is_workspace(&cwd) {
        Ok(cwd)
    } else {
        bail!(
            "{}: this is not an authoring workspace: it has no {RECORD_FILE}. The authoring commands run only in the workspace directory itself (the one holding {SOURCE_FILE}, {RECORD_FILE}, {CATALOG_FILE}, {FACTS_FILE} and {DRAFT_FILE}), not in a subdirectory of it. Change to that directory and run the command again.",
            cwd.display()
        )
    }
}
fn is_workspace(dir: &Path) -> bool {
    fs::symlink_metadata(dir.join(RECORD_FILE)).is_ok_and(|m| m.is_file())
}
fn exists(root: &Path, given: &str) -> bool {
    fs::symlink_metadata(root.join(given)).is_ok()
}

/// `given` inside `root`: relative, without `..`, and through no symbolic
/// link at any depth.
fn inside(root: &Path, given: &str) -> Result<PathBuf> {
    let refuse = || {
        anyhow!(
            "{given}: only a path inside the workspace, relative to it, is accepted (such as cases/bugfix.json); absolute paths and `..` are refused."
        )
    };
    let path = Path::new(given);
    if path.is_absolute() {
        return Err(refuse());
    }
    let mut at = root.to_path_buf();
    let mut named = false;
    for component in path.components() {
        match component {
            Component::Normal(name) => {
                at.push(name);
                named = true;
                if fs::symlink_metadata(&at).is_ok_and(|m| m.file_type().is_symlink()) {
                    bail!(
                        "{given}: {} is a symbolic link; the workspace accepts only its own files and directories, not links. Write a regular file instead.",
                        at.strip_prefix(root).unwrap_or(&at).display()
                    );
                }
            }
            Component::CurDir => {}
            _ => return Err(refuse()),
        }
    }
    if named { Ok(at) } else { Err(refuse()) }
}

/// A regular file of the workspace, at most `max` bytes, as UTF-8: checked
/// before it is opened, and the opened file checked again.
fn read_inside(root: &Path, given: &str, max: u64) -> Result<String> {
    let path = inside(root, given)?;
    let checked = fs::symlink_metadata(&path)
        .map_err(|e| anyhow!("{given}: cannot be read from the workspace ({e})."))?;
    anyhow::ensure!(checked.is_file(), "{given} is not a regular file.");
    let file = fs::File::open(&path).map_err(|e| anyhow!("{given}: cannot be read ({e})."))?;
    let opened = file
        .metadata()
        .map_err(|e| anyhow!("{given}: cannot be read ({e})."))?;
    anyhow::ensure!(opened.is_file(), "{given} is not a regular file.");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        anyhow::ensure!(
            (opened.dev(), opened.ino()) == (checked.dev(), checked.ino()),
            "{given} was replaced while it was being read; write it once and run again."
        );
        anyhow::ensure!(
            opened.nlink() == 1,
            "{given} has other hard links; the workspace accepts only files of its own. Write it as a new file."
        );
    }
    let too_big = || anyhow!("{given} is larger than {max} bytes; keep it under that.");
    if opened.len() > max {
        return Err(too_big());
    }
    let mut bytes = vec![];
    file.take(max + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Err(too_big());
    }
    String::from_utf8(bytes).map_err(|_| anyhow!("{given} is not UTF-8 text."))
}

/// Writes `bytes` to `given` inside `root` whole: to a new file beside it,
/// then renamed over it, so a reader never sees half a file and a link at
/// the target is replaced, never followed.
fn write_inside(root: &Path, given: &str, bytes: &[u8]) -> Result<()> {
    let path = inside(root, given)?;
    let parent = path.parent().unwrap_or(root).to_path_buf();
    let mut at = root.to_path_buf();
    for component in parent
        .strip_prefix(root)
        .unwrap_or(Path::new(""))
        .components()
    {
        at.push(component);
        match fs::symlink_metadata(&at) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => bail!("{given}: {} is not a directory.", at.display()),
            Err(_) => fs::create_dir(&at)
                .map_err(|e| anyhow!("{given}: cannot create {} ({e}).", at.display()))?,
        }
    }
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("{given}: names no file."))?
        .to_string_lossy()
        .to_string();
    let temporary = parent.join(format!(".{name}.{}.tmp", std::process::id()));
    let _ = fs::remove_file(&temporary);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|e| anyhow!("{given}: cannot be written ({e})."))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&temporary, &path).map_err(|e| {
        let _ = fs::remove_file(&temporary);
        anyhow!("{given}: cannot be written ({e}).")
    })
}

fn parse_json<T: serde::de::DeserializeOwned>(file: &str, text: &str) -> Result<T> {
    serde_json::from_str(text).map_err(|e| {
        let full = e.to_string();
        let reason = full
            .rsplit_once(" at line ")
            .map_or(full.as_str(), |(reason, _)| reason);
        anyhow!(
            "{file} line {} column {}: {reason}. Fix it there and run the command again.",
            e.line(),
            e.column()
        )
    })
}
fn read_case(root: &Path, given: &str) -> Result<Case> {
    parse_json::<Case>(given, &read_inside(root, given, MAX_CASE_BYTES)?).map_err(|e| {
        anyhow!(
            "{e} A case is {{\"name\", \"situation\", \"start\", \"returns\", \"expect\"}}; see the case format in GUIDE.md."
        )
    })
}
/// Every `cases/*.json`, by file name, or every fault found reading them.
fn read_cases(root: &Path) -> std::result::Result<(Vec<String>, Vec<Case>), Vec<String>> {
    let dir = inside(root, CASES_DIR).map_err(|e| vec![format!("{e:#}")])?;
    let mut names = vec![];
    match fs::symlink_metadata(&dir) {
        Err(_) => {}
        Ok(meta) if !meta.is_dir() => {
            return Err(vec![format!(
                "{CASES_DIR} is not a directory; put each case in its own file, {CASES_DIR}/<name>.json."
            )]);
        }
        Ok(_) => {
            let entries = fs::read_dir(&dir)
                .map_err(|e| vec![format!("{CASES_DIR}/: cannot be read ({e}).")])?;
            for entry in entries {
                let entry =
                    entry.map_err(|e| vec![format!("{CASES_DIR}/: cannot be read ({e}).")])?;
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".json") {
                    names.push(format!("{CASES_DIR}/{name}"));
                }
            }
        }
    }
    names.sort();
    if !(MIN_CASES..=MAX_CASES).contains(&names.len()) {
        return Err(vec![format!(
            "{CASES_DIR}/ holds {} case files (*.json); a playbook is saved with at least {MIN_CASES} cases and at most {MAX_CASES}. Write a case for each situation the document distinguishes.",
            names.len()
        )]);
    }
    let mut cases = vec![];
    let mut faults = vec![];
    for name in &names {
        match read_case(root, name) {
            Ok(case) => cases.push(case),
            Err(e) => faults.push(format!("{e:#}")),
        }
    }
    if faults.is_empty() {
        Ok((names, cases))
    } else {
        Err(faults)
    }
}

fn bounded(what: &str, text: &str, max: usize) -> Result<()> {
    anyhow::ensure!(
        !text.trim().is_empty(),
        "{what} is empty; write it in the owner's words."
    );
    anyhow::ensure!(
        text.len() <= max,
        "{what} is {} bytes; keep it to {max}.",
        text.len()
    );
    Ok(())
}
/// The lines named hold whole sentences: the line before them is blank, or
/// ends a sentence, or they start a new block (a list item or heading); and
/// their last line ends a sentence, or the next line is blank or starts a
/// new block. So a quote never stops in the middle of a sentence.
fn whole(source: &str, line: u32, end: Option<u32>) -> Result<()> {
    fn ends(text: &str) -> bool {
        let text = text
            .trim_end()
            .trim_end_matches(['"', '\'', '\u{201d}', '\u{2019}', ')', ']', '*', '_']);
        text.ends_with(['.', '!', '?', ':'])
    }
    fn starts_block(text: &str) -> bool {
        let text = text.trim_start();
        let numbered = text.split_once(['.', ')']).is_some_and(|(n, rest)| {
            !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) && rest.starts_with(' ')
        });
        numbered
            || ["- ", "* ", "+ ", "#", ">", "|"]
                .iter()
                .any(|p| text.starts_with(p))
    }
    let lines: Vec<&str> = source.lines().collect();
    let first = line as usize - 1;
    let last = end.unwrap_or(line) as usize - 1;
    let shown = |a: usize, b: usize| {
        if a == b {
            format!("--line {}", a + 1)
        } else {
            format!("--lines {}-{}", a + 1, b + 1)
        }
    };
    // The sentence's whole extent, which the suggestion names, so that it is
    // itself accepted.
    let (mut start, mut finish) = (first, last);
    while start > 0
        && !lines[start - 1].trim().is_empty()
        && !ends(lines[start - 1])
        && !starts_block(lines[start])
    {
        start -= 1;
    }
    while finish + 1 < lines.len()
        && !lines[finish + 1].trim().is_empty()
        && !starts_block(lines[finish + 1])
        && !ends(lines[finish])
    {
        finish += 1;
    }
    if start < first {
        bail!(
            "line {}: the sentence starts on line {}, which it goes on from. Name the whole sentence, from its first line, such as {}.",
            first + 1,
            start + 1,
            shown(start, finish)
        );
    }
    if finish > last {
        bail!(
            "line {}: the sentence goes on in line {}. Name the whole sentence, to its last line, such as {}.",
            last + 1,
            last + 2,
            shown(start, finish)
        );
    }
    Ok(())
}
/// `A-B`: the first and last line of a sentence (`None` when they are one).
fn parse_range(range: &str) -> Result<(u32, Option<u32>)> {
    let refused = || {
        anyhow!(
            "--lines {range}: write the first and last line of the sentence as A-B, such as --lines 21-22, with A no greater than B; use --line N for one line."
        )
    };
    let (a, b) = range.split_once('-').ok_or_else(refused)?;
    let (a, b): (u32, u32) = (
        a.trim().parse().map_err(|_| refused())?,
        b.trim().parse().map_err(|_| refused())?,
    );
    anyhow::ensure!(a >= 1 && a <= b, refused());
    Ok((a, (b != a).then_some(b)))
}
fn shown(line: u32, end: Option<u32>) -> String {
    match end {
        Some(end) => format!("lines {line}-{end}"),
        None => format!("line {line}"),
    }
}

/// The catalog `commands` and `events` describe: in a workspace, its
/// `catalog.json` (or `--registry`, under the workspace's path rule); else
/// `./catalog.json` when present. `None` leaves the default to the caller.
pub fn workspace_registry(registry: Option<&Path>) -> Result<Option<Registry>> {
    let cwd = std::env::current_dir().context("the current directory cannot be read")?;
    if !is_workspace(&cwd) && cwd.ancestors().skip(1).any(is_workspace) {
        // In a subdirectory of a workspace: the same refusal as the tools.
        workspace_root()?;
    }
    let file = match registry {
        Some(path) if is_workspace(&cwd) => path
            .to_str()
            .ok_or_else(|| anyhow!("--registry: the path is not UTF-8"))?
            .to_string(),
        Some(_) => return Ok(None),
        None if is_workspace(&cwd) || exists(&cwd, CATALOG_FILE) => CATALOG_FILE.to_string(),
        None => return Ok(None),
    };
    let text = read_inside(&cwd, &file, MAX_CATALOG_BYTES)?;
    Ok(Some(parse_json(&file, &text)?))
}
