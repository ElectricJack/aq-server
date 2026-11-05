use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, bail};
use aq_server::store::Store;
use clap::{Parser, Subcommand};
use sqlx::postgres::PgConnectOptions;

#[derive(Parser)]
#[command(name = "aq-server", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the admin and worker listeners.
    Serve {
        /// Vault directory containing config.toml.
        #[arg(long, env = "AQ_VAULT_DIR")]
        vault: PathBuf,
    },
    /// Probe the local admin listener's /healthz; exit 0 if healthy, 1 if not.
    Healthcheck {
        /// Vault directory; the probe targets its admin_bind port (default 8080).
        #[arg(long, env = "AQ_VAULT_DIR")]
        vault: Option<PathBuf>,
    },
    /// GET the loopback /readyz and print its JSON; exit 0 on 200, 1 otherwise.
    Readiness {
        /// Vault directory; the probe targets its admin_bind port (default 8080).
        #[arg(long, env = "AQ_VAULT_DIR")]
        vault: Option<PathBuf>,
    },
    /// GET loopback /dependency-readyz; print JSON and exit nonzero if degraded.
    DependencyReadiness {
        #[arg(long, env = "AQ_VAULT_DIR")]
        vault: Option<PathBuf>,
    },
    /// Onboard a GitHub repository (D2): central aq-git binding, retained
    /// default-branch base, profile pre-check and project binding. The request
    /// JSON is the `POST /api/repositories/onboard` body; re-running the same
    /// `operation_id` resumes or replays it. Prints the onboarding JSON.
    OnboardRepository {
        #[arg(long, env = "AQ_VAULT_DIR")]
        vault: PathBuf,
        /// Email of the owner the operation is recorded against.
        #[arg(long)]
        owner_email: String,
        /// Request JSON file, or `-` for stdin.
        #[arg(long)]
        request: PathBuf,
    },
    /// Import or replace a subscription login (C5b): stores the file as the
    /// newest version of a configured subscription-login credential. An
    /// operator upload always wins over node write-backs. Prints the version.
    ImportLogin {
        #[arg(long, env = "AQ_VAULT_DIR")]
        vault: PathBuf,
        /// Credential profile as `id@version`.
        #[arg(long)]
        credential: String,
        /// Registry credential revision.
        #[arg(long)]
        revision: u64,
        /// Login file: Codex `auth.json` or a Claude long-lived token file.
        #[arg(long)]
        source: PathBuf,
    },
    /// Print this binary's canonical release manifest (the bytes that get signed).
    Manifest {
        #[arg(long)]
        server_image: String,
        #[arg(long)]
        worker_image: String,
        /// Must equal this binary's version.
        #[arg(long)]
        version: String,
        #[arg(long)]
        source_revision: String,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    // stderr only: stdout carries machine-read output (manifest, readiness).
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let result = match cli.command {
        Command::Serve { vault } => serve(vault).await,
        Command::Healthcheck { vault } => std::process::exit(healthcheck(vault).await),
        Command::Readiness { vault } => std::process::exit(readiness(vault).await),
        Command::DependencyReadiness { vault } => {
            std::process::exit(readiness_at(vault, "/dependency-readyz").await)
        }
        Command::OnboardRepository {
            vault,
            owner_email,
            request,
        } => onboard_repository(vault, &owner_email, &request).await,
        Command::ImportLogin {
            vault,
            credential,
            revision,
            source,
        } => import_login(vault, &credential, revision, &source).await,
        Command::Manifest {
            server_image,
            worker_image,
            version,
            source_revision,
        } => manifest(&server_image, &worker_image, &version, &source_revision),
    };
    if let Err(e) = result {
        eprintln!("aq-server: {e:#}");
        std::process::exit(2);
    }
}

async fn serve(vault: PathBuf) -> anyhow::Result<()> {
    let (config, store) = connect(&vault).await?;
    aq_server::run(config, store).await
}

async fn onboard_repository(
    vault: PathBuf,
    owner_email: &str,
    request: &std::path::Path,
) -> anyhow::Result<()> {
    use std::io::Read;
    let mut body = Vec::new();
    if request == std::path::Path::new("-") {
        std::io::stdin()
            .take(128 * 1024 + 1)
            .read_to_end(&mut body)?;
    } else {
        std::fs::File::open(request)
            .context("cannot open the request file")?
            .take(128 * 1024 + 1)
            .read_to_end(&mut body)?;
    }
    if body.len() > 128 * 1024 {
        bail!("onboarding request exceeds 128 KiB");
    }
    let request: aq_server::repository_onboarding::OnboardRequest =
        serde_json::from_slice(&body).context("invalid onboarding request")?;
    let (config, store) = connect(&vault).await?;
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
    let state = aq_server::build_state(config, store, http).await?;
    let Some(owner) = aq_server::repository_onboarding::owner_by_email(&state, owner_email)
        .await
        .map_err(|e| anyhow::anyhow!("cannot look up the owner: {e:?}"))?
    else {
        bail!("no owner with that email");
    };
    let out = aq_server::repository_onboarding::onboard(&state, owner, request)
        .await
        .map_err(|e| anyhow::anyhow!("onboarding refused: {e:?}"))?;
    println!("{}", serde_json::to_string_pretty(&out)?);
    if out["disposition"] == "bound" {
        Ok(())
    } else {
        std::process::exit(1)
    }
}

async fn import_login(
    vault: PathBuf,
    credential: &str,
    revision: u64,
    source: &std::path::Path,
) -> anyhow::Result<()> {
    let (config, store) = connect(&vault).await?;
    let Some(loaded) = config.intelligence.clone() else {
        bail!("[intelligence] is not configured");
    };
    let version = aq_server::intelligence::login_import::import(
        &store,
        &loaded,
        credential,
        revision,
        source,
        chrono::Utc::now().timestamp_millis(),
    )
    .await?;
    println!("{version}");
    Ok(())
}

async fn connect(vault: &std::path::Path) -> anyhow::Result<(aq_server::config::Config, Store)> {
    let config = aq_server::config::load(vault)?;
    // Checked before touching the database so a misconfigured production
    // deployment fails fast.
    aq_server::config::check_deployment(&config, std::env::var("AQ_DEPLOYMENT").ok().as_deref())?;
    let Some(url) = std::env::var("DATABASE_URL").ok().filter(|u| !u.is_empty()) else {
        bail!("DATABASE_URL is not set");
    };
    // Parse errors can echo the URL, so they are reported without detail.
    let opts = PgConnectOptions::from_str(&url)
        .map_err(|_| anyhow::anyhow!("DATABASE_URL is not a valid PostgreSQL URL"))?;
    tracing::info!(
        host = opts.get_host(),
        database = opts.get_database().unwrap_or(""),
        "connecting to database"
    );
    // sqlx connection errors do not include the password.
    let store = Store::connect_with(opts)
        .await
        .context("cannot connect to the database")?;
    Ok((config, store))
}

fn manifest(
    server_image: &str,
    worker_image: &str,
    version: &str,
    source_revision: &str,
) -> anyhow::Result<()> {
    use std::io::Write;
    let m =
        aq_server::updates::build_manifest(server_image, worker_image, version, source_revision)?;
    // No trailing newline: these are exactly the signed bytes.
    let mut out = std::io::stdout().lock();
    out.write_all(&m.to_canonical_json())?;
    out.flush()?;
    Ok(())
}

/// Admin port from the vault, or 8080 when it cannot be read.
fn admin_port(vault: Option<PathBuf>) -> u16 {
    vault
        .and_then(|v| aq_server::config::load(&v).ok())
        .map(|c| c.server.admin_bind.port())
        .unwrap_or(8080)
}

async fn get_local(port: u16, path: &str) -> reqwest::Result<reqwest::Response> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()?
        .get(format!("http://127.0.0.1:{port}{path}"))
        .send()
        .await
}

/// Container health probe. The image has no curl, so the binary checks itself.
async fn healthcheck(vault: Option<PathBuf>) -> i32 {
    let port = admin_port(vault);
    match get_local(port, "/healthz")
        .await
        .and_then(|r| r.error_for_status())
    {
        Ok(_) => 0,
        Err(e) => {
            eprintln!("aq-server: unhealthy: {e}");
            1
        }
    }
}

/// Prints the loopback readiness report; used by the host updater via `docker exec`.
async fn readiness(vault: Option<PathBuf>) -> i32 {
    readiness_at(vault, "/readyz").await
}

async fn readiness_at(vault: Option<PathBuf>, path: &str) -> i32 {
    let port = admin_port(vault);
    let resp = match get_local(port, path).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("aq-server: not ready: {e}");
            return 1;
        }
    };
    let ok = resp.status() == reqwest::StatusCode::OK;
    match resp.text().await {
        Ok(body) => println!("{body}"),
        Err(e) => {
            eprintln!("aq-server: not ready: {e}");
            return 1;
        }
    }
    if ok { 0 } else { 1 }
}
