//! Connect the existing reservation and physical host owners without fabricating
//! worker identities. One isolated controller serves one durable conversation.
mod plan;
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
            Configure, LocalPolicy, PolicyKind, PreviewRequest, PreviewStatus, ReservationOwner,
            Reserve,
        },
    },
    store::{Role, User},
};
use aq_session_host::{LocalEnvironment, ResourcePolicy};
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
async fn report(
    pool: &PgPool,
    config: &ServiceConfig,
    instance: Uuid,
    phase: &str,
    stage: &str,
    state: Option<&Snapshot>,
) -> Result<()> {
    let generation = state.map(|s| s.run.plan().generation as i64);
    let session = state.and_then(|s| aq_supervisor_control::authority::session_id(s.run.plan()));
    sqlx::query("INSERT INTO supervisor_service_reports(conversation_id,instance_id,observed_at_ms,phase,stage,generation,session_id,last_error) VALUES($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT(conversation_id) DO UPDATE SET instance_id=EXCLUDED.instance_id,observed_at_ms=EXCLUDED.observed_at_ms,phase=EXCLUDED.phase,stage=EXCLUDED.stage,generation=EXCLUDED.generation,session_id=EXCLUDED.session_id,last_error=EXCLUDED.last_error").bind(config.conversation_id).bind(instance).bind(now()as i64).bind(phase).bind(stage).bind(generation).bind(session).bind(if matches!(phase,"failed"|"uncertain"){Some(stage)}else{None}).execute(pool).await?;
    Ok(())
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
fn installed(config: &ServiceConfig) -> Result<Option<(Config, Host)>> {
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
    let pool = PgPool::connect_lazy(&c.database_url)?;
    let host = Supervisor::new(
        pool,
        c.root.clone(),
        local,
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
    let owner = ReservationOwner::with_pool(pool.clone());
    let instance = Uuid::new_v4();
    let mut scan_after = 0;
    let mut failed_revision = None;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        let ctl = control(&pool, config.conversation_id).await?;
        let outcome = step(
            &config,
            &pool,
            &owner,
            instance,
            &ctl,
            &mut scan_after,
            &mut failed_revision,
        )
        .await;
        if let Err(error) = outcome {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(config.root.join("service-error.txt"))
            {
                let _ = write!(file, "{error:#}");
            }
            let _ = report(
                &pool,
                &config,
                instance,
                "failed",
                "Supervisor needs inspection; work and history are retained",
                None,
            )
            .await;
            failed_revision = Some(ctl.revision);
        }
        tokio::select! {_=terminate.recv()=>break,_=tokio::signal::ctrl_c()=>break,_=tokio::time::sleep(Duration::from_millis(750))=>{}}
    }
    if let Some((c, host)) = installed(&config)? {
        let s = host.stop(c.central.subject.actor_id()).await?;
        if s.run.phase() == Phase::StoppedReported {
            clean_homes(&config)?;
        } else {
            report(
                &pool,
                &config,
                instance,
                "uncertain",
                "Stop is unconfirmed; inspect the retained supervisor",
                Some(&s),
            )
            .await?;
            return Ok(());
        }
    }
    report(
        &pool,
        &config,
        instance,
        "stopped",
        "Supervisor service stopped",
        None,
    )
    .await?;
    Ok(())
}
async fn step(
    config: &ServiceConfig,
    pool: &PgPool,
    owner: &ReservationOwner,
    instance: Uuid,
    ctl: &Control,
    scan_after: &mut u64,
    failed_revision: &mut Option<u64>,
) -> Result<()> {
    if let Some((turn, host)) = installed(config)? {
        ensure!(
            turn.central.subject.actor_id() == ctl.actor,
            "supervisor actor changed"
        );
        let current = aq_supervisor_control::authority::sessions(pool)
            .inspect(ctl.actor)
            .await?;
        let successor = current.as_ref().is_some_and(|s| {
            s.run.plan().generation < turn.central.generation
                && matches!(
                    s.run.phase(),
                    Phase::StoppedReported | Phase::CancelledBeforeStart
                )
        });
        let state = if current.is_none() || successor {
            if !ctl.enabled {
                return Ok(());
            }
            if let Some(reason) = crate::config::swarm_wait(
                pool,
                turn.central.profiles.catalog.digest.as_str(),
                turn.swarm_profile.as_ref(),
            )
            .await?
            {
                report(pool, config, instance, "waiting", swarm_stage(reason), None).await?;
                return Ok(());
            }
            host.start(
                turn.central.clone(),
                turn.host.clone(),
                protected_read(&turn.startup_file, 1048576)?,
                turn.central.max_permit_ms,
            )
            .await?
        } else {
            let existing = current.context("central state missing")?;
            ensure!(
                existing.run.plan().generation == turn.central.generation,
                "current turn does not match retained generation"
            );
            if !ctl.enabled && existing.run.phase() != Phase::StoppedReported {
                host.stop(ctl.actor).await?
            } else {
                host.reconcile(ctl.actor).await?
            }
        };
        match state.run.phase() {
            Phase::StartCommitted | Phase::RunningReported => {
                *failed_revision = None;
                host.enable_replies(ctl.actor, now()).await?;
                let p = state
                    .run
                    .permit()
                    .context("active supervisor permit missing")?;
                let boot = crate::clock()?.boot_ms;
                if boot >= p.issued_boot_ms + (p.expires_boot_ms - p.issued_boot_ms) / 2
                    && p.expires_boot_ms < state.run.original_deadline_boot_ms()
                {
                    host.renew(ctl.actor, p.sequence, turn.central.max_permit_ms)
                        .await?;
                }
                report(
                    pool,
                    config,
                    instance,
                    "working",
                    "Working on the conversation",
                    Some(&state),
                )
                .await?;
                return Ok(());
            }
            Phase::StopRequested => {
                report(
                    pool,
                    config,
                    instance,
                    "stopping",
                    "Stopping supervisor; retaining work",
                    Some(&state),
                )
                .await?;
                return Ok(());
            }
            Phase::StopUncertain => {
                report(
                    pool,
                    config,
                    instance,
                    "uncertain",
                    "Stop is unconfirmed; inspect the retained supervisor",
                    Some(&state),
                )
                .await?;
                return Ok(());
            }
            Phase::StoppedReported | Phase::CancelledBeforeStart => {
                clean_homes(config)?;
                let replied: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM central_supervisor_replies WHERE dispatch_id=$1)",
                )
                .bind(turn.central.dispatch_id)
                .fetch_one(pool)
                .await?;
                archive_turn(config, &turn)?;
                if ctl.enabled && !replied {
                    *failed_revision = Some(ctl.revision);
                    report(pool,config,instance,"failed","Turn ended without a confirmed reply; inspect task changes before continuing",Some(&state)).await?;
                    return Ok(());
                }
            }
            Phase::Prepared => {
                report(
                    pool,
                    config,
                    instance,
                    "uncertain",
                    "Prepared supervisor needs reconciliation",
                    Some(&state),
                )
                .await?;
                return Ok(());
            }
        }
    }
    if !ctl.enabled {
        *failed_revision = None;
        report(
            pool,
            config,
            instance,
            "stopped",
            "Supervisor is stopped",
            None,
        )
        .await?;
        return Ok(());
    }
    if *failed_revision == Some(ctl.revision) {
        report(
            pool,
            config,
            instance,
            "failed",
            "Inspect the prior turn, then stop and start to continue",
            None,
        )
        .await?;
        return Ok(());
    }
    let user = user(pool, ctl).await?;
    let mut actor = owner.inspect_actor(&user, ctl.actor).await.ok();
    let approved: Option<Uuid> =
        sqlx::query_scalar("SELECT approved_by FROM central_supervisor_actors WHERE actor_id=$1")
            .bind(ctl.actor)
            .fetch_optional(pool)
            .await?;
    if actor.as_ref().is_none_or(|a| !a.policy.enabled) || approved != Some(user.id) {
        owner
            .configure_actor(
                &user,
                &serde_json::to_vec(&Configure {
                    operation_id: Uuid::new_v4(),
                    actor_id: ctl.actor,
                    expected_actor_revision: actor.as_ref().map_or(0, |a| a.revision),
                    policy: LocalPolicy {
                        schema_version: 1,
                        kind: PolicyKind::GlobalSupervisorReservation,
                        actor_id: ctl.actor,
                        conversation_id: config.conversation_id,
                        enabled: true,
                    },
                })?,
            )
            .await
            .map_err(api_error)?;
        actor = Some(
            owner
                .inspect_actor(&user, ctl.actor)
                .await
                .map_err(api_error)?,
        );
    }
    let actor = actor.context("supervisor policy is unavailable")?;
    let preview = owner
        .preview_batch(
            &user,
            PreviewRequest {
                actor_id: ctl.actor,
                expected_actor_revision: actor.revision,
                scan_after: *scan_after,
                through_cursor: None,
            },
        )
        .await
        .map_err(api_error)?;
    match preview.status {
        PreviewStatus::NoEligibleMail => {
            *scan_after = preview.scanned_through_cursor;
            report(pool, config, instance, "idle", "Ready for messages", None).await?;
        }
        PreviewStatus::PartialScan => {
            *scan_after = preview.scanned_through_cursor;
            report(
                pool,
                config,
                instance,
                "waiting",
                "Checking queued messages",
                None,
            )
            .await?;
        }
        PreviewStatus::Ready => {
            let reservation = owner
                .reserve_batch(
                    &user,
                    &serde_json::to_vec(&Reserve {
                        operation_id: Uuid::new_v4(),
                        reservation_id: Uuid::new_v4(),
                        actor_id: ctl.actor,
                        expected_actor_revision: preview.actor_revision,
                        expected_policy_revision: preview.policy_revision,
                        scan_after: preview.scan_after,
                        through_cursor: preview.through_cursor,
                        selection_digest: preview.selection_digest,
                    })?,
                )
                .await
                .map_err(api_error)?
                .reservation
                .context("supervisor reservation was refused")?;
            let generation = aq_supervisor_control::authority::sessions(pool)
                .inspect(ctl.actor)
                .await?
                .map_or(1, |s| s.run.plan().generation + 1);
            let session = Uuid::new_v4();
            start_turn(
                config,
                pool,
                instance,
                ctl,
                &actor.policy_digest,
                reservation.id,
                generation,
                session,
            )
            .await?;
        }
        PreviewStatus::ReservationOutstanding => {
            // A dispatch recorded before plan installation is recoverable only
            // if its generation never entered central authority. Preserve its
            // immutable input and operation/session IDs; never retarget an
            // admitted, handed-off or uncertain generation.
            let prior = aq_supervisor_control::authority::sessions(pool)
                .inspect(ctl.actor)
                .await?;
            let closed = prior.as_ref().is_none_or(|s| {
                matches!(
                    s.run.phase(),
                    Phase::StoppedReported | Phase::CancelledBeforeStart
                )
            });
            let next = prior.as_ref().map_or(1, |s| s.run.plan().generation + 1);
            let row=sqlx::query("SELECT a.current_reservation,d.generation,d.session_id FROM central_supervisor_actors a LEFT JOIN central_supervisor_dispatches d ON d.actor_id=a.actor_id AND d.reservation_id=a.current_reservation WHERE a.actor_id=$1").bind(ctl.actor).fetch_one(pool).await?;
            let generation = row
                .get::<Option<i64>, _>("generation")
                .map_or(next, |g| g as u64);
            if closed && generation == next {
                let reservation: Uuid = row
                    .get::<Option<Uuid>, _>("current_reservation")
                    .context("retained reservation missing")?;
                let session = row
                    .get::<Option<Uuid>, _>("session_id")
                    .unwrap_or_else(Uuid::new_v4);
                start_turn(
                    config,
                    pool,
                    instance,
                    ctl,
                    &actor.policy_digest,
                    reservation,
                    generation,
                    session,
                )
                .await?;
            } else {
                report(
                    pool,
                    config,
                    instance,
                    "uncertain",
                    "An earlier reservation needs inspection",
                    None,
                )
                .await?;
            }
        }
        PreviewStatus::Disabled => {
            report(
                pool,
                config,
                instance,
                "stopped",
                "Supervisor policy is disabled",
                None,
            )
            .await?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn start_turn(
    config: &ServiceConfig,
    pool: &PgPool,
    instance: Uuid,
    ctl: &Control,
    policy: &aq_central_authority::Digest,
    reservation: Uuid,
    generation: u64,
    session: Uuid,
) -> Result<()> {
    let digest = ctl.profile["catalog_digest"]
        .as_str()
        .context("supervisor catalog unavailable")?;
    if let Some(reason) = crate::config::swarm_wait(pool, digest, Some(&ctl.profile)).await? {
        report(pool, config, instance, "waiting", swarm_stage(reason), None).await?;
        return Ok(());
    }
    let turn = dispatch::prepare_turn(pool, ctl.actor, reservation, generation, session, now())
        .await
        .map_err(api_error)?;
    let candidate = plan::assemble(config, ctl, policy, generation, session, turn)?;
    plan::install(config, &candidate)?;
    let (_, host) = installed(config)?.context("installed supervisor turn missing")?;
    let state = host
        .start(
            candidate.central.clone(),
            candidate.host.clone(),
            protected_read(&candidate.startup_file, 1048576)?,
            candidate.central.max_permit_ms,
        )
        .await?;
    host.enable_replies(ctl.actor, now()).await?;
    report(
        pool,
        config,
        instance,
        "working",
        "Working on the conversation",
        Some(&state),
    )
    .await
}

fn swarm_stage(reason: &str) -> &'static str {
    if reason == aq_swarm_settings::admission::CEILING {
        "Waiting for this provider and intelligence class concurrency limit"
    } else {
        "Waiting for an authoritative intelligence class and configured capacity"
    }
}
