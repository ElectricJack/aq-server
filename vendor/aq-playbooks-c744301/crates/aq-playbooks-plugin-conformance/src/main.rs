//! Standalone acceptance driver for operator-installed exec plugins.
use anyhow::{Context, Result, ensure};
use aq_playbooks_model::{EventEnvelope, canonical_bytes, digest};
use clap::Parser;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs,
    io::{self, Read as _},
    path::{Path, PathBuf},
    process::ExitCode,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

mod plugin;
use plugin::{Failure, FailureKind, Plugin, Response};

#[derive(Parser)]
#[command(about = "Check an F5 plugin package and its exec responses without a server")]
struct Args {
    /// Installed plugin directory containing plugin.toml.
    #[arg(long)]
    plugin_dir: PathBuf,
    /// Disposable vault containing any declared binding and fake secret files.
    #[arg(long)]
    vault: PathBuf,
    /// Disposable writable root for the plugin's persistent state.
    #[arg(long)]
    state_root: PathBuf,
    /// Project UUID used in invocations and binding lookup.
    #[arg(long)]
    project: Uuid,
    /// Optional JSON object that overrides this project's vault binding.
    #[arg(long)]
    binding: Option<PathBuf>,
    /// JSON file containing test cases, or '-' to read stdin.
    #[arg(long)]
    fixtures: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixtures {
    cases: Vec<Case>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    id: String,
    command: String,
    input: Value,
    #[serde(default)]
    expect: Expect,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Expect {
    outcome: Option<String>,
    event_names: Option<Vec<String>>,
    failure: Option<FailureKind>,
    /// Simulate runner re-dispatch after this expected first transport failure.
    retry_after: Option<FailureKind>,
}

#[derive(Serialize)]
struct CaseResult<'a> {
    id: &'a str,
    command: &'a str,
    idempotency_key: String,
    result: Observation,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum Observation {
    Response {
        response: Response,
        envelopes: Vec<EventEnvelope>,
        exact_replay: bool,
        attempts: u8,
    },
    ExpectedFailure {
        failure: Failure,
    },
    Failed {
        reason: String,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args = Args::parse();
    let (passed, report) = match run(&args).await {
        Ok(result) => result,
        Err(error) => (
            false,
            json!({"status":"failed","error":format!("{error:#}"),"external_side_effects_verified":false}),
        ),
    };
    let stdout = io::stdout().lock();
    if serde_json::to_writer(stdout, &report).is_err() {
        return ExitCode::FAILURE;
    }
    println!();
    if passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

async fn run(args: &Args) -> Result<(bool, Value)> {
    let fixtures: Fixtures = if args.fixtures == Path::new("-") {
        let mut input = Vec::new();
        io::stdin().read_to_end(&mut input)?;
        serde_json::from_slice(&input).context("invalid fixture JSON")?
    } else {
        read_json(&args.fixtures, "fixtures")?
    };
    ensure!(!fixtures.cases.is_empty(), "fixture contains no cases");
    let plugin = Plugin::load(&args.plugin_dir, &args.state_root, &args.vault)?;
    let mut results = Vec::new();
    let mut ids = BTreeSet::new();
    let mut passed = true;
    for case in &fixtures.cases {
        ensure!(
            !case.id.is_empty() && ids.insert(case.id.as_str()),
            "case ids must be nonempty and distinct"
        );
        ensure!(
            case.expect.failure.is_none()
                || (case.expect.outcome.is_none()
                    && case.expect.event_names.is_none()
                    && case.expect.retry_after.is_none()),
            "case {:?}: a terminal failure cannot be combined with success expectations",
            case.id
        );
        let namespace = format!("conformance:{}", args.project);
        let intent_id = format!("{namespace}:{}", case.id);
        let key = digest(&(&namespace, &intent_id))?;
        let result = match check_case(args, &plugin, case, &intent_id, &key).await {
            Ok(result) => result,
            Err(error) => {
                passed = false;
                Observation::Failed {
                    reason: format!("{error:#}"),
                }
            }
        };
        results.push(CaseResult {
            id: &case.id,
            command: &case.command,
            idempotency_key: key,
            result,
        });
    }
    Ok((
        passed,
        json!({
            "status": if passed { "passed" } else { "failed" },
            "plugin": plugin.extension.name(),
            "project_id": args.project,
            "cases": results,
            "external_side_effects_verified": false,
        }),
    ))
}

async fn check_case(
    args: &Args,
    plugin: &Plugin,
    case: &Case,
    intent_id: &str,
    key: &str,
) -> Result<Observation> {
    let command = plugin
        .extension
        .registry()
        .commands
        .get(&case.command)
        .context("fixture names a command outside the plugin contract")?;
    command
        .input
        .validate(&case.input)
        .context("invalid fixture input")?;
    ensure!(
        canonical_bytes(&case.input)?.len() <= command.max_payload_bytes,
        "fixture input exceeds command payload limit"
    );
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis()
        .try_into()?;
    let deadline = Instant::now() + Duration::from_millis(command.deadline_ms);
    let invoke = |deadline: Option<Instant>| {
        let binding = plugin.binding(args.project, args.binding.as_deref());
        let remaining_ms = deadline.map_or(command.deadline_ms, |deadline| {
            deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX)
        });
        plugin.invoke(plugin::Call {
            command,
            input: &case.input,
            key,
            project: args.project,
            binding,
            intent_id,
            now_ms,
            remaining_ms,
        })
    };
    let first = invoke(Some(deadline)).await;
    if let Some(expected) = case.expect.failure {
        return match first {
            Err(failure) if failure.kind == expected => {
                Ok(Observation::ExpectedFailure { failure })
            }
            Err(failure) => Err(anyhow::anyhow!("expected {expected}, observed {failure}")),
            Ok(_) => Err(anyhow::anyhow!(
                "expected {expected}, observed a valid response"
            )),
        };
    }
    let (first, attempts) = if let Some(expected) = case.expect.retry_after {
        match first {
            Err(failure) if failure.kind == expected => (invoke(Some(deadline)).await?, 3),
            Err(failure) => anyhow::bail!("expected first attempt {expected}, observed {failure}"),
            Ok(_) => anyhow::bail!("expected first attempt {expected}, observed a valid response"),
        }
    } else {
        (first?, 2)
    };
    if let Some(outcome) = &case.expect.outcome {
        ensure!(
            &first.response.outcome == outcome,
            "unexpected response outcome"
        );
    }
    if let Some(names) = &case.expect.event_names {
        ensure!(
            first
                .response
                .events
                .iter()
                .map(|event| &event.name)
                .eq(names),
            "unexpected response event names or ordering"
        );
    }
    let replay = invoke(None).await.context("replay failed")?;
    ensure!(
        first.bytes == replay.bytes,
        "replay stdout differs byte-for-byte"
    );
    Ok(Observation::Response {
        response: first.response,
        envelopes: first.envelopes,
        exact_replay: true,
        attempts,
    })
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path, label: &str) -> Result<T> {
    let bytes = fs::read(path).with_context(|| format!("cannot read {label} file"))?;
    serde_json::from_slice(&bytes).with_context(|| format!("invalid {label} JSON"))
}
