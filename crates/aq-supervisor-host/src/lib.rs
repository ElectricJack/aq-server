//! Trusted-process supervisor lifecycle. No worker identity or network endpoint.
//! PlanAuthority must authenticate policy and original immutable inputs; pins alone
//! never authorize execution. The executable uses owner-only installed admission.
pub mod adapter;
mod bridge;
pub mod config;
mod conversation;
pub mod credentials;
mod journal;
pub mod service;
pub mod subscription;
pub mod tool;
mod verifier;
use anyhow::{Context, Result, bail, ensure};
use aq_central_authority::{
    owner::{Receipt, SessionOwner, Snapshot},
    *,
};
use aq_session_host::{
    GuardianIdentity, HostAdapter, HostObservation, HostPlan, LaunchIntent, LocalEnvironment,
    SessionHost,
};
pub use bridge::{clock, validate_join};
use sqlx::{Connection, PgConnection, PgPool, Postgres, Row, Transaction};
use std::{path::PathBuf, sync::Arc, time::Duration};
use uuid::Uuid;
use verifier::Verifier;

/// Implemented by the authenticated central controller or exact installed local
/// admission. Verify policy and original mail facts under the supplied transaction.
#[async_trait::async_trait]
pub trait PlanAuthority: Send + Sync + 'static {
    async fn authorize(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        central: &CentralPlan,
        host: &HostPlan,
    ) -> Result<()>;
}
pub struct Supervisor<A, H> {
    pool: PgPool,
    root: PathBuf,
    local: LocalEnvironment,
    authority: Arc<A>,
    adapter: Arc<H>,
}
struct Record {
    id: Uuid,
    host: HostPlan,
}
/// Reads a regular, non-symlink file of at most 1 MiB.
fn private_file(path: &std::path::Path) -> Result<Vec<u8>> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let m = file.metadata()?;
    ensure!(m.is_file() && m.len() <= 1_048_576, "staged input bounds");
    let mut bytes = Vec::new();
    file.take(1_048_577).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 == m.len(), "staged input changed");
    Ok(bytes)
}
fn accepted(receipt: Receipt) -> Result<Snapshot> {
    ensure!(
        !matches!(receipt.decision, Decision::Refused { .. }),
        "central lifecycle refused: {:?}",
        receipt.decision
    );
    Ok(receipt.snapshot)
}
/// Attempts for one controller step before the supervisor gives up.
pub const RETRY_ATTEMPTS: u32 = 3;
/// First backoff; doubles per attempt (200 ms, 400 ms) to stay well inside a permit.
pub const RETRY_BACKOFF: Duration = Duration::from_millis(200);

/// Database or connection failure that a later attempt may clear. Refusals,
/// evidence mismatches and uncertain stops are never retried.
pub fn is_transient(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<sqlx::Error>().is_some()
            || matches!(
                cause.downcast_ref::<owner::OwnerError>(),
                Some(owner::OwnerError::Storage)
            )
    })
}

/// Run `op` up to `attempts` times with doubling `backoff` while `retry` accepts
/// the error. A retry never sleeps to or past `deadline_boot_ms` (permit expiry),
/// so a guardian is never left running on an expired permit by the retry itself.
pub async fn retry_bounded<T, F, Fut>(
    attempts: u32,
    backoff: Duration,
    deadline_boot_ms: u64,
    retry: impl Fn(&anyhow::Error) -> bool,
    mut op: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut delay = backoff;
    let mut attempt = 1;
    loop {
        let error = match op().await {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        let wake = clock()?
            .boot_ms
            .saturating_add(u64::try_from(delay.as_millis()).unwrap_or(u64::MAX));
        if attempt >= attempts || !retry(&error) || wake >= deadline_boot_ms {
            return Err(error);
        }
        tokio::time::sleep(delay).await;
        delay = delay.saturating_mul(2);
        attempt += 1;
    }
}
fn active(s: &Snapshot) -> bool {
    matches!(
        s.run.phase(),
        Phase::StartCommitted | Phase::RunningReported
    )
}
impl<A: PlanAuthority, H: HostAdapter + Send + Sync + 'static> Supervisor<A, H> {
    pub fn new(
        pool: PgPool,
        root: PathBuf,
        local: LocalEnvironment,
        authority: Arc<A>,
        adapter: Arc<H>,
    ) -> Self {
        Self {
            pool,
            root,
            local,
            authority,
            adapter,
        }
    }
    fn owner(&self, candidate: Option<HostPlan>) -> SessionOwner<Verifier<A>> {
        SessionOwner::new(
            self.pool.clone(),
            Verifier {
                authority: self.authority.clone(),
                candidate,
            },
        )
    }
    async fn fence(&self, actor: Uuid) -> Result<PgConnection> {
        ensure!(!actor.is_nil(), "nil actor");
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::symlink_metadata(&self.root)?;
        ensure!(
            m.is_dir()
                && m.uid() == unsafe { libc::geteuid() }
                && m.mode() & 0o077 == 0
                && self.root.canonicalize()? == self.root,
            "host root must be canonical and private"
        );
        // Dedicated connection: a waiting physical operation must not exhaust the
        // pool needed by the holder's durable callbacks. Closing releases the lock.
        let mut conn = PgConnection::connect_with(&self.pool.connect_options()).await?;
        sqlx::query("SET lock_timeout='10s'")
            .execute(&mut conn)
            .await?;
        sqlx::query("SELECT pg_advisory_lock(hashtextextended($1, 671))")
            .bind(actor.to_string())
            .execute(&mut conn)
            .await?;
        Ok(conn)
    }
    async fn current(&self, actor: Uuid) -> Result<Snapshot> {
        Box::pin(self.owner(None).inspect(actor))
            .await?
            .context("central actor absent")
    }
    async fn record(&self, s: &Snapshot) -> Result<Record> {
        let row =
            sqlx::query("SELECT * FROM central_host_launches WHERE actor_id=$1 AND generation=$2")
                .bind(s.run.plan().subject.actor_id())
                .bind(s.run.plan().generation as i64)
                .fetch_one(&self.pool)
                .await?;
        let root: String = row.try_get("root")?;
        ensure!(
            std::path::Path::new(&root) == self.root,
            "host root changed; preserve original owner"
        );
        let body: String = row.try_get("host_plan")?;
        let host = HostPlan::decode(body.as_bytes())?;
        validate_join(s.run.plan(), &host)?;
        ensure!(
            s.run.original_deadline_boot_ms() == host.deadline_boot_ms,
            "central original deadline mismatch"
        );
        let id = row.try_get("launch_id")?;
        let reservation: String = row.try_get("reservation")?;
        ensure!(
            reservation == journal::reservation(s.run.plan(), &host, id, &self.root)?,
            "host reservation mismatch"
        );
        Ok(Record { id, host })
    }
    /// Start an authenticated exact plan once. Same-plan retries reconcile the
    /// existing physical attempt; a crash never turns into a second launch.
    pub async fn start(
        &self,
        central: CentralPlan,
        host: HostPlan,
        startup: Vec<u8>,
        ttl_ms: u64,
    ) -> Result<Snapshot> {
        validate_join(&central, &host)?;
        host.verify_startup(&startup)?;
        let actor = central.subject.actor_id();
        let _fence = self.fence(actor).await?;
        if let Some(old) = Box::pin(self.owner(None).inspect(actor)).await?
            && old.run.plan() == &central
        {
            let s = Box::pin(self.reconcile_locked(actor)).await?;
            ensure!(active(&s), "existing generation is not live");
            return Ok(s);
        }
        let now = clock()?;
        ensure!(
            now.machine_boot_id == host.machine_boot_id
                && now.boot_ms >= host.admitted_boot_ms
                && now.boot_ms < host.deadline_boot_ms,
            "host admission boot/deadline"
        );
        let owner = self.owner(Some(host.clone()));
        let prepared = Box::pin(owner.prepare(
            central.clone(),
            Clock {
                machine_boot_id: host.machine_boot_id,
                boot_ms: host.admitted_boot_ms,
            },
        ))
        .await?;
        ensure!(
            prepared.run.original_deadline_boot_ms() == host.deadline_boot_ms,
            "original deadline join"
        );
        let id = Uuid::new_v4();
        let reservation = journal::reservation(&central, &host, id, &self.root)?;
        sqlx::query("INSERT INTO central_host_launches(actor_id,generation,launch_id,root,host_plan,reservation) VALUES($1,$2,$3,$4,$5,$6)")
            .bind(actor).bind(central.generation as i64).bind(id).bind(self.root.to_str().context("host root UTF8")?).bind(journal::bytes(&host)?).bind(&reservation).execute(&self.pool).await?;
        let started = accepted(
            Box::pin(owner.start(
                &central.binding()?,
                journal::launch_ref(&central, id, &reservation)?,
                ttl_ms,
                clock()?,
            ))
            .await?,
        )?;
        let permit = bridge::permit(
            &central,
            &host,
            started.run.permit().context("missing central permit")?,
        )?;
        let root = self.root.clone();
        let local = self.local.clone();
        let adapter = self.adapter.clone();
        let mut callbacks = journal::Callbacks {
            authority: self.authority.clone(),
            pool: self.pool.clone(),
            runtime: tokio::runtime::Handle::current(),
            launch: id,
            plan: host.clone(),
        };
        // Any error retains the reservation/callbacks. Only recovery may touch it.
        let observation = tokio::task::spawn_blocking(move || -> Result<_> {
            let handle = SessionHost::launch_plan(
                &root,
                id,
                &host,
                &startup,
                permit,
                &local,
                adapter.as_ref(),
                &mut callbacks,
            )?;
            handle.observe()
        })
        .await
        .map_err(anyhow::Error::from)
        .and_then(|r| r);
        let result = match observation {
            Ok(observation) => Box::pin(self.running(&started, &observation)).await,
            Err(error) => Err(error),
        };
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                // Even a refused late Running report must trigger cleanup.
                // Recovery alone decides whether stopped proof is available.
                let _ = Box::pin(self.stop_locked(actor)).await;
                return Err(error);
            }
        };
        if !active(&result) {
            return Box::pin(self.stop_locked(actor)).await;
        }
        Ok(result)
    }
    async fn running(&self, s: &Snapshot, o: &HostObservation) -> Result<Snapshot> {
        let g = journal::append(
            &self.pool,
            o.guardian.intent.launch_id,
            "guardian",
            &o.guardian,
        )
        .await?;
        let pin = journal::append(&self.pool, o.guardian.intent.launch_id, "running", o).await?;
        accepted(
            Box::pin(self.owner(None).report(
                &s.run.plan().binding()?,
                Event::Running {
                    observation: HostObservationRef {
                        binding: s.run.plan().binding()?,
                        launch_id: o.guardian.intent.launch_id,
                        guardian: g,
                        observation: pin,
                    },
                },
                clock()?,
            ))
            .await?,
        )
    }
    pub async fn inspect(&self, actor: Uuid) -> Result<Snapshot> {
        self.current(actor).await
    }
    pub async fn observe(&self, actor: Uuid) -> Result<HostObservation> {
        let _fence = self.fence(actor).await?;
        let s = self.current(actor).await?;
        let r = self.record(&s).await?;
        bridge::permit(
            s.run.plan(),
            &r.host,
            s.run.permit().context("missing permit")?,
        )?;
        let g = journal::fact::<GuardianIdentity>(&self.pool, r.id, "guardian")
            .await?
            .context("guardian not recorded")?;
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || SessionHost::observe_existing(&root, &g)?.observe())
            .await?
    }
    pub async fn reconcile(&self, actor: Uuid) -> Result<Snapshot> {
        let _fence = self.fence(actor).await?;
        Box::pin(self.reconcile_locked(actor)).await
    }
    async fn reconcile_locked(&self, actor: Uuid) -> Result<Snapshot> {
        let old = self.current(actor).await?;
        if matches!(
            old.run.phase(),
            Phase::StoppedReported | Phase::CancelledBeforeStart
        ) {
            return Ok(old);
        }
        let tick = accepted(
            Box::pin(
                self.owner(None)
                    .report(&old.run.plan().binding()?, Event::Tick, clock()?),
            )
            .await?,
        )?;
        if active(&tick) {
            let record = self.record(&tick).await?;
            let mut tx = self.pool.begin().await?;
            let authorized = self
                .authority
                .authorize(&mut tx, tick.run.plan(), &record.host)
                .await
                .is_ok();
            tx.rollback().await?;
            if !authorized {
                return Box::pin(self.stop_locked(actor)).await;
            }
            if let (Ok(permit), Some(g)) = (
                bridge::permit(
                    tick.run.plan(),
                    &record.host,
                    tick.run.permit().context("missing permit")?,
                ),
                journal::fact::<GuardianIdentity>(&self.pool, record.id, "guardian").await?,
            ) {
                // One failed physical observation is not proof of loss: retry
                // inside the current permit before stopping the session.
                let expires = tick.run.permit().context("missing permit")?.expires_boot_ms;
                let observed = retry_bounded(
                    RETRY_ATTEMPTS,
                    RETRY_BACKOFF,
                    expires,
                    |_| true,
                    || {
                        let root = self.root.clone();
                        let (g, permit) = (g.clone(), permit.clone());
                        async move {
                            tokio::task::spawn_blocking(move || -> Result<_> {
                                let handle = SessionHost::observe_existing(&root, &g)?;
                                let observation = handle.observe()?;
                                ensure!(
                                    observation.permit_sequence <= permit.sequence,
                                    "physical permit ahead of central owner"
                                );
                                if observation.permit_sequence < permit.sequence {
                                    handle.renew(&permit)?;
                                }
                                Ok(observation)
                            })
                            .await?
                        }
                    },
                )
                .await;
                if let Ok(o) = observed {
                    if tick.run.phase() == Phase::StartCommitted {
                        return Box::pin(self.running(&tick, &o)).await;
                    }
                    return Ok(tick);
                }
            }
        }
        Box::pin(self.stop_locked(actor)).await
    }
    pub async fn renew(&self, actor: Uuid, prior_sequence: u64, ttl_ms: u64) -> Result<Snapshot> {
        let _fence = self.fence(actor).await?;
        let current = Box::pin(self.reconcile_locked(actor)).await?;
        ensure!(active(&current), "central generation inactive");
        let next = accepted(
            Box::pin(self.owner(None).renew(
                &current.run.plan().binding()?,
                prior_sequence,
                ttl_ms,
                clock()?,
            ))
            .await?,
        )?;
        let record = self.record(&next).await?;
        let permit = bridge::permit(
            next.run.plan(),
            &record.host,
            next.run.permit().context("missing permit")?,
        )?;
        let guardian = journal::fact::<GuardianIdentity>(&self.pool, record.id, "guardian")
            .await?
            .context("guardian missing")?;
        let root = self.root.clone();
        let result = tokio::task::spawn_blocking(move || {
            SessionHost::observe_existing(&root, &guardian)?.renew(&permit)
        })
        .await?;
        if result.is_err() {
            Box::pin(self.stop_locked(actor)).await?;
            bail!("physical renewal failed; stopped and preserved");
        }
        Ok(next)
    }
    /// F1b: prove the turn input reached the live session by re-reading the
    /// exact staged `context-0.bin`. Host Running alone is not delivery; any
    /// mismatch or unreadable file is reported delivery_unknown, never re-sent.
    pub async fn handoff(&self, actor: Uuid) -> Result<Snapshot> {
        let _fence = self.fence(actor).await?;
        let s = self.current(actor).await?;
        if matches!(s.run.dispatch(), DispatchState::Resolved { .. }) {
            return Ok(s);
        }
        ensure!(
            s.run.phase() == Phase::RunningReported,
            "handoff requires a running generation"
        );
        let association = s
            .run
            .association()
            .cloned()
            .context("no dispatch association")?;
        let reconcile = matches!(s.run.dispatch(), DispatchState::Unknown { .. });
        let r = self.record(&s).await?;
        let staged = private_file(&self.root.join(r.id.to_string()).join("context-0.bin"));
        let event = match staged {
            Ok(bytes) if r.host.verify_startup(&bytes).is_ok() => {
                let receipt = journal::append(
                    &self.pool,
                    r.id,
                    "handoff",
                    &verifier::HandoffFact {
                        launch_id: r.id,
                        association: association.clone(),
                        disposition: HandoffDisposition::HandedOff,
                        input: Digest::of(&bytes),
                    },
                )
                .await?;
                let evidence = HandoffEvidence {
                    association,
                    disposition: HandoffDisposition::HandedOff,
                    receipt,
                };
                if reconcile {
                    Event::ReconcileHandoff { evidence }
                } else {
                    Event::Handoff { evidence }
                }
            }
            _ => Event::ReportUnknown { association },
        };
        accepted(
            Box::pin(
                self.owner(None)
                    .report(&s.run.plan().binding()?, event, clock()?),
            )
            .await?,
        )
    }
    /// F1b: stage the generation-bound supervisor credential beside the live
    /// session only (0600, create-new). It is never written to a worker scope.
    pub async fn install_credential(&self, actor: Uuid, token: &str) -> Result<PathBuf> {
        ensure!(
            token.len() == 70
                && token.starts_with("aqsv1_")
                && token[6..]
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "malformed supervisor credential"
        );
        let _fence = self.fence(actor).await?;
        let s = self.current(actor).await?;
        ensure!(active(&s), "central generation inactive");
        self.install_credential_locked(&s, token).await
    }
    async fn install_credential_locked(&self, s: &Snapshot, token: &str) -> Result<PathBuf> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let r = self.record(s).await?;
        let path = self
            .root
            .join(r.id.to_string())
            .join("supervisor.credential");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?;
        file.write_all(token.as_bytes())?;
        file.sync_all()?;
        Ok(path)
    }
    pub async fn stop(&self, actor: Uuid) -> Result<Snapshot> {
        let _fence = self.fence(actor).await?;
        Box::pin(self.stop_locked(actor)).await
    }
    async fn stop_locked(&self, actor: Uuid) -> Result<Snapshot> {
        let s = self.current(actor).await?;
        if matches!(
            s.run.phase(),
            Phase::StoppedReported | Phase::CancelledBeforeStart
        ) {
            return Ok(s);
        }
        let s = accepted(
            Box::pin(self.owner(None).report(
                &s.run.plan().binding()?,
                Event::Cancel {
                    reason: CancelReason::Operator,
                },
                clock()?,
            ))
            .await?,
        )?;
        if s.run.phase() == Phase::CancelledBeforeStart {
            return Ok(s);
        }
        let r = self.record(&s).await?;
        let intent = journal::fact::<LaunchIntent>(&self.pool, r.id, "intent").await?;
        let guardian = journal::fact::<GuardianIdentity>(&self.pool, r.id, "guardian").await?;
        let root = self.root.clone();
        let stopped = tokio::task::spawn_blocking(move || -> Result<_> {
            let mut handle =
                SessionHost::recover(&root, &intent.context("launch intent unavailable")?)?;
            if let Some(g) = guardian {
                ensure!(handle.identity() == &g, "recovery guardian changed");
            }
            handle.wait_stopped(Duration::from_secs(10))?;
            let proof = handle.stopped_proof()?;
            Ok(proof.observation().clone())
        })
        .await?;
        match stopped {
            Ok(o) => {
                let guardian = journal::append(&self.pool, r.id, "guardian", &o.guardian).await?;
                let proof = journal::append(&self.pool, r.id, "stopped", &o).await?;
                accepted(
                    Box::pin(self.owner(None).report(
                        &s.run.plan().binding()?,
                        Event::Stopped {
                            evidence: StopEvidenceRef {
                                host: HostObservationRef {
                                    binding: s.run.plan().binding()?,
                                    launch_id: r.id,
                                    guardian,
                                    observation: proof.clone(),
                                },
                                stopped_writers: proof,
                            },
                        },
                        clock()?,
                    ))
                    .await?,
                )
            }
            Err(_) => {
                let report=journal::append(&self.pool,r.id,"lost",&serde_json::json!({"launch_id":r.id,"reason":"stopped-writer proof unavailable"})).await?;
                accepted(
                    Box::pin(self.owner(None).report(
                        &s.run.plan().binding()?,
                        Event::HostLost {
                            binding: s.run.plan().binding()?,
                            launch_id: r.id,
                            report,
                        },
                        clock()?,
                    ))
                    .await?,
                )?;
                bail!("stop uncertain; successor prohibited; scratch preserved")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn transient() -> anyhow::Error {
        anyhow::Error::from(sqlx::Error::PoolTimedOut).context("reconcile")
    }

    #[tokio::test]
    async fn transient_controller_error_is_retried_then_bounded() {
        let calls = AtomicU32::new(0);
        let tiny = Duration::from_millis(1);
        // Two transient failures, then success: the controller survives.
        let ok = retry_bounded(RETRY_ATTEMPTS, tiny, u64::MAX, is_transient, || {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            async move { if n < 2 { Err(transient()) } else { Ok(n) } }
        })
        .await
        .unwrap();
        assert_eq!((ok, calls.load(Ordering::SeqCst)), (2, 3));
        // Persistent transient failure gives up after exactly RETRY_ATTEMPTS.
        calls.store(0, Ordering::SeqCst);
        let r: Result<()> = retry_bounded(RETRY_ATTEMPTS, tiny, u64::MAX, is_transient, || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Err(transient()) }
        })
        .await;
        assert!(r.is_err() && calls.load(Ordering::SeqCst) == RETRY_ATTEMPTS);
        // Storage owner errors are transient; refusals and other errors are not.
        assert!(is_transient(&anyhow::Error::from(
            owner::OwnerError::Storage
        )));
        assert!(!is_transient(&anyhow::Error::from(
            owner::OwnerError::Conflict
        )));
        calls.store(0, Ordering::SeqCst);
        let r: Result<()> = retry_bounded(RETRY_ATTEMPTS, tiny, u64::MAX, is_transient, || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Err(anyhow::anyhow!("stop uncertain; successor prohibited")) }
        })
        .await;
        assert!(r.is_err() && calls.load(Ordering::SeqCst) == 1);
        // No retry may sleep past the permit window.
        calls.store(0, Ordering::SeqCst);
        let expired = clock().unwrap().boot_ms;
        let r: Result<()> = retry_bounded(
            RETRY_ATTEMPTS,
            tiny,
            expired,
            |_| true,
            || {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Err(transient()) }
            },
        )
        .await;
        assert!(r.is_err() && calls.load(Ordering::SeqCst) == 1);
    }
}
