//! Dormant trusted-process persistence, not an authenticated network API.
//!
//! The embedding must implement EvidenceVerifier using the real policy, mailbox
//! and physical host evidence, in the supplied transaction. There is deliberately
//! no production verifier or daemon activation until the subject-neutral host
//! contract is available. Artifact references and caller-supplied clocks alone
//! are not authority. The embedding obtains CLOCK_BOOTTIME on the host's boot.
mod history;
use crate::*;
use sqlx::{PgPool, Postgres, Row, Transaction, postgres::PgRow};
use std::{future::Future, time::Duration};

pub const MAX_GENERATIONS: u64 = 1024;
pub type OwnerResult<T> = std::result::Result<T, OwnerError>;

#[derive(Debug, thiserror::Error)]
pub enum OwnerError {
    #[error("central model: {0}")]
    Model(#[from] Error),
    #[error("central session binding conflicts")]
    Conflict,
    #[error("central session not found")]
    Missing,
    #[error("central evidence not verified")]
    Evidence,
    #[error("central retained generation capacity exhausted")]
    Capacity,
    #[error("central persistence unavailable or inconsistent")]
    Storage,
    #[error("unsupported owner report")]
    Unsupported,
}
impl From<sqlx::Error> for OwnerError {
    fn from(_: sqlx::Error) -> Self {
        Self::Storage
    }
}

/// Trusted, installed code only. No permissive/default verifier is provided.
/// Implementations must check current authorization and original evidence under
/// the supplied transaction, without network or process effects. A physical stop
/// reference must resolve to exact stopped-writer proof, never PID disappearance.
#[async_trait::async_trait]
pub trait EvidenceVerifier: Send + Sync {
    async fn authorize_plan(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        plan: &CentralPlan,
    ) -> OwnerResult<()>;
    async fn verify_event(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        run: &Run,
        event: &Event,
    ) -> OwnerResult<()>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub revision: u64,
    pub run: Run,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Receipt {
    pub snapshot: Snapshot,
    pub decision: Decision,
}
pub struct SessionOwner<V> {
    pool: PgPool,
    verifier: V,
}
struct Head {
    actor: Uuid,
    conversation: Uuid,
    authority: Uuid,
    generation: u64,
}
fn session(p: &CentralPlan) -> (Uuid, Uuid) {
    match p.session {
        SessionIdentity::Central {
            authority_id,
            session_id,
            ..
        } => (authority_id, session_id),
        // All call sites first validate the plan or a retained Run.
        SessionIdentity::Remote { .. } => unreachable!("validated central plan"),
    }
}
fn number(n: u64) -> OwnerResult<i64> {
    i64::try_from(n).map_err(|_| OwnerError::Conflict)
}
fn state_bytes(run: &Run) -> OwnerResult<String> {
    String::from_utf8(encode(&Document::State(run.clone()))?).map_err(|_| OwnerError::Storage)
}
async fn bounded<T>(future: impl Future<Output = OwnerResult<T>>) -> OwnerResult<T> {
    tokio::time::timeout(Duration::from_secs(4), future)
        .await
        .map_err(|_| OwnerError::Storage)?
}
async fn begin(pool: &PgPool) -> OwnerResult<Transaction<'_, Postgres>> {
    let mut tx = pool.begin().await?;
    // Two prepared statements, not raw_sql: raw_sql's Executor bound defeats
    // Send inference for owner futures awaited from HTTP handlers.
    sqlx::query("SET LOCAL lock_timeout='2s'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL statement_timeout='3s'")
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}
/// A read-only REPEATABLE READ snapshot: the head and generation rows are read
/// consistently without row locks, so inspection never blocks a writer.
async fn begin_read(pool: &PgPool) -> OwnerResult<Transaction<'_, Postgres>> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL statement_timeout='3s'")
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}
const HEAD: &str = "SELECT actor_id,conversation_id,authority_id,current_generation FROM central_session_heads WHERE actor_id=$1";
async fn head(tx: &mut Transaction<'_, Postgres>, actor: Uuid) -> OwnerResult<Option<Head>> {
    read_head(tx, actor, true).await
}
async fn read_head(
    tx: &mut Transaction<'_, Postgres>,
    actor: Uuid,
    lock: bool,
) -> OwnerResult<Option<Head>> {
    id(actor)?;
    let sql = if lock {
        format!("{HEAD} FOR UPDATE")
    } else {
        HEAD.to_owned()
    };
    let row = sqlx::query(&sql)
        .bind(actor)
        .fetch_optional(&mut **tx)
        .await?;
    row.map(|row| {
        let generation: i64 = row.try_get("current_generation")?;
        if !(1..=MAX_GENERATIONS as i64).contains(&generation) {
            return Err(OwnerError::Storage);
        }
        Ok(Head {
            actor,
            conversation: row.try_get("conversation_id")?,
            authority: row.try_get("authority_id")?,
            generation: generation as u64,
        })
    })
    .transpose()
}
fn snapshot(row: PgRow, h: &Head) -> OwnerResult<Snapshot> {
    let bytes: String = row.try_get("state_bytes")?;
    let hash: String = row.try_get("state_digest")?;
    if Digest::of(bytes.as_bytes()).as_str() != hash {
        return Err(OwnerError::Storage);
    }
    let Document::State(run) = decode(bytes.as_bytes()).map_err(|_| OwnerError::Storage)? else {
        return Err(OwnerError::Storage);
    };
    let revision: i64 = row.try_get("revision")?;
    let generation: i64 = row.try_get("generation")?;
    let session_id: Uuid = row.try_get("session_id")?;
    if revision < 1
        || generation < 1
        || run.plan().generation != generation as u64
        || run.plan().subject.actor_id() != h.actor
        || run.plan().subject.conversation_id() != h.conversation
        || session(run.plan()) != (h.authority, session_id)
        || generation as u64 > h.generation
    {
        return Err(OwnerError::Storage);
    }
    Ok(Snapshot {
        revision: revision as u64,
        run,
    })
}
async fn load(
    tx: &mut Transaction<'_, Postgres>,
    h: &Head,
    generation: u64,
) -> OwnerResult<Snapshot> {
    let row = sqlx::query(
        "SELECT * FROM central_session_generations WHERE actor_id=$1 AND generation=$2",
    )
    .bind(h.actor)
    .bind(number(generation)?)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(OwnerError::Storage)?;
    snapshot(row, h)
}
async fn current(
    tx: &mut Transaction<'_, Postgres>,
    binding: &CentralBinding,
) -> OwnerResult<(Head, Snapshot)> {
    binding.validate()?;
    let h = head(tx, binding.subject.actor_id())
        .await?
        .ok_or(OwnerError::Missing)?;
    if binding.generation != h.generation {
        return Err(OwnerError::Conflict);
    }
    let s = load(tx, &h, h.generation).await?;
    if s.run.plan().binding()? != *binding {
        return Err(OwnerError::Conflict);
    }
    Ok((h, s))
}
async fn save(
    tx: &mut Transaction<'_, Postgres>,
    old: Snapshot,
    transition: Transition,
) -> OwnerResult<Receipt> {
    let mut revision = old.revision;
    if old.run != transition.state {
        revision = revision.checked_add(1).ok_or(OwnerError::Capacity)?;
        let bytes = state_bytes(&transition.state)?;
        let changed = sqlx::query("UPDATE central_session_generations SET revision=$1,state_bytes=$2,state_digest=$6 WHERE actor_id=$3 AND generation=$4 AND revision=$5")
            .bind(number(revision)?).bind(&bytes).bind(old.run.plan().subject.actor_id())
            .bind(number(old.run.plan().generation)?).bind(number(old.revision)?).bind(Digest::of(bytes.as_bytes()).as_str()).execute(&mut **tx).await?;
        if changed.rows_affected() != 1 {
            return Err(OwnerError::Conflict);
        }
    }
    Ok(Receipt {
        snapshot: Snapshot {
            revision,
            run: transition.state,
        },
        decision: transition.decision,
    })
}

impl<V: EvidenceVerifier> SessionOwner<V> {
    pub fn new(pool: PgPool, verifier: V) -> Self {
        Self { pool, verifier }
    }

    /// Reserve the exact immutable plan. Replays return persisted state, never
    /// a freshness claim. Successors use every retained generation's mail fence.
    pub async fn prepare(&self, plan: CentralPlan, clock: Clock) -> OwnerResult<Snapshot> {
        bounded(async {
            plan.validate()?;
            clock.validate()?;
            let mut tx = begin(&self.pool).await?;
            let inserted = sqlx::query("INSERT INTO central_session_heads(actor_id,conversation_id,authority_id,current_generation) VALUES($1,$2,$3,0) ON CONFLICT DO NOTHING")
                .bind(plan.subject.actor_id()).bind(plan.subject.conversation_id()).bind(session(&plan).0).execute(&mut *tx).await?.rows_affected() == 1;
            // An inserted row is already exclusively owned by this transaction;
            // an existing head must be locked before authorization is checked.
            // A skipped insert with no head for this actor means another actor
            // owns the UNIQUE conversation_id: a typed conflict, not storage.
            let h = if inserted { None } else { Some(head(&mut tx, plan.subject.actor_id()).await?.ok_or(OwnerError::Conflict)?) };
            self.verifier.authorize_plan(&mut tx, &plan).await?;
            let run = if let Some(h) = h {
                if h.conversation != plan.subject.conversation_id() || h.authority != session(&plan).0 { return Err(OwnerError::Conflict); }
                let old = load(&mut tx, &h, h.generation).await?;
                if old.run.plan() == &plan { tx.commit().await?; return Ok(old); }
                if h.generation >= MAX_GENERATIONS { return Err(OwnerError::Capacity); }
                let history = history::fence(&mut tx, &h, &plan).await?;
                old.run.successor(plan, clock, &history)?
            } else {
                Run::new(plan, clock)?
            };
            let p = run.plan();
            sqlx::query("UPDATE central_session_heads SET current_generation=$1 WHERE actor_id=$2")
                .bind(number(p.generation)?).bind(p.subject.actor_id()).execute(&mut *tx).await?;
            let bytes = state_bytes(&run)?;
            sqlx::query("INSERT INTO central_session_generations(actor_id,generation,session_id,revision,state_bytes,state_digest) VALUES($1,$2,$3,1,$4,$5)")
                .bind(p.subject.actor_id()).bind(number(p.generation)?).bind(session(p).1).bind(&bytes).bind(Digest::of(bytes.as_bytes()).as_str()).execute(&mut *tx).await?;
            tx.commit().await?;
            Ok(Snapshot { revision: 1, run })
        }).await
    }

    /// Commit a start/dispatch association and issue its first permit before
    /// returning. This performs no physical launch or mailbox delivery.
    pub async fn start(
        &self,
        binding: &CentralBinding,
        launch: HostLaunchRef,
        ttl_ms: u64,
        clock: Clock,
    ) -> OwnerResult<Receipt> {
        bounded(async {
            let mut tx = begin(&self.pool).await?;
            let (_, old) = current(&mut tx, binding).await?;
            self.verifier
                .authorize_plan(&mut tx, old.run.plan())
                .await?;
            let dispatch = if let Some(dispatch) = old.run.association() {
                dispatch.clone()
            } else {
                old.run
                    .plan()
                    .dispatch(issue(&old.run, 1, ttl_ms, &clock)?)?
            };
            let event = Event::CommitStart { launch, dispatch };
            event.validate()?;
            self.verifier
                .verify_event(&mut tx, &old.run, &event)
                .await?;
            let next = old.run.transition(&event, clock)?;
            let receipt = save(&mut tx, old, next).await?;
            tx.commit().await?;
            Ok(receipt)
        })
        .await
    }

    /// Compare the prior sequence under the actor lock and issue the next permit.
    /// A lost response is resolved by inspect, not by issuing another sequence.
    pub async fn renew(
        &self,
        binding: &CentralBinding,
        prior_sequence: u64,
        ttl_ms: u64,
        clock: Clock,
    ) -> OwnerResult<Receipt> {
        bounded(async {
            let mut tx = begin(&self.pool).await?;
            let (_, old) = current(&mut tx, binding).await?;
            self.verifier
                .authorize_plan(&mut tx, old.run.plan())
                .await?;
            let tick = old.run.transition(&Event::Tick, clock.clone())?;
            let next = if !matches!(tick.decision, Decision::Applied)
                || !matches!(
                    tick.state.phase(),
                    Phase::StartCommitted | Phase::RunningReported
                ) {
                Transition {
                    state: tick.state,
                    decision: if matches!(tick.decision, Decision::Applied) {
                        Decision::Refused {
                            reason: Refusal::Inactive,
                        }
                    } else {
                        tick.decision
                    },
                }
            } else if tick.state.permit().map(|p| p.sequence) != Some(prior_sequence) {
                Transition {
                    state: tick.state,
                    decision: Decision::Refused {
                        reason: Refusal::Sequence,
                    },
                }
            } else {
                let sequence = prior_sequence.checked_add(1).ok_or(OwnerError::Capacity)?;
                let permit = issue(&tick.state, sequence, ttl_ms, &clock)?;
                tick.state.transition(&Event::Renew { permit }, clock)?
            };
            let receipt = save(&mut tx, old, next).await?;
            tx.commit().await?;
            Ok(receipt)
        })
        .await
    }

    /// Record only independently verified host/control/mail reports. No public
    /// endpoint accepts these references, and reports cannot manufacture permits.
    pub async fn report(
        &self,
        binding: &CentralBinding,
        event: Event,
        clock: Clock,
    ) -> OwnerResult<Receipt> {
        bounded(async {
            if matches!(event, Event::CommitStart { .. } | Event::Renew { .. }) {
                return Err(OwnerError::Unsupported);
            }
            event.validate()?;
            let mut tx = begin(&self.pool).await?;
            let (_, old) = current(&mut tx, binding).await?;
            // Cleanup and authority revocation must remain possible after plan
            // authorization is revoked; verifier authorizes each report itself.
            self.verifier
                .verify_event(&mut tx, &old.run, &event)
                .await?;
            let next = old.run.transition(&event, clock)?;
            let receipt = save(&mut tx, old, next).await?;
            tx.commit().await?;
            Ok(receipt)
        })
        .await
    }

    /// Historical data only: neither inspection nor reconstruction renews a
    /// permit, observes a live process, or authorizes physical re-adoption.
    /// Reads take no row locks (a read-only snapshot), so the `dispatch::live`
    /// capability probe and credential checks never wait on a writer.
    // Reads return boxed Send futures so HTTP handlers can await them.
    pub fn inspect(&self, actor: Uuid) -> Read<'_> {
        Box::pin(self.read(actor, None))
    }
    /// Read and validate the current head under the caller's transaction. The
    /// actor lock is retained until that transaction commits or rolls back, so
    /// an embedding can authorize a write atomically with lifecycle changes.
    pub async fn inspect_in_transaction(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        actor: Uuid,
    ) -> OwnerResult<Option<Snapshot>> {
        let Some(h) = head(tx, actor).await? else {
            return Ok(None);
        };
        load(tx, &h, h.generation).await.map(Some)
    }
    pub fn generation(&self, actor: Uuid, generation: u64) -> Read<'_> {
        if generation == 0 || generation > MAX_GENERATIONS {
            return Box::pin(async { Err(OwnerError::Conflict) });
        }
        Box::pin(self.read(actor, Some(generation)))
    }
    async fn read(&self, actor: Uuid, generation: Option<u64>) -> OwnerResult<Option<Snapshot>> {
        bounded(async {
            let mut tx = begin_read(&self.pool).await?;
            let Some(h) = read_head(&mut tx, actor, false).await? else {
                return Ok(None);
            };
            let g = generation.unwrap_or(h.generation);
            if g > h.generation {
                return Ok(None);
            }
            let s = load(&mut tx, &h, g).await?;
            tx.commit().await?;
            Ok(Some(s))
        })
        .await
    }
}
/// A boxed, Send snapshot read.
pub type Read<'a> =
    std::pin::Pin<Box<dyn Future<Output = OwnerResult<Option<Snapshot>>> + Send + 'a>>;

fn issue(run: &Run, sequence: u64, ttl_ms: u64, clock: &Clock) -> OwnerResult<PermitData> {
    clock.validate()?;
    ensure(
        ttl_ms > 0 && ttl_ms <= run.plan().max_permit_ms && ttl_ms <= MAX_PERMIT_MS,
        "permit duration",
    )?;
    let expires = clock
        .boot_ms
        .checked_add(ttl_ms)
        .ok_or(Error::Invalid("permit overflow"))?
        .min(run.original_deadline_boot_ms());
    let permit = PermitData {
        binding: run.plan().binding()?,
        machine_boot_id: clock.machine_boot_id,
        sequence,
        issued_boot_ms: clock.boot_ms,
        expires_boot_ms: expires,
    };
    permit.validate()?;
    Ok(permit)
}
