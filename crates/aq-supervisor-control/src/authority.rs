//! Original central generation/credential checks shared by the command and read adapters.
use aq_central_authority::{
    CentralPlan, Event, Phase, Run, SessionIdentity,
    owner::{EvidenceVerifier, OwnerError, OwnerResult, SessionOwner, Snapshot},
};
use aq_manual_sessions::Error;
use axum::http::StatusCode;
use sha2::Digest as _;
use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;
type Result<T> = std::result::Result<T, Error>;
fn db(_: sqlx::Error) -> Error {
    unavailable()
}
fn unavailable() -> Error {
    Error(
        StatusCode::SERVICE_UNAVAILABLE,
        "Supervisor authority is unavailable",
    )
}
fn invalid() -> Error {
    Error(
        StatusCode::BAD_REQUEST,
        "Invalid supervisor authority request",
    )
}
#[derive(Clone)]
pub struct ReadOnly;
#[async_trait::async_trait]
impl EvidenceVerifier for ReadOnly {
    async fn authorize_plan(
        &self,
        _: &mut Transaction<'_, Postgres>,
        _: &CentralPlan,
    ) -> OwnerResult<()> {
        Err(OwnerError::Unsupported)
    }
    async fn verify_event(
        &self,
        _: &mut Transaction<'_, Postgres>,
        _: &Run,
        _: &Event,
    ) -> OwnerResult<()> {
        Err(OwnerError::Unsupported)
    }
}
pub fn sessions(pool: &PgPool) -> SessionOwner<ReadOnly> {
    SessionOwner::new(pool.clone(), ReadOnly)
}
pub fn active(s: &Snapshot) -> bool {
    matches!(
        s.run.phase(),
        Phase::StartCommitted | Phase::RunningReported
    )
}
pub fn session_id(plan: &CentralPlan) -> Option<Uuid> {
    match plan.session {
        SessionIdentity::Central { session_id, .. } => Some(session_id),
        _ => None,
    }
}
pub const SCOPES: [&str; 5] = [
    "graph.read",
    "graph.write",
    "enqueue",
    "messaging",
    "decisions",
];
pub const REPLY_CONTRACT: &str = "supervisor-conversation-reply-1";
const TOKEN_PREFIX: &str = "aqsv1_";
const MAX_TTL_MS: u64 = 24 * 60 * 60 * 1000;

/// The only identity a supervisor credential grants. No worker, task or attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SupervisorPrincipal {
    pub actor_id: Uuid,
    pub generation: u64,
    pub session_id: Uuid,
    pub scopes: Vec<String>,
    pub project_id: Option<Uuid>,
}

fn rejected() -> Error {
    Error(StatusCode::UNAUTHORIZED, "supervisor credential rejected")
}
fn inactive() -> Error {
    Error(StatusCode::CONFLICT, "supervisor session is not active")
}
fn token_digest(token: &str) -> String {
    hex::encode(sha2::Sha256::digest(token.as_bytes()))
}
fn well_formed(token: &str) -> bool {
    token.strip_prefix(TOKEN_PREFIX).is_some_and(|h| {
        h.len() == 64
            && h.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}
fn millis(n: u64) -> Result<i64> {
    i64::try_from(n)
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(unavailable)
}

/// The actor's head generation and session while that generation is active.
async fn current(
    pool: &PgPool,
    tx: &mut Transaction<'_, Postgres>,
    actor: Uuid,
) -> Result<Option<(u64, Uuid)>> {
    let snapshot = sessions(pool)
        .inspect_in_transaction(tx, actor)
        .await
        .map_err(|_| unavailable())?;
    Ok(snapshot.filter(active).and_then(|s| {
        let plan = s.run.plan();
        session_id(plan).map(|id| (plan.generation, id))
    }))
}

/// Issues the one live credential for the actor's active head generation.
/// Prior live credentials are revoked; only the digest is stored.
pub async fn issue_credential(
    pool: &PgPool,
    actor: Uuid,
    ttl_ms: u64,
    at_ms: u64,
) -> Result<String> {
    if ttl_ms == 0 || ttl_ms > MAX_TTL_MS {
        return Err(invalid());
    }
    let at = millis(at_ms)?;
    let expires = millis(at_ms.checked_add(ttl_ms).ok_or_else(invalid)?)?;
    let mut tx = pool.begin().await.map_err(db)?;
    let (generation, session_id) = current(pool, &mut tx, actor).await?.ok_or_else(inactive)?;
    let g = i64::try_from(generation).map_err(|_| inactive())?;
    let token = {
        use rand::RngCore;
        let mut b = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut b);
        format!("{TOKEN_PREFIX}{}", hex::encode(b))
    };
    sqlx::query("UPDATE central_supervisor_credentials SET revoked_at_ms=$2 WHERE actor_id=$1 AND revoked_at_ms IS NULL")
        .bind(actor)
        .bind(at)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    sqlx::query("INSERT INTO central_supervisor_credentials(token_digest,actor_id,generation,session_id,scopes,issued_at_ms,expires_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7)")
        .bind(token_digest(&token))
        .bind(actor)
        .bind(g)
        .bind(session_id)
        .bind(SCOPES.iter().map(|s| s.to_string()).collect::<Vec<_>>())
        .bind(at)
        .bind(expires)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    tx.commit().await.map_err(db)?;
    Ok(token)
}

/// Use-time check: present, unrevoked, unexpired, scoped, and still the
/// actor's active head generation. Anything else is 401 (403 for scope).
pub async fn authenticate(
    pool: &PgPool,
    bearer: &str,
    scope: &str,
    at_ms: u64,
) -> Result<SupervisorPrincipal> {
    let mut tx = pool.begin().await.map_err(db)?;
    let principal = authenticate_in_transaction(pool, &mut tx, bearer, scope, at_ms).await?;
    tx.commit().await.map_err(db)?;
    Ok(principal)
}

/// Authenticate inside the transaction that performs the protected operation.
/// Required scopes: graph.read for graph reads, graph.write for graph mutations,
/// enqueue for work creation, messaging for replies/mail, decisions for decisions.
/// Locks the actor before the credential, matching issuance and lifecycle writes.
/// Keep this transaction open through the protected read or write.
pub async fn authenticate_in_transaction(
    pool: &PgPool,
    tx: &mut Transaction<'_, Postgres>,
    bearer: &str,
    scope: &str,
    at_ms: u64,
) -> Result<SupervisorPrincipal> {
    if !well_formed(bearer) {
        return Err(rejected());
    }
    let actor: Uuid = sqlx::query_scalar(
        "SELECT actor_id FROM central_supervisor_credentials WHERE token_digest=$1",
    )
    .bind(token_digest(bearer))
    .fetch_optional(&mut **tx)
    .await
    .map_err(db)?
    .ok_or_else(rejected)?;
    let active = current(pool, tx, actor).await?.ok_or_else(rejected)?;
    let at = millis(at_ms)?;
    let row = sqlx::query("SELECT actor_id,generation,session_id,scopes,expires_at_ms,revoked_at_ms FROM central_supervisor_credentials WHERE token_digest=$1 FOR SHARE")
        .bind(token_digest(bearer))
        .fetch_optional(&mut **tx)
        .await
        .map_err(db)?
        .ok_or_else(rejected)?;
    if row.get::<Option<i64>, _>("revoked_at_ms").is_some()
        || row.get::<i64, _>("expires_at_ms") <= at
    {
        return Err(rejected());
    }
    let conversation: Uuid = sqlx::query_scalar(
        "SELECT conversation_id FROM central_supervisor_actors WHERE actor_id=$1",
    )
    .bind(actor)
    .fetch_one(&mut **tx)
    .await
    .map_err(db)?;
    let managed: bool =
        sqlx::query_scalar("SELECT to_regclass('supervisor_conversation_ids') IS NOT NULL")
            .fetch_one(&mut **tx)
            .await
            .map_err(db)?;
    let project_id = if managed {
        crate::project(tx, conversation).await.map_err(db)?
    } else {
        None
    };
    if managed {
        let enabled: Option<bool> =
            sqlx::query_scalar("SELECT enabled FROM supervisor_controls WHERE actor_id=$1")
                .bind(actor)
                .fetch_optional(&mut **tx)
                .await
                .map_err(db)?;
        if enabled == Some(false) {
            return Err(rejected());
        }
    }
    let principal = SupervisorPrincipal {
        actor_id: row.get("actor_id"),
        generation: u64::try_from(row.get::<i64, _>("generation")).map_err(|_| rejected())?,
        session_id: row.get("session_id"),
        scopes: row.get("scopes"),
        project_id,
    };
    if active != (principal.generation, principal.session_id) {
        return Err(rejected());
    }
    if !principal.scopes.iter().any(|s| s == scope) {
        return Err(Error(
            StatusCode::FORBIDDEN,
            "supervisor credential lacks scope",
        ));
    }
    crate::read::owner(tx, actor).await?;
    Ok(principal)
}
