use anyhow::{Context, Result};
use aq_playbooks_cli::{catalog, simulation};
use aq_playbooks_model::{Artifact, Registry};
use aq_playbooks_postgres::PostgresStorage;
use aq_playbooks_runner::{
    Authority, CatalogKind, CatalogQuery, Engine, InspectionKind, InspectionQuery, RetentionPolicy,
    Storage,
};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
mod authoring;

#[derive(Parser)]
#[command(
    name = "aq-playbooks",
    version,
    about = "Author, check, inspect and administer bounded policy workflows"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    #[command(flatten)]
    Authoring(authoring::Authoring),
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
    /// Write the worked authoring workspaces (each a directory to run the
    /// authoring tools in) under DIR.
    Examples {
        #[arg(long)]
        output: PathBuf,
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
    /// Inspect current metadata while explicitly owning an existing idle namespace.
    /// Fails rather than stealing another process's lease. Never admits/drives work.
    InspectCatalog {
        #[arg(long, value_enum, default_value = "definitions")]
        kind: CatalogInspectKind,
        #[arg(long)]
        exact: Option<String>,
        #[arg(long, default_value_t = 25)]
        limit: usize,
        #[arg(long, default_value_t = 1)]
        pages: usize,
        #[arg(long, required = true)]
        acquire_idle_namespace: bool,
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
    /// Not needed: every commit already deletes finished work and old
    /// entries, a bounded amount each. Kept for existing scripts: runs one
    /// more such commit (plus a page of unreferenced artifact disposal) and
    /// prints what it did.
    Gc {
        #[arg(long, default_value_t = 100)]
        limit: usize,
        #[command(flatten)]
        store: StoreArgs,
    },
    /// Set explicit retention windows from a JSON policy (the deprecated
    /// `max_tombstones` and `terminal_run_ms` are accepted and ignored).
    Retention {
        policy: PathBuf,
        #[command(flatten)]
        store: StoreArgs,
    },
    /// Fence a retired authenticated producer epoch: its events and requests are refused from then on.
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
    /// Lower a retention clock that a far-future host time pinned (every
    /// request with a request time is refused as stale while it is ahead).
    /// Caps expiries and run timers written from the pin, records a stale
    /// floor and an admin receipt. Run with --dry-run first; committing
    /// needs --confirm-clock-rebase.
    RebaseRetentionClock {
        /// The corrected real time (ms): from now to a minute after it.
        #[arg(long)]
        to_ms: i64,
        /// The last correct host time before the pin (ms, in the past): a
        /// retry of a key the engine no longer remembers with a request time
        /// at or before it is refused as stale.
        #[arg(long)]
        stale_floor_ms: i64,
        #[arg(long)]
        actor: String,
        #[arg(long)]
        reason: String,
        /// Print what the rebase would change and commit nothing.
        #[arg(long, conflicts_with = "confirm_clock_rebase")]
        dry_run: bool,
        #[arg(long, required_unless_present = "dry_run")]
        confirm_clock_rebase: bool,
        #[command(flatten)]
        store: StoreArgs,
    },
    /// Lower a stale floor an earlier rebase set too high, accepting that
    /// keys forgotten after the new floor may be re-admitted as new by a
    /// retry with a request time in between. Needs --confirm-floor-lowering.
    LowerStaleFloor {
        #[arg(long)]
        to_ms: i64,
        #[arg(long)]
        actor: String,
        #[arg(long)]
        reason: String,
        #[arg(long, required = true)]
        confirm_floor_lowering: bool,
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
#[derive(Clone, Copy, ValueEnum)]
enum CatalogInspectKind {
    Definitions,
    Activations,
}
impl From<CatalogInspectKind> for CatalogKind {
    fn from(kind: CatalogInspectKind) -> Self {
        match kind {
            CatalogInspectKind::Definitions => Self::Definitions,
            CatalogInspectKind::Activations => Self::Activations,
        }
    }
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
/// Advance `run` until it makes no progress. A run whose cohort settled has
/// left the State (it is in history), so `NotFound` also ends the loop.
fn advance_until_settled(e: &Engine<PostgresStorage>, run: &str) -> Result<()> {
    for _ in 0..1000 {
        match e.advance(run, time()?) {
            Ok(true) => {}
            Ok(false) | Err(aq_playbooks_runner::Error::NotFound(_)) => break,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}
/// Lists or describes a contract. A contract that is the pinned catalog's
/// own is described with the host's notes on it (`"notes"`), the same notes
/// `CATALOG.md` renders.
fn choose<T: Serialize>(
    kind: &str,
    map: &std::collections::BTreeMap<String, T>,
    action: CatalogAction,
) -> Result<()> {
    match action {
        CatalogAction::List => print(&map.keys().collect::<Vec<_>>()),
        CatalogAction::Describe { name } => {
            let contract = map
                .get(&name)
                .with_context(|| format!("unknown registration: {name}"))?;
            let mut described = serde_json::to_value(contract)?;
            let pinned = aq_playbooks_compiler::docs::catalog_registry();
            let same = match kind {
                "commands" => pinned.commands.get(&name).map(serde_json::to_value),
                _ => pinned.events.get(&name).map(serde_json::to_value),
            };
            if let (Some(Ok(pinned)), Some(notes)) =
                (same, aq_playbooks_compiler::docs::notes(kind, &name))
                && pinned == described
                && let Some(object) = described.as_object_mut()
            {
                object.insert("notes".into(), notes);
            }
            print(&described)
        }
    }
}
fn catalog_file(path: Option<PathBuf>) -> Result<Registry> {
    if let Some(registry) = authoring::workspace_registry(path.as_deref())? {
        return Ok(registry);
    }
    path.map(|p| read(&p))
        .unwrap_or_else(|| Ok(catalog::catalog()))
}
fn main() {
    // Argument errors are JSON on stdout, exit 2, for an agent reading stdout.
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) if !error.use_stderr() => error.exit(),
        Err(error) => {
            println!("{}", json!({"error": error.render().to_string()}));
            std::process::exit(2);
        }
    };
    // Discovery is agent-facing too: its refusals are JSON on stdout.
    let discovery = matches!(
        cli.command,
        Command::Commands { .. } | Command::Events { .. }
    );
    if let Err(error) = execute(cli) {
        if discovery {
            println!("{}", json!({"error": format!("{error:#}")}));
        } else {
            eprintln!("{error:#}");
        }
        std::process::exit(1);
    }
}
fn execute(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Authoring(command) => authoring::run(command),
        Command::Artifact {
            action: ArtifactAction::Inspect { path },
        } => print(&artifact(&path)?),
        Command::Commands { registry, action } => {
            choose("commands", &catalog_file(registry)?.commands, action)
        }
        Command::Events { registry, action } => {
            choose("events", &catalog_file(registry)?.events, action)
        }
        Command::Examples { output } => {
            let files = aq_playbooks_compiler::docs::example_files();
            for (path, text) in &files {
                let target = output.join(path);
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&target, text)?;
            }
            let examples: std::collections::BTreeSet<&str> = files
                .iter()
                .filter_map(|(p, _)| p.split('/').next())
                .collect();
            print(&json!({"directory":output,"examples":examples}))
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
            advance_until_settled(&e, &id)?;
            let view = simulation::inspect(&e, Some(&id))?;
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
        Command::InspectCatalog {
            kind,
            exact,
            limit,
            pages,
            acquire_idle_namespace,
            store,
        } => {
            anyhow::ensure!(
                acquire_idle_namespace && (1..=4).contains(&pages),
                "explicit idle ownership and 1..4 pages required"
            );
            let mut query = CatalogQuery::first(kind.into(), limit);
            query.exact = exact;
            query.validate()?;
            let db = connect_store(&store)?;
            anyhow::ensure!(
                db.incarnation(&store.namespace)?.is_some(),
                "catalog namespace does not exist"
            );
            let e = Engine::new(
                db,
                &store.namespace,
                format!(
                    "catalog-cli-{}-{}",
                    std::process::id(),
                    SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
                ),
            )?;
            let observed = (|| -> Result<Vec<_>> {
                let mut output = Vec::with_capacity(pages);
                for _ in 0..pages {
                    let page = e.inspect_catalog(&query)?;
                    query.after = page.next_cursor.clone();
                    query.expected = Some(page.stamp.clone());
                    output.push(page);
                    if query.after.is_none() {
                        break;
                    }
                }
                Ok(output)
            })();
            let released = e.release();
            let observed = observed?;
            released?;
            if pages == 1 {
                print(&observed[0])
            } else {
                print(&observed)
            }
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
        Command::RebaseRetentionClock {
            to_ms,
            stale_floor_ms,
            actor,
            reason,
            dry_run,
            confirm_clock_rebase,
            store,
        } => {
            anyhow::ensure!(
                dry_run || confirm_clock_rebase,
                "--confirm-clock-rebase or --dry-run required"
            );
            let e = engine(&store)?;
            let report = if dry_run {
                e.rebase_retention_clock_dry_run(to_ms, stale_floor_ms, &actor, &reason, time()?)
            } else {
                e.rebase_retention_clock(to_ms, stale_floor_ms, &actor, &reason, time()?)
            };
            e.release()?;
            let report = report?;
            print(&json!({"dry_run":dry_run,"report":report}))
        }
        Command::LowerStaleFloor {
            to_ms,
            actor,
            reason,
            confirm_floor_lowering,
            store,
        } => {
            anyhow::ensure!(confirm_floor_lowering, "--confirm-floor-lowering required");
            let e = engine(&store)?;
            let lowered = e.lower_stale_floor(to_ms, &actor, &reason, time()?);
            e.release()?;
            let (from_ms, to_ms) = lowered?;
            print(&json!({"from_ms":from_ms,"to_ms":to_ms}))
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
            advance_until_settled(&e, &run)?;
            let view = simulation::inspect(&e, Some(&run))?;
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
                    if !state.intents.contains_key(&request.intent) {
                        // An operation that left with its finished run is
                        // in history: it is settled, nothing to recover.
                        let finished = e
                            .inspect(&InspectionQuery {
                                kind: InspectionKind::Effects,
                                run: Some(run.clone()),
                                after: None,
                                limit: 1000,
                            })?
                            .items
                            .iter()
                            .any(|item| item["id"] == request.intent.as_str());
                        anyhow::ensure!(
                            !finished,
                            "recovery intent has finished with its run; nothing to recover"
                        );
                    }
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
