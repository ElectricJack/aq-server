use anyhow::{Context, Result, bail};
use aq_playbooks_cli::{catalog, pilots, simulation};
use aq_playbooks_compiler::{CompileInput, compile, semantic_diff, validate};
use aq_playbooks_model::{Artifact, Registry, canonical_bytes};
use aq_playbooks_postgres::PostgresStorage;
use aq_playbooks_runner::{
    Authority, Engine, InspectionKind, InspectionQuery, RetentionPolicy, Storage,
};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Parser)]
#[command(
    name = "aq-playbooks",
    version,
    about = "Compile, simulate, and inspect bounded policy workflows"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Validate and package an exact source/proposal/catalog bundle.
    Compile {
        input: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
    },
    Validate {
        input: PathBuf,
    },
    Diff {
        before: PathBuf,
        after: PathBuf,
    },
    Artifact {
        #[command(subcommand)]
        action: ArtifactAction,
    },
    Commands {
        #[arg(long)]
        registry: Option<PathBuf>,
        #[command(subcommand)]
        action: CatalogAction,
    },
    Events {
        #[arg(long)]
        registry: Option<PathBuf>,
        #[command(subcommand)]
        action: CatalogAction,
    },
    Interfaces {
        artifact: PathBuf,
        #[command(subcommand)]
        action: CatalogAction,
    },
    /// Report the finite synthetic AQ pilot API surface.
    Surface,
    /// Generate runnable fake-host pilot artifacts and scenarios.
    Examples {
        #[arg(long)]
        output: PathBuf,
    },
    /// Uses an in-memory store, scripted fake responses, and a virtual clock.
    Simulate {
        scenario: Option<PathBuf>,
        #[arg(long, conflicts_with = "scenario")]
        pilot: Option<String>,
    },
    Install {
        artifact: PathBuf,
        #[command(flatten)]
        store: StoreArgs,
    },
    Activate {
        binding: String,
        artifact: String,
        #[arg(long)]
        authority: PathBuf,
        #[command(flatten)]
        store: StoreArgs,
    },
    /// Admit and evaluate pure transitions. Host effects remain durable intents.
    Run {
        binding: String,
        #[arg(long, default_value = "main")]
        entry: String,
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        authority: PathBuf,
        #[command(flatten)]
        store: StoreArgs,
    },
    /// Diagnostic aggregate scan; protected stores require their configured keys.
    Metrics {
        #[command(flatten)]
        store: StoreArgs,
    },
    Inspect {
        run: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
        #[arg(long, value_enum, default_value = "runs")]
        kind: InspectKind,
        #[arg(long)]
        after: Option<String>,
        #[command(flatten)]
        store: StoreArgs,
    },
    Cancel {
        run: String,
        #[command(flatten)]
        store: StoreArgs,
    },
    /// Set independent controls for new calls, event starts and execution authority.
    Controls {
        binding: String,
        #[arg(long)]
        calls_enabled: Option<bool>,
        #[arg(long)]
        event_starts_enabled: Option<bool>,
        #[arg(long)]
        execution_enabled: Option<bool>,
        #[command(flatten)]
        store: StoreArgs,
    },
    /// Replace the revoked capability set for one actor/scope. Empty explicitly restores it.
    Authority {
        actor: String,
        scope: String,
        #[arg(long)]
        revoke: Vec<String>,
        #[command(flatten)]
        store: StoreArgs,
    },
    /// Collect one bounded page of safely expired state using the installed retention policy.
    Gc {
        #[arg(long, default_value_t = 100)]
        limit: usize,
        #[command(flatten)]
        store: StoreArgs,
    },
    /// Set explicit retention horizons and replay tombstone quota from a JSON policy.
    Retention {
        policy: PathBuf,
        #[command(flatten)]
        store: StoreArgs,
    },
    /// Fence a retired authenticated producer epoch before compacting replay tombstones.
    RetireProducer {
        producer: String,
        epoch: u64,
        #[arg(long)]
        actor: String,
        #[arg(long)]
        reason: String,
        #[command(flatten)]
        store: StoreArgs,
    },
    /// Re-encrypt the namespace with the active key from --protection-keys.
    RotateKeys {
        #[command(flatten)]
        store: StoreArgs,
    },
    /// Explain pending effects or record an evidence-backed host recovery request.
    Reconcile {
        run: Option<String>,
        #[arg(long)]
        request: Option<PathBuf>,
        #[command(flatten)]
        store: StoreArgs,
    },
}
#[derive(Subcommand)]
enum ArtifactAction {
    Inspect { path: PathBuf },
}
#[derive(Subcommand)]
enum CatalogAction {
    List,
    Describe { name: String },
}
#[derive(Clone, Copy, ValueEnum)]
enum InspectKind {
    Runs,
    Effects,
    Receipts,
}
impl From<InspectKind> for InspectionKind {
    fn from(value: InspectKind) -> Self {
        match value {
            InspectKind::Runs => Self::Runs,
            InspectKind::Effects => Self::Effects,
            InspectKind::Receipts => Self::Receipts,
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryInput {
    intent: String,
    actor: String,
    reason: String,
    evidence_refs: Vec<String>,
}
#[derive(Args)]
struct StoreArgs {
    #[arg(long, env = "AQ_PLAYBOOKS_DATABASE_URL", hide_env_values = true)]
    database_url: String,
    #[arg(long)]
    namespace: String,
    /// Owner-only JSON keyring file; keys are never stored in the database.
    #[arg(long)]
    protection_keys: Option<PathBuf>,
}
fn read<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    anyhow::ensure!(bytes.len() <= 16_777_216, "input file exceeds 16 MiB");
    serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
}
fn artifact(path: &Path) -> Result<Artifact> {
    let a: Artifact = read(path)?;
    a.verify()?;
    Ok(a)
}
fn print(value: &impl Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}
fn time() -> Result<i64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis()
        .try_into()?)
}
fn connect_store(args: &StoreArgs) -> Result<PostgresStorage> {
    match &args.protection_keys {
        Some(path) => Ok(PostgresStorage::connect_protected(
            &args.database_url,
            aq_playbooks_cli::configuration::read_keyring(path)?,
        )?),
        None => Ok(PostgresStorage::connect(&args.database_url)?),
    }
}
fn engine(args: &StoreArgs) -> Result<Engine<PostgresStorage>> {
    Ok(Engine::new(
        connect_store(args)?,
        &args.namespace,
        format!(
            "cli-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ),
    )?)
}
fn choose<T: Serialize>(
    map: &std::collections::BTreeMap<String, T>,
    action: CatalogAction,
) -> Result<()> {
    match action {
        CatalogAction::List => print(&map.keys().collect::<Vec<_>>()),
        CatalogAction::Describe { name } => print(
            map.get(&name)
                .with_context(|| format!("unknown registration: {name}"))?,
        ),
    }
}
fn catalog_file(path: Option<PathBuf>) -> Result<Registry> {
    path.map(|p| read(&p))
        .unwrap_or_else(|| Ok(catalog::catalog()))
}
fn main() {
    if let Err(error) = execute(Cli::parse()) {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}
fn execute(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Compile { input, output } => {
            let input: CompileInput = read(&input)?;
            match compile(&input) {
                Ok(a) => {
                    fs::write(output, canonical_bytes(&a)?)?;
                    print(
                        &json!({"artifact":a.digest,"executable_fingerprint":a.executable_fingerprint}),
                    )
                }
                Err(d) => {
                    print(&d)?;
                    bail!("compilation refused")
                }
            }
        }
        Command::Validate { input } => {
            let d = validate(&read(&input)?);
            print(&d)?;
            if !d.is_empty() {
                bail!("proposal is not valid")
            };
            Ok(())
        }
        Command::Diff { before, after } => {
            print(&semantic_diff(&artifact(&before)?, &artifact(&after)?)?)
        }
        Command::Artifact {
            action: ArtifactAction::Inspect { path },
        } => print(&artifact(&path)?),
        Command::Commands { registry, action } => choose(&catalog_file(registry)?.commands, action),
        Command::Events { registry, action } => choose(&catalog_file(registry)?.events, action),
        Command::Interfaces {
            artifact: path,
            action,
        } => choose(&artifact(&path)?.content.playbook.entries, action),
        Command::Surface => print(&catalog::describe_surface()),
        Command::Examples { output } => {
            fs::create_dir_all(&output)?;
            for name in pilots::NAMES {
                let scenario = pilots::scenario(name)?;
                fs::write(
                    output.join(format!("{name}.scenario.json")),
                    serde_json::to_vec_pretty(&scenario)?,
                )?;
                for a in &scenario.artifacts {
                    fs::write(
                        output.join(format!("{}.artifact.json", a.content.playbook.id)),
                        canonical_bytes(a)?,
                    )?;
                    let input = pilots::compile_input(a);
                    fs::write(
                        output.join(format!("{}.proposal.json", a.content.playbook.id)),
                        serde_json::to_vec_pretty(&input)?,
                    )?;
                    fs::write(
                        output.join(format!("{}.md", a.content.playbook.id)),
                        &input.source,
                    )?;
                }
            }
            print(&json!({"directory":output,"pilots":pilots::NAMES}))
        }
        Command::Simulate { scenario, pilot } => {
            let s = if let Some(name) = pilot {
                pilots::scenario(&name)?
            } else if let Some(path) = scenario {
                read(&path)?
            } else {
                bail!("provide a scenario file or --pilot")
            };
            print(&simulation::simulate(&s)?)
        }
        Command::Install {
            artifact: path,
            store,
        } => {
            let a = artifact(&path)?;
            let digest = a.digest.clone();
            let e = engine(&store)?;
            e.install(a)?;
            e.release()?;
            print(&json!({"installed":digest}))
        }
        Command::Activate {
            binding,
            artifact,
            authority,
            store,
        } => {
            let e = engine(&store)?;
            e.activate_at(&binding, &artifact, read(&authority)?, time()?)?;
            e.release()?;
            print(&json!({"binding":binding,"artifact":artifact}))
        }
        Command::Run {
            binding,
            entry,
            input,
            authority,
            store,
        } => {
            let e = engine(&store)?;
            let id = e.admit(
                &binding,
                &entry,
                read::<Value>(&input)?,
                read::<Authority>(&authority)?,
                time()?,
            )?;
            for _ in 0..1000 {
                if !e.advance(&id, time()?)? {
                    break;
                }
            }
            let view = simulation::inspect(&e.snapshot()?, Some(&id));
            e.release()?;
            print(&view)
        }
        Command::Metrics { store } => {
            let db = connect_store(&store)?;
            let state = db.load(&store.namespace)?.state;
            print(&aq_playbooks_runner::metrics::StateMetrics::from_state(
                &state,
                time()?,
            )?)
        }
        Command::Inspect {
            run,
            limit,
            kind,
            after,
            store,
        } => {
            let db = connect_store(&store)?;
            print(&db.inspect(
                &store.namespace,
                &InspectionQuery {
                    kind: kind.into(),
                    run,
                    after,
                    limit,
                },
            )?)
        }
        Command::Controls {
            binding,
            calls_enabled,
            event_starts_enabled,
            execution_enabled,
            store,
        } => {
            anyhow::ensure!(
                calls_enabled.is_some()
                    || event_starts_enabled.is_some()
                    || execution_enabled.is_some(),
                "specify at least one control"
            );
            let e = engine(&store)?;
            let state = e.snapshot()?;
            let mut controls = state
                .bindings
                .get(&binding)
                .context("unknown callable binding")?
                .controls
                .clone();
            if let Some(value) = calls_enabled {
                controls.calls_enabled = value;
            }
            if let Some(value) = event_starts_enabled {
                controls.event_starts_enabled = value;
            }
            if let Some(value) = execution_enabled {
                controls.execution_enabled = value;
            }
            e.set_activation_controls(&binding, controls.clone(), time()?)?;
            e.release()?;
            print(&json!({"binding":binding,"controls":controls}))
        }
        Command::Authority {
            actor,
            scope,
            revoke,
            store,
        } => {
            let e = engine(&store)?;
            e.set_execution_authority(&actor, &scope, revoke.into_iter().collect(), time()?)?;
            e.release()?;
            print(&json!({"authority_updated":true,"actor":actor,"scope":scope}))
        }
        Command::Gc { limit, store } => {
            let e = engine(&store)?;
            let report = e.collect_garbage(time()?, limit)?;
            e.release()?;
            print(&report)
        }
        Command::Retention { policy, store } => {
            let policy: RetentionPolicy = read(&policy)?;
            let e = engine(&store)?;
            e.set_retention_policy(policy)?;
            e.release()?;
            print(&json!({"retention_updated":true}))
        }
        Command::RetireProducer {
            producer,
            epoch,
            actor,
            reason,
            store,
        } => {
            let e = engine(&store)?;
            e.advance_producer_epoch(&producer, epoch, &actor, &reason, time()?)?;
            e.release()?;
            print(&json!({"producer":producer,"epoch":epoch}))
        }
        Command::RotateKeys { store } => {
            anyhow::ensure!(
                store.protection_keys.is_some(),
                "rotation requires --protection-keys"
            );
            let e = engine(&store)?;
            let revision = e.store.reencrypt_namespace(&e.namespace, &e.owner)?;
            e.release()?;
            print(&json!({"reencrypted":true,"revision":revision}))
        }
        Command::Cancel { run, store } => {
            let e = engine(&store)?;
            e.cancel(&run, time()?)?;
            for _ in 0..1000 {
                if !e.advance(&run, time()?)? {
                    break;
                }
            }
            let view = simulation::inspect(&e.snapshot()?, Some(&run));
            e.release()?;
            print(&view)
        }
        Command::Reconcile {
            run,
            request,
            store,
        } => {
            if let Some(path) = request {
                let request: RecoveryInput = read(&path)?;
                let e = engine(&store)?;
                if let Some(run) = run {
                    let state = e.snapshot()?;
                    anyhow::ensure!(
                        state
                            .intents
                            .get(&request.intent)
                            .is_some_and(|i| i.run == run),
                        "recovery intent does not belong to selected run"
                    );
                }
                let id = e.request_recovery(
                    &request.intent,
                    &request.actor,
                    &request.reason,
                    request.evidence_refs,
                    time()?,
                )?;
                e.release()?;
                print(
                    &json!({"recovery_request":id,"status":"recorded","execution":"registered host performs authoritative reconciliation"}),
                )
            } else {
                let db = connect_store(&store)?;
                print(&db.inspect(
                    &store.namespace,
                    &InspectionQuery {
                        kind: InspectionKind::Effects,
                        run,
                        after: None,
                        limit: 100,
                    },
                )?)
            }
        }
    }
}
