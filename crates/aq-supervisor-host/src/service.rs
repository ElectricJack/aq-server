//! Connect the existing reservation and physical host owners without fabricating
//! worker identities. One isolated controller serves one durable conversation.
mod plan;
mod reconcile;
mod reservation;
use crate::{
    Supervisor,
    adapter::NativeAdapter,
    config::{Config, InstalledAuthority, protected_read},
};
use anyhow::{Context, Result, ensure};
use aq_central_authority::{Phase, owner::Snapshot};
use aq_server::{
    agent_access::supervisor_conversation::{
        dispatch,
        reservations::{
            Cancel, Configure, LocalPolicy, PolicyKind, PreviewRequest, PreviewStatus,
            ReservationOwner, Reserve,
        },
    },
    store::{Role, User},
};
use aq_session_host::{LocalEnvironment, ResourcePolicy};
use reconcile::Installed;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{Connection, PgPool, Row};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use uuid::Uuid;
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    pub database_url: String,
    pub conversation_id: Uuid,
    pub authority_id: Uuid,
    pub root: PathBuf,
    pub workspace: PathBuf,
    pub homes: PathBuf,
    pub helper: PathBuf,
    pub tmux: PathBuf,
    pub tmux_version: String,
    pub native: PathBuf,
    pub native_digest: aq_session_protocol::Digest,
    pub image_digest: String,
    pub secret_file: PathBuf,
    pub server_origin: String,
}
#[derive(Clone)]
struct Control {
    actor: Uuid,
    enabled: bool,
    revision: u64,
    profile: Value,
    owner: Option<Uuid>,
    project: Option<Uuid>,
    label: String,
}
type Host = Supervisor<InstalledAuthority, NativeAdapter>;
impl ServiceConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let c: Self = serde_json::from_slice(&protected_read(path, 65536)?)?;
        ensure!(
            !c.conversation_id.is_nil() && !c.authority_id.is_nil(),
            "supervisor service identity"
        );
        crate::tool::origin(&c.server_origin)?;
        ensure!(
            [
                &c.root,
                &c.workspace,
                &c.homes,
                &c.helper,
                &c.tmux,
                &c.native,
                &c.secret_file
            ]
            .iter()
            .all(|p| p.is_absolute()),
            "absolute supervisor service paths"
        );
        ensure!(
            c.image_digest.starts_with("sha256:") && c.image_digest.len() == 71,
            "pinned supervisor environment"
        );
        Ok(c)
    }
    fn turn_file(&self) -> PathBuf {
        self.root.join("current-turn.json")
    }
}
async fn control(pool: &PgPool, id: Uuid) -> Result<Control> {
    let r=sqlx::query("SELECT c.*,i.project_id,COALESCE(p.label,'Global supervisor')AS label FROM supervisor_controls c JOIN supervisor_conversation_ids i USING(conversation_id) LEFT JOIN project_supervisor_conversations p USING(conversation_id) WHERE c.conversation_id=$1").bind(id).fetch_one(pool).await?;
    Ok(Control {
        actor: r.get("actor_id"),
        enabled: r.get("enabled"),
        revision: r.get::<i64, _>("revision") as u64,
        profile: r.get::<Option<Value>, _>("profile").unwrap_or(Value::Null),
        owner: r.get("approved_by"),
        project: r.get("project_id"),
        label: r.get("label"),
    })
}
async fn user(pool: &PgPool, control: &Control) -> Result<User> {
    let id = control.owner.context("supervisor has not been approved")?;
    let r = sqlx::query("SELECT email,issuer,subject FROM users WHERE id=$1 AND role='owner'")
        .bind(id)
        .fetch_one(pool)
        .await?;
    Ok(User {
        id,
        email: r.get("email"),
        role: Role::Owner,
        issuer: r.get("issuer"),
        subject: r.get("subject"),
    })
}
impl Service<'_> {
    async fn report(&self, phase: &str, stage: &str, state: Option<&Snapshot>) -> Result<()> {
        let generation = state.map(|s| s.run.plan().generation as i64);
        let session =
            state.and_then(|s| aq_supervisor_control::authority::session_id(s.run.plan()));
        sqlx::query("INSERT INTO supervisor_service_reports(conversation_id,instance_id,observed_at_ms,phase,stage,generation,session_id,last_error) VALUES($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT(conversation_id) DO UPDATE SET instance_id=EXCLUDED.instance_id,observed_at_ms=EXCLUDED.observed_at_ms,phase=EXCLUDED.phase,stage=EXCLUDED.stage,generation=EXCLUDED.generation,session_id=EXCLUDED.session_id,last_error=EXCLUDED.last_error").bind(self.config.conversation_id).bind(self.instance).bind(now()as i64).bind(phase).bind(stage).bind(generation).bind(session).bind(if matches!(phase,"failed"|"uncertain"){Some(stage)}else{None}).execute(self.pool).await?;
        Ok(())
    }
}
// Neither a pool acquisition timeout nor PostgreSQL refusing a new connection
// establishes a failed turn. Re-read the retained reservation and turn on the next poll.
fn connection_unavailable(error: &anyhow::Error) -> bool {
    match error.downcast_ref::<sqlx::Error>() {
        Some(sqlx::Error::PoolTimedOut) => true,
        Some(sqlx::Error::Database(db)) => db.code().as_deref() == Some("53300"),
        _ => false,
    }
}

#[cfg(test)]
#[test]
fn pool_exhaustion_can_repoll_but_an_unconfirmed_turn_stays_failed() {
    let timed_out =
        anyhow::Error::from(sqlx::Error::PoolTimedOut).context("read supervisor control");
    assert!(connection_unavailable(&timed_out));
    assert!(!connection_unavailable(&anyhow::anyhow!(
        "turn ended without a reply"
    )));
    assert!(!connection_unavailable(&anyhow::Error::from(
        sqlx::Error::RowNotFound
    )));
}

fn api_error(_: aq_server::error::AppError) -> anyhow::Error {
    anyhow::anyhow!("Supervisor reservation or dispatch was refused")
}
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn installed(config: &ServiceConfig, service_pool: &PgPool) -> Result<Option<(Config, Host)>> {
    let path = config.turn_file();
    if !path.exists() {
        return Ok(None);
    }
    let (c, digest) = Config::load(&path)?;
    ensure!(
        c.central.subject.conversation_id() == config.conversation_id && c.root == config.root,
        "foreign installed supervisor turn"
    );
    let local = LocalEnvironment {
        resource_policy: ResourcePolicy::CallerAdmission(c.resource_admission.clone()),
        workspace: c.workspace.clone(),
        helper_executable: c.helper.clone(),
        tmux_executable: c.tmux.clone(),
        tmux_version: c.tmux_version.clone(),
        credential_profiles: vec![],
        context_material: vec![],
    };
    // The installed plan normally uses this service's database. Reuse its
    // bounded pool on each control poll, including while a turn is active.
    let pool = if c.database_url == config.database_url {
        service_pool.clone()
    } else {
        PgPool::connect_lazy(&c.database_url)?
    };
    let host = Supervisor::new(
        pool,
        crate::Runtime {
            root: c.root.clone(),
            local,
        },
        Arc::new(InstalledAuthority {
            path: path.clone(),
            digest: digest.clone(),
        }),
        Arc::new(NativeAdapter {
            config: path,
            digest,
            wrapper: std::env::current_exe()?,
        }),
    );
    Ok(Some((c, host)))
}
fn clean_homes(config: &ServiceConfig) -> Result<()> {
    for entry in std::fs::read_dir(&config.homes)? {
        let path = entry?.path();
        let m = std::fs::symlink_metadata(&path)?;
        if !m.is_dir() || m.file_type().is_symlink() {
            continue;
        }
        for name in ["auth.json", "supervisor.credential"] {
            let _ = std::fs::remove_file(path.join(name));
        }
    }
    Ok(())
}
fn archive_turn(config: &ServiceConfig, turn: &Config) -> Result<()> {
    let destination = config.root.join(format!(
        "turn-{}-{}.json",
        turn.central.generation, turn.central.dispatch_id
    ));
    std::fs::rename(config.turn_file(), destination)?;
    Ok(())
}
pub async fn run(path: &Path) -> Result<()> {
    let config = ServiceConfig::load(path)?;
    prepare_runtime(&config)?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .connect(&config.database_url)
        .await?;
    let mut lease = sqlx::PgConnection::connect(&config.database_url).await?;
    let held: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock(hashtextextended($1,109))")
        .bind(format!("central-service:{}", config.conversation_id))
        .fetch_one(&mut lease)
        .await?;
    ensure!(held, "supervisor controller already running");
    let service = Service {
        config: &config,
        pool: &pool,
        owner: ReservationOwner::with_pool(pool.clone()),
        instance: Uuid::new_v4(),
    };
    service.poll().await?;
    service.shutdown().await
}

fn prepare_runtime(config: &ServiceConfig) -> Result<()> {
    let mounts = std::fs::read_to_string("/proc/mounts")?;
    ensure!(
        Path::new("/.dockerenv").is_file()
            && mounts.lines().any(|l| {
                let f = l.split_whitespace().collect::<Vec<_>>();
                f.get(1) == Some(&"/")
                    && f.get(3)
                        .is_some_and(|flags| flags.split(',').any(|v| v == "ro"))
            }),
        "central subscription requires a read-only container root"
    );

    ensure!(
        unsafe { libc::geteuid() } == 0,
        "central service requires UID separation"
    );
    std::fs::create_dir_all(&config.root)?;
    std::fs::create_dir_all(&config.workspace)?;
    std::fs::create_dir_all(&config.homes)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&config.root, std::fs::Permissions::from_mode(0o700))?;
    std::fs::set_permissions(&config.homes, std::fs::Permissions::from_mode(0o711))?;
    std::fs::set_permissions(&config.workspace, std::fs::Permissions::from_mode(0o700))?;
    std::os::unix::fs::chown(&config.workspace, Some(10001), Some(10001))?;
    Ok(())
}

struct Service<'a> {
    config: &'a ServiceConfig,
    pool: &'a PgPool,
    owner: ReservationOwner,
    instance: Uuid,
}

#[derive(Default)]
struct Poll {
    scan_after: u64,
    failed_revision: Option<u64>,
}

impl Service<'_> {
    async fn poll(&self) -> Result<()> {
        let mut cursor = Poll::default();
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let mut poll_delay = Duration::ZERO;
        loop {
            tokio::select! {_=terminate.recv()=>break,_=tokio::signal::ctrl_c()=>break,_=tokio::time::sleep(poll_delay)=>{}}
            poll_delay = Duration::from_millis(750);
            let ctl = match control(self.pool, self.config.conversation_id).await {
                Ok(ctl) => ctl,
                Err(error) if connection_unavailable(&error) => continue,
                Err(error) => return Err(error),
            };
            let outcome = self.step(&ctl, &mut cursor).await;
            if let Err(error) = outcome {
                if connection_unavailable(&error) {
                    let _ = self
                        .report("waiting", "Waiting for a database connection", None)
                        .await;
                    continue;
                }
                self.write_error(&error);
                let _ = self
                    .report(
                        "failed",
                        "Supervisor needs inspection; work and history are retained",
                        None,
                    )
                    .await;
                cursor.failed_revision = Some(ctl.revision);
            }
        }
        Ok(())
    }

    fn write_error(&self, error: &anyhow::Error) {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(self.config.root.join("service-error.txt"))
        {
            let _ = write!(file, "{error:#}");
        }
    }

    async fn shutdown(&self) -> Result<()> {
        if let Some((c, host)) = installed(self.config, self.pool)? {
            let s = host.stop(c.central.subject.actor_id()).await?;
            if s.run.phase() == Phase::StoppedReported {
                clean_homes(self.config)?;
            } else {
                self.report(
                    "uncertain",
                    "Stop is unconfirmed; inspect the retained supervisor",
                    Some(&s),
                )
                .await?;
                return Ok(());
            }
        }
        self.report("stopped", "Supervisor service stopped", None)
            .await?;
        Ok(())
    }
}
impl Service<'_> {
    async fn step(&self, ctl: &Control, cursor: &mut Poll) -> Result<()> {
        if let Some((turn, host)) = installed(self.config, self.pool)?
            && !self
                .reconcile(ctl, cursor, Installed { turn, host })
                .await?
        {
            return Ok(());
        }
        if !ctl.enabled {
            cursor.failed_revision = None;
            self.report("stopped", "Supervisor is stopped", None)
                .await?;
            return Ok(());
        }
        if cursor.failed_revision == Some(ctl.revision) {
            self.report(
                "failed",
                "Inspect the prior turn, then stop and start to continue",
                None,
            )
            .await?;
            return Ok(());
        }
        self.scan(ctl, cursor).await
    }
}
/// Automatically retry a mail batch once if its previous guardian never
/// started the harness. A repeated launch failure stays visible for inspection.
async fn retry_unstarted(pool: &PgPool, previous: &Snapshot) -> Result<bool> {
    let mut tx = pool.begin().await?;
    if crate::verifier::not_started(&mut tx, &previous.run)
        .await?
        .is_none()
    {
        return Ok(false);
    }
    let first: bool = sqlx::query_scalar(
        "SELECT NOT EXISTS(SELECT 1 FROM central_supervisor_dispatches p
         WHERE p.actor_id=d.actor_id AND p.generation<d.generation AND p.batch_digest=d.batch_digest)
         FROM central_supervisor_dispatches d WHERE d.dispatch_id=$1",
    )
    .bind(previous.run.plan().dispatch_id)
    .fetch_one(&mut *tx)
    .await?;
    Ok(first)
}

/// The current reservation's own turn ran (its generation is the latest) and
/// stopped without a reply: `Some(true)` releases its mail for the next turn
/// because the owner stopped and started the supervisor after the dispatch,
/// `Some(false)` waits for that. `None` is any other outstanding reservation.
struct DispatchTime {
    created: Option<i64>,
    control_updated: i64,
}
impl DispatchTime {
    fn stopped_turn(&self, closed: bool, generation: u64, next: u64) -> Option<bool> {
        let dispatched = self.created?;
        (closed && generation + 1 == next).then_some(self.control_updated > dispatched)
    }
}

fn swarm_stage(reason: &str) -> &'static str {
    if reason == aq_swarm_settings::admission::CEILING {
        "Waiting for this provider and intelligence class concurrency limit"
    } else {
        "Waiting for an authoritative intelligence class and configured capacity"
    }
}

#[cfg(test)]
mod tests {
    use super::DispatchTime;

    #[test]
    fn a_stopped_turn_is_released_only_after_an_owner_restart() {
        // Generation 5 ran for this reservation and stopped; the next would be 6.
        assert_eq!(
            DispatchTime {
                created: Some(1_000),
                control_updated: 900
            }
            .stopped_turn(true, 5, 6),
            Some(false)
        );
        assert_eq!(
            DispatchTime {
                created: Some(1_000),
                control_updated: 1_000
            }
            .stopped_turn(true, 5, 6),
            Some(false)
        );
        assert_eq!(
            DispatchTime {
                created: Some(1_000),
                control_updated: 1_001
            }
            .stopped_turn(true, 5, 6),
            Some(true)
        );
        // Still running, an older generation, or no dispatch: not this case.
        assert_eq!(
            DispatchTime {
                created: Some(1_000),
                control_updated: 2_000
            }
            .stopped_turn(false, 5, 6),
            None
        );
        assert_eq!(
            DispatchTime {
                created: Some(1_000),
                control_updated: 2_000
            }
            .stopped_turn(true, 4, 6),
            None
        );
        assert_eq!(
            DispatchTime {
                created: None,
                control_updated: 2_000
            }
            .stopped_turn(true, 6, 6),
            None
        );
    }
}
