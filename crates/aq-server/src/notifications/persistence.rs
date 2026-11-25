//! PostgreSQL owner. Lock order: notification budget -> project -> sorted users
//! -> routes -> deliveries -> idempotency receipts. Never network under locks.
use super::{Error, Result, config::Loaded};
use aq_lifecycle_api::Scope;
use aq_notifications::{Delivery, Message, Payload, Route, Status};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Row, Transaction};
use std::sync::Arc;
use uuid::Uuid;
#[derive(Clone)]
pub struct Service {
    pub pool: PgPool,
    pub configuration: Option<Arc<Loaded>>,
    pub public_origin: String,
    pub clock: crate::Clock,
}
pub use aq_notifications::{RetryRequest, RouteRequest, SendRequest};
#[derive(Serialize)]
pub struct Inbox {
    pub items: Vec<Value>,
    pub next_after: Option<i64>,
}
pub(crate) fn digest<T: Serialize>(v: &T) -> Result<String> {
    let mut value = serde_json::to_value(v)?;
    value.sort_all_objects();
    Ok(crate::tokens::verifier(&serde_json::to_string(&value)?))
}
fn revision(v: u64) -> Result<i64> {
    if v == 0 || v > i64::MAX as u64 {
        Err(Error::Invalid)
    } else {
        Ok(v as i64)
    }
}
async fn lock_budget(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    sqlx::query("SELECT singleton FROM notification_budget WHERE singleton FOR UPDATE")
        .fetch_one(&mut **tx)
        .await?;
    Ok(())
}
async fn user_role(tx: &mut Transaction<'_, Postgres>, actor: Uuid) -> Result<Option<String>> {
    Ok(
        sqlx::query_scalar("SELECT role FROM users WHERE id=$1 FOR SHARE")
            .bind(actor)
            .fetch_optional(&mut **tx)
            .await?,
    )
}
async fn owner(tx: &mut Transaction<'_, Postgres>, actor: Uuid) -> Result<()> {
    let role = user_role(tx, actor).await?;
    if role.as_deref() != Some("owner") {
        return Err(Error::Forbidden);
    }
    Ok(())
}
fn can_read(role: Option<&str>, actor: Uuid, recipient: Uuid) -> bool {
    role.is_some_and(|role| role == "owner" || actor == recipient)
}
async fn scope(tx: &mut Transaction<'_, Postgres>, scope: &Scope) -> Result<()> {
    let binding: Option<Value> = sqlx::query_scalar(
        "SELECT binding FROM project_execution_bindings WHERE project_id=$1 FOR SHARE",
    )
    .bind(scope.project_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(binding) = binding else {
        return Err(Error::NotFound);
    };
    if binding["enabled"] != true || binding["graph_workspace_id"] != scope.workspace_id.to_string()
    {
        return Err(Error::NotFound);
    }
    Ok(())
}
async fn recipient(tx: &mut Transaction<'_, Postgres>, id: Uuid) -> Result<()> {
    let found: Option<Uuid> = sqlx::query_scalar("SELECT id FROM users WHERE id=$1 FOR SHARE")
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?;
    if found.is_none() {
        return Err(Error::NotFound);
    }
    Ok(())
}
mod operations;
use operations::{Operation, Reservation};
mod configuration;
mod messages;
impl Service {
    pub(crate) fn now(&self) -> Result<u64> {
        revision(
            (self.clock)()
                .timestamp_millis()
                .try_into()
                .map_err(|_| Error::Unavailable)?,
        )
        .map(|v| v as u64)
    }
    pub async fn set_route(&self, actor: Uuid, request: RouteRequest) -> Result<Value> {
        request.route.validate()?;
        revision(request.route.revision)?;
        if request.expected_revision.checked_add(1) != Some(request.route.revision) {
            return Err(Error::Invalid);
        }
        let hash = digest(&("route/1", &request))?;
        let operation = Operation {
            actor,
            id: request.operation_id,
            hash: &hash,
        };
        let mut tx = self.pool.begin().await?;
        lock_budget(&mut tx).await?;
        scope(&mut tx, &request.route.scope).await?;
        owner(&mut tx, actor).await?;
        recipient(&mut tx, request.route.recipient_user_id).await?;
        if let Some(response) = operation.replay(&mut tx).await? {
            return Ok(response);
        }
        let old: Option<Value> = sqlx::query_scalar(
            "SELECT definition FROM notification_routes WHERE route_id=$1 FOR UPDATE",
        )
        .bind(request.route.route_id)
        .fetch_optional(&mut *tx)
        .await?;
        let previous: Option<Route> = old
            .as_ref()
            .map(|value| serde_json::from_value(value.clone()))
            .transpose()?;
        if !request.replaces(previous.as_ref()) {
            return Err(Error::Conflict);
        }
        let value = serde_json::to_value(&request.route)?;
        if serde_json::to_vec(&value)?.len() > 16384 {
            return Err(Error::Invalid);
        }
        Reservation::Route {
            created: old.is_none(),
        }
        .reserve(&mut tx)
        .await?;
        sqlx::query("INSERT INTO notification_routes(route_id,project_id,workspace_id,recipient_user_id,revision,definition,digest,endpoint_key,endpoint_revision,enabled) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) ON CONFLICT(route_id) DO UPDATE SET revision=EXCLUDED.revision,definition=EXCLUDED.definition,digest=EXCLUDED.digest,endpoint_key=EXCLUDED.endpoint_key,endpoint_revision=EXCLUDED.endpoint_revision,enabled=EXCLUDED.enabled")
            .bind(request.route.route_id).bind(request.route.scope.project_id).bind(request.route.scope.workspace_id).bind(request.route.recipient_user_id).bind(revision(request.route.revision)?).bind(value).bind(digest(&request.route)?).bind(&request.route.endpoint_key).bind(revision(request.route.endpoint_revision)?).bind(request.route.enabled).execute(&mut *tx).await?;
        let response = json!({"route_id":request.route.route_id,"revision":request.route.revision});
        operation.record(&mut tx, &response).await?;
        tx.commit().await?;
        Ok(response)
    }
}

impl Service {
    pub async fn inbox(
        &self,
        actor: Uuid,
        requested_recipient: Uuid,
        (after, limit): (i64, u32),
    ) -> Result<Inbox> {
        if after < 0 || !(1..=64).contains(&limit) {
            return Err(Error::Invalid);
        }
        let mut tx = self.pool.begin().await?;
        let role = user_role(&mut tx, actor).await?;
        if !can_read(role.as_deref(), actor, requested_recipient) {
            return Err(Error::Forbidden);
        }
        let rows=sqlx::query("SELECT m.sequence,m.definition,m.external_state FROM notification_messages m JOIN project_execution_bindings p ON p.project_id=m.project_id WHERE m.recipient_user_id=$1 AND m.sequence>$2 AND p.binding->>'graph_workspace_id'=m.workspace_id::text AND p.binding->>'enabled'='true' ORDER BY m.sequence LIMIT $3")
            .bind(requested_recipient).bind(after).bind(i64::from(limit)+1).fetch_all(&mut *tx).await?;
        let more = rows.len() > limit as usize;
        let mut items = Vec::new();
        let mut last = None;
        for row in rows.into_iter().take(limit as usize) {
            let sequence: i64 = row.get("sequence");
            last = Some(sequence);
            items.push(json!({"sequence":sequence,"message":row.get::<Value,_>("definition"),"external_state":row.get::<String,_>("external_state")}));
        }
        tx.commit().await?;
        Ok(Inbox {
            items,
            next_after: if more { last } else { None },
        })
    }
    pub async fn inspect(&self, actor: Uuid, message_id: Uuid) -> Result<Value> {
        let mut tx = self.pool.begin().await?;
        let message: Option<Value> =
            sqlx::query_scalar("SELECT definition FROM notification_messages WHERE message_id=$1")
                .bind(message_id)
                .fetch_optional(&mut *tx)
                .await?;
        let message: Message = serde_json::from_value(message.ok_or(Error::NotFound)?)?;
        message.validate()?;
        scope(&mut tx, &message.scope).await?;
        let role = user_role(&mut tx, actor).await?;
        if !can_read(role.as_deref(), actor, message.recipient_user_id) {
            return Err(Error::NotFound);
        }
        let rows=sqlx::query("SELECT state,record_revision FROM notification_deliveries WHERE message_id=$1 ORDER BY sequence LIMIT 9").bind(message_id).fetch_all(&mut *tx).await?;
        if rows.len() > 8 {
            return Err(Error::Unavailable);
        }
        let mut states = Vec::new();
        for row in rows {
            let d: Delivery = serde_json::from_value(row.get("state"))?;
            d.validate()?;
            if d.message != message {
                return Err(Error::Unavailable);
            }
            states.push(json!({"delivery_id":d.delivery_id,"record_revision":row.get::<i64,_>("record_revision"),"route_id":d.route.route_id,"route_revision":d.route.revision,"status":d.status,"attempts":d.attempts,"duplicate_risk_retries":d.duplicate_risk_retries,"human_acknowledgment":"unacknowledged","decision":"not_applicable"}));
        }
        tx.commit().await?;
        Ok(
            json!({"message":message,"external_state":if states.is_empty(){"no_external_route"}else{"routed"},"deliveries":states}),
        )
    }
}

#[path = "dispatch.rs"]
mod dispatch;

impl Service {
    /// An operator-created retry is a new durable intent, never a replay of the
    /// uncertain HTTP attempt. The frozen route must explicitly permit it too.
    pub async fn retry_unknown(&self, actor: Uuid, request: RetryRequest) -> Result<Value> {
        if !request.accept_duplicate_risk {
            return Err(Error::Invalid);
        }
        let expected = revision(request.expected_revision)?;
        let hash = digest(&("duplicate-risk-retry/1", &request))?;
        let operation = Operation {
            actor,
            id: request.operation_id,
            hash: &hash,
        };
        let mut tx = self.pool.begin().await?;
        lock_budget(&mut tx).await?;
        let row=sqlx::query("SELECT state,record_revision FROM notification_deliveries WHERE delivery_id=$1 FOR UPDATE").bind(request.delivery_id).fetch_optional(&mut *tx).await?.ok_or(Error::NotFound)?;
        let d: Delivery = serde_json::from_value(row.get("state"))?;
        d.validate()?;
        scope(&mut tx, &d.message.scope).await?;
        owner(&mut tx, actor).await?;
        recipient(&mut tx, d.message.recipient_user_id).await?;
        if let Some(response) = operation.replay(&mut tx).await? {
            return Ok(response);
        }
        if row.get::<i64, _>("record_revision") != expected {
            return Err(Error::Conflict);
        }
        let current: Option<Value> = sqlx::query_scalar(
            "SELECT definition FROM notification_routes WHERE route_id=$1 FOR SHARE",
        )
        .bind(d.route.route_id)
        .fetch_optional(&mut *tx)
        .await?;
        if current != Some(serde_json::to_value(&d.route)?) || !d.route.enabled {
            return Err(Error::Conflict);
        }
        let next = d.retry_unknown(self.now()?)?;
        dispatch::save(&mut tx, &next, expected).await?;
        Reservation::Operation.reserve(&mut tx).await?;
        let response = json!({"delivery_id":d.delivery_id,"record_revision":expected+1,"duplicate_risk_retries":next.duplicate_risk_retries,"status":next.status});
        operation.record(&mut tx, &response).await?;
        tx.commit().await?;
        Ok(response)
    }
}
