//! Current-Owner reservations over the actual global mailbox. Completed turns
//! consume their reservation while immutable dispatch history retains mail holds.
mod reads;
#[cfg(test)]
mod tests;
mod types;
mod writes;
use super::{Service, definition, key, owner as conversation_owner};
use crate::{
    AppState,
    error::AppError,
    store::{Role, User},
};
use aq_central_authority::{Digest, MailBatch, MailRef};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Row, Transaction};
use std::{future::Future, time::Duration};
pub use types::*;
use uuid::Uuid;
type Result<T> = std::result::Result<T, AppError>;
const CONTRACT: &str = "central-reservation-1";
const MAX_OPS: i64 = 4096;
const MAX_BYTES: i64 = 67108864;
const CONTROL_BYTES: i64 = 131072;
const MAX_SCAN: usize = 128;
fn invalid() -> AppError {
    AppError::BadRequest("invalid bounded central reservation request".into())
}
fn unavailable() -> AppError {
    AppError::IdentityUnavailable("central reservation source unavailable".into())
}
fn conflict() -> AppError {
    AppError::Conflict("central reservation identity or version conflicts".into())
}
fn missing() -> AppError {
    AppError::NotFound("central reservation record not found".into())
}
fn capacity() -> AppError {
    AppError::Maintenance("central reservation retention capacity exhausted".into())
}
fn db(_: sqlx::Error) -> AppError {
    unavailable()
}
fn canonical<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    aq_agent_api::canonical(&json!(value)).map_err(|_| invalid())
}
fn hash<T: Serialize>(value: &T) -> Result<Digest> {
    Ok(Digest::of(&canonical(value)?))
}
fn parse<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    if bytes.is_empty() || bytes.len() > 16384 {
        return Err(invalid());
    }
    serde_json::from_value(aq_agent_api::parse(bytes).map_err(|_| invalid())?)
        .map_err(|_| invalid())
}
fn positive(n: u64) -> Result<i64> {
    i64::try_from(n).ok().filter(|n| *n > 0).ok_or_else(invalid)
}
fn cursor(n: u64) -> Result<i64> {
    i64::try_from(n).map_err(|_| invalid())
}
fn uuid(id: Uuid) -> Result<()> {
    if id.is_nil() { Err(invalid()) } else { Ok(()) }
}
fn identity(user: &User) -> Value {
    json!({"id":user.id,"email":user.email,"issuer":user.issuer,"subject":user.subject,"role":"owner"})
}
async fn authorize(tx: &mut Transaction<'_, Postgres>, user: &User) -> Result<()> {
    if user.role != Role::Owner {
        return Err(AppError::Forbidden("current Owner required".into()));
    }
    conversation_owner(tx, user).await
}
fn command<T: Serialize>(kind: &str, user: &User, request: &T) -> Result<(String, Digest)> {
    let bytes = canonical(
        &json!({"contract":CONTRACT,"kind":kind,"owner":identity(user),"request":request}),
    )?;
    if bytes.len() > 16384 {
        return Err(invalid());
    }
    let digest = Digest::of(&bytes);
    Ok((String::from_utf8(bytes).map_err(|_| invalid())?, digest))
}
struct Actor {
    id: Uuid,
    conversation: Uuid,
    revision: i64,
    policy_revision: i64,
    policy: LocalPolicy,
    policy_bytes: String,
    policy_digest: Digest,
    current: Option<Uuid>,
    operations: i64,
    bytes: i64,
}
impl Actor {
    fn from_row(row: sqlx::postgres::PgRow) -> Result<Self> {
        let policy_bytes: String = row.get("policy_bytes");
        let policy: LocalPolicy =
            serde_json::from_slice(policy_bytes.as_bytes()).map_err(|_| unavailable())?;
        policy.validate().map_err(|_| unavailable())?;
        let a = Self {
            id: row.get("actor_id"),
            conversation: row.get("conversation_id"),
            revision: row.get("revision"),
            policy_revision: row.get("policy_revision"),
            policy,
            policy_digest: serde_json::from_value(json!(row.get::<String, _>("policy_digest")))
                .map_err(|_| unavailable())?,
            policy_bytes,
            current: row.get("current_reservation"),
            operations: row.get("retained_operations"),
            bytes: row.get("retained_bytes"),
        };
        if a.id != a.policy.actor_id
            || a.conversation != a.policy.conversation_id
            || canonical(&a.policy)? != a.policy_bytes.as_bytes()
            || Digest::of(a.policy_bytes.as_bytes()) != a.policy_digest
            || a.revision != a.operations
            || a.policy_revision < 1
            || a.policy_revision > a.revision
            || !(1..=MAX_OPS).contains(&a.operations)
            || !(1..=MAX_BYTES).contains(&a.bytes)
            || a.current.is_some()
                && (a.operations > MAX_OPS - 1 || a.bytes > MAX_BYTES - CONTROL_BYTES)
        {
            return Err(unavailable());
        }
        Ok(a)
    }
}
/// An in-process owner service. The caller must supply independently authenticated
/// User context; methods still recheck its full current identity under lock.
pub struct ReservationOwner {
    service: Service,
}
impl ReservationOwner {
    /// Trusted central controller; each operation still checks the live complete Owner.
    pub fn with_pool(pool: PgPool) -> Self {
        Self {
            service: Service {
                pool,
                gate: tokio::sync::Semaphore::new(2),
            },
        }
    }

    pub fn new(state: &AppState) -> Self {
        Self {
            service: Service::new(state),
        }
    }
    async fn bounded<T>(&self, future: impl Future<Output = Result<T>>) -> Result<T> {
        let _permit = self.service.gate.try_acquire().map_err(|_| unavailable())?;
        tokio::time::timeout(Duration::from_secs(4), future)
            .await
            .map_err(|_| unavailable())?
    }
    pub async fn configure_actor(&self, user: &User, bytes: &[u8]) -> Result<Receipt> {
        self.bounded(self.configure(user, bytes)).await
    }
    pub async fn reserve_batch(&self, user: &User, bytes: &[u8]) -> Result<Receipt> {
        self.bounded(self.reserve(user, bytes)).await
    }
    pub async fn cancel_reservation(&self, user: &User, bytes: &[u8]) -> Result<Receipt> {
        self.bounded(self.cancel(user, bytes)).await
    }
    pub async fn preview_batch(&self, user: &User, request: PreviewRequest) -> Result<Preview> {
        self.bounded(self.preview(user, request)).await
    }
    pub async fn inspect_actor(&self, user: &User, actor: Uuid) -> Result<ActorView> {
        self.bounded(self.inspect(user, actor)).await
    }
    pub async fn lookup_operation(
        &self,
        user: &User,
        actor: Uuid,
        operation: Uuid,
    ) -> Result<Lookup> {
        self.bounded(self.lookup(user, actor, operation)).await
    }
    pub async fn history(
        &self,
        user: &User,
        actor: Uuid,
        before: Option<u64>,
        limit: u8,
    ) -> Result<History> {
        self.bounded(self.read_history(user, actor, before, limit))
            .await
    }
}
async fn actor(tx: &mut Transaction<'_, Postgres>, id: Uuid, write: bool) -> Result<Option<Actor>> {
    uuid(id)?;
    let sql = if write {
        "SELECT * FROM central_supervisor_actors WHERE actor_id=$1 FOR UPDATE"
    } else {
        "SELECT * FROM central_supervisor_actors WHERE actor_id=$1 FOR SHARE"
    };
    let Some(row) = sqlx::query(sql)
        .bind(id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(db)?
    else {
        return Ok(None);
    };
    let a = Actor::from_row(row)?;
    let history=sqlx::query("SELECT count(*) AS n,COALESCE(sum(charged_bytes),0)::bigint AS bytes,COALESCE(max(actor_revision),0) AS revision,COALESCE(bool_and(kind IN ('configure','reserve','cancel')),TRUE) AS known FROM central_supervisor_operations WHERE actor_id=$1").bind(id).fetch_one(&mut **tx).await.map_err(db)?;
    let active:Vec<Uuid>=sqlx::query_scalar("SELECT reservation_id FROM central_supervisor_reservations WHERE actor_id=$1 AND state='awaiting_prepared_plan' LIMIT 2").bind(id).fetch_all(&mut **tx).await.map_err(db)?;
    let unknown:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM central_supervisor_reservations WHERE actor_id=$1 AND state NOT IN ('awaiting_prepared_plan','cancelled_before_plan','consumed'))").bind(id).fetch_one(&mut **tx).await.map_err(db)?;
    if history.get::<i64, _>("n") != a.operations
        || history.get::<i64, _>("bytes") != a.bytes
        || history.get::<i64, _>("revision") != a.revision
        || !history.get::<bool, _>("known")
        || unknown
        || active != a.current.into_iter().collect::<Vec<_>>()
    {
        return Err(unavailable());
    }
    Ok(Some(a))
}
async fn bound_actor(
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    id: Uuid,
    write: bool,
) -> Result<Actor> {
    authorize(tx, user).await?;
    let conversation: Uuid = sqlx::query_scalar(
        "SELECT conversation_id FROM central_supervisor_actors WHERE actor_id=$1",
    )
    .bind(id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db)?
    .ok_or_else(missing)?;
    let c = definition(tx, Some(conversation), false).await?;
    let a = actor(tx, id, write).await?.ok_or_else(missing)?;
    if a.conversation != c.id {
        return Err(unavailable());
    }
    Ok(a)
}
/// F1b: read one reservation for the dispatch path (no Owner context; actor-scoped).
pub(crate) async fn load(
    tx: &mut Transaction<'_, Postgres>,
    actor_id: Uuid,
    id: Uuid,
) -> Result<ReservationView> {
    let a = actor(tx, actor_id, false).await?.ok_or_else(missing)?;
    reads::reservation(tx, &a, id, false)
        .await?
        .ok_or_else(missing)
}
