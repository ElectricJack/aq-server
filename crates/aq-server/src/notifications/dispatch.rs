//! Durable dispatch owner. Only `tick` may turn a committed attempt into HTTP;
//! recovery never returns that attempt to the send queue.
use super::*;
use aq_notifications::discord::{Discord, Exchange, ThrottleScope};
use aq_notifications::{DispatchAuthority, FailureReason, TransportOutcome};

const LEASE_MS: u64 = 20_000;
async fn dispatcher(tx: &mut Transaction<'_, Postgres>, id: Uuid) -> Result<()> {
    lock_budget(tx).await?;
    let current: Option<Uuid> =
        sqlx::query_scalar("SELECT dispatcher_id FROM notification_budget WHERE singleton")
            .fetch_one(&mut **tx)
            .await?;
    if current != Some(id) {
        return Err(Error::DispatcherFenced);
    }
    Ok(())
}
pub(super) async fn save(
    tx: &mut Transaction<'_, Postgres>,
    d: &Delivery,
    expected: i64,
) -> Result<()> {
    d.validate()?;
    let schedule = d.schedule(LEASE_MS)?;
    let changed=sqlx::query("UPDATE notification_deliveries SET state=$1,status=$2,due_at_ms=$3,attempt_id=$4,lease_until_ms=$5,record_revision=record_revision+1 WHERE delivery_id=$6 AND record_revision=$7")
        .bind(serde_json::to_value(d)?).bind(schedule.status).bind(schedule.due_at_ms as i64).bind(schedule.attempt_id).bind(schedule.lease_until_ms.map(|v|v as i64)).bind(d.delivery_id).bind(expected).execute(&mut **tx).await?.rows_affected();
    if changed != 1 {
        return Err(Error::Conflict);
    }
    Ok(())
}
fn exists(result: Result<()>) -> Result<bool> {
    match result {
        Ok(()) => Ok(true),
        Err(Error::NotFound) => Ok(false),
        Err(error) => Err(error),
    }
}
enum Destination {
    Unavailable,
    Busy,
    Ready,
}
async fn destination(
    tx: &mut Transaction<'_, Postgres>,
    route: &Route,
    now: u64,
) -> Result<Destination> {
    let endpoint=sqlx::query("SELECT endpoint_revision,available,disabled,not_before_ms FROM notification_destinations WHERE endpoint_key=$1 FOR SHARE").bind(&route.endpoint_key).fetch_optional(&mut **tx).await?;
    let Some(endpoint) = endpoint.filter(|e| {
        e.get::<i64, _>("endpoint_revision") as u64 == route.endpoint_revision
            && e.get::<bool, _>("available")
            && !e.get::<bool, _>("disabled")
    }) else {
        return Ok(Destination::Unavailable);
    };
    if endpoint.get::<i64, _>("not_before_ms") as u64 > now {
        return Ok(Destination::Busy);
    }
    let busy:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM notification_deliveries WHERE endpoint_key=$1 AND status='dispatching')").bind(&route.endpoint_key).fetch_one(&mut **tx).await?;
    Ok(if busy {
        Destination::Busy
    } else {
        Destination::Ready
    })
}
async fn recover_expired(tx: &mut Transaction<'_, Postgres>, now: u64) -> Result<()> {
    // A takeover waits out the prior finite attempt lease. It never
    // retransmits, including when the old process died before opening HTTP.
    let expired=sqlx::query("SELECT state,record_revision FROM notification_deliveries WHERE status='dispatching' AND lease_until_ms<=$1 ORDER BY sequence LIMIT 64 FOR UPDATE")
            .bind(now as i64).fetch_all(&mut **tx).await?;
    for row in expired {
        let d: Delivery = serde_json::from_value(row.get("state"))?;
        save(tx, &d.recover(now)?, row.get("record_revision")).await?;
    }
    Ok(())
}
async fn capacity(tx: &mut Transaction<'_, Postgres>, now: u64) -> Result<bool> {
    let provider: i64 = sqlx::query_scalar(
        "SELECT provider_not_before_ms FROM notification_budget WHERE singleton",
    )
    .fetch_one(&mut **tx)
    .await?;
    let inflight: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM notification_deliveries WHERE status='dispatching'",
    )
    .fetch_one(&mut **tx)
    .await?;
    Ok(now >= provider as u64 && inflight < 4)
}
struct Claimed {
    delivery: Delivery,
    revision: i64,
    attempt: Uuid,
    claimed: std::time::Instant,
}
impl Service {
    /// At most one send per tick. Callers use a separate bounded notification
    /// pool/task; SQL connections and locks are released before HTTP begins.
    pub async fn tick(&self, dispatcher_id: Uuid, client: &Discord) -> Result<bool> {
        let Some(claim) = self.claim(dispatcher_id).await? else {
            return Ok(false);
        };
        // The committed dispatch is already possibly effective. A pause or
        // changed configuration cannot safely turn it back into queued work.
        if claim.claimed.elapsed().as_millis() >= 1000 {
            return Ok(true);
        }
        let endpoint = self
            .configuration
            .as_ref()
            .and_then(|c| c.endpoints.get(&claim.delivery.route.endpoint_key))
            .filter(|e| e.revision == claim.delivery.route.endpoint_revision)
            .and_then(|e| e.webhook.as_ref())
            .ok_or(Error::Unavailable)?;
        let exchange = client.send(endpoint, &claim.delivery.payload).await?;
        self.finish(dispatcher_id, &claim, exchange).await?;
        Ok(true)
    }
    async fn claim(&self, dispatcher_id: Uuid) -> Result<Option<Claimed>> {
        let mut tx = self.pool.begin().await?;
        dispatcher(&mut tx, dispatcher_id).await?;
        let now = self.now()?;
        recover_expired(&mut tx, now).await?;
        if !capacity(&mut tx, now).await? {
            tx.commit().await?;
            return Ok(None);
        }
        let after: i64 =
            sqlx::query_scalar("SELECT scan_after FROM notification_budget WHERE singleton")
                .fetch_one(&mut *tx)
                .await?;
        let rows=sqlx::query("SELECT sequence,state,record_revision FROM notification_deliveries WHERE sequence>$2 AND status IN ('queued','retry_scheduled') AND due_at_ms<=$1 ORDER BY sequence LIMIT 64 FOR UPDATE")
            .bind(now as i64).bind(after).fetch_all(&mut *tx).await?;
        if rows.is_empty() {
            sqlx::query("UPDATE notification_budget SET scan_after=0 WHERE singleton")
                .execute(&mut *tx)
                .await?;
        }
        for row in rows {
            if let Some(claim) = self.claim_delivery(&mut tx, row).await? {
                tx.commit().await?;
                return Ok(Some(claim));
            }
        }
        tx.commit().await?;
        Ok(None)
    }

    async fn claim_delivery(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        row: sqlx::postgres::PgRow,
    ) -> Result<Option<Claimed>> {
        sqlx::query("UPDATE notification_budget SET scan_after=$1 WHERE singleton")
            .bind(row.get::<i64, _>("sequence"))
            .execute(&mut **tx)
            .await?;
        let mut d: Delivery = serde_json::from_value(row.get("state"))?;
        d.validate()?;
        let rev: i64 = row.get("record_revision");
        let scope_ok = exists(scope(tx, &d.message.scope).await)?;
        let recipient_ok = exists(recipient(tx, d.message.recipient_user_id).await)?;
        let current: Option<Value> = sqlx::query_scalar(
            "SELECT definition FROM notification_routes WHERE route_id=$1 FOR SHARE",
        )
        .bind(d.route.route_id)
        .fetch_optional(&mut **tx)
        .await?;
        let current: Option<Route> = current.map(serde_json::from_value).transpose()?;
        let now = self.now()?;
        if now < d.last_transition_ms {
            return Err(Error::Unavailable);
        }
        if let Some(reason) = d.suppression(current.as_ref(), scope_ok && recipient_ok, now) {
            d.status = Status::Suppressed { reason };
            d.last_transition_ms = now;
            save(tx, &d, rev).await?;
            return Ok(None);
        }
        match destination(tx, &d.route, now).await? {
            Destination::Unavailable => {
                d.status = Status::PermanentlyFailed {
                    reason: FailureReason::DestinationUnavailable,
                };
                d.last_transition_ms = now;
                save(tx, &d, rev).await?;
                return Ok(None);
            }
            Destination::Busy => return Ok(None),
            Destination::Ready => {}
        }
        let attempt = Uuid::new_v4();
        let authority = DispatchAuthority {
            route: current.as_ref().ok_or(Error::Unavailable)?,
            recipient_can_view: scope_ok && recipient_ok,
        };
        let next = d.dispatch_or_exhaust(authority, attempt, now)?;
        save(tx, &next, rev).await?;
        if !matches!(next.status, Status::Dispatching { .. }) {
            return Ok(None);
        }
        // The local pause bound includes the commit.
        Ok(Some(Claimed {
            delivery: next,
            revision: rev + 1,
            attempt,
            claimed: std::time::Instant::now(),
        }))
    }
    async fn finish(&self, dispatcher_id: Uuid, claim: &Claimed, exchange: Exchange) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        dispatcher(&mut tx, dispatcher_id).await?;
        let row=sqlx::query("SELECT state,record_revision FROM notification_deliveries WHERE delivery_id=$1 FOR UPDATE").bind(claim.delivery.delivery_id).fetch_one(&mut *tx).await?;
        let d: Delivery = serde_json::from_value(row.get("state"))?;
        if row.get::<i64, _>("record_revision") != claim.revision || d != claim.delivery {
            return Err(Error::Conflict);
        }
        let now = self.now()?;
        if let Some(throttle) = exchange.throttle {
            let due = now
                .checked_add(throttle.after_ms)
                .filter(|v| *v <= i64::MAX as u64)
                .ok_or(Error::Unavailable)?;
            match throttle.scope {
                ThrottleScope::Provider => {
                    sqlx::query("UPDATE notification_budget SET provider_not_before_ms=GREATEST(provider_not_before_ms,$1) WHERE singleton").bind(due as i64).execute(&mut *tx).await?;
                }
                ThrottleScope::Destination => {
                    sqlx::query("UPDATE notification_destinations SET not_before_ms=GREATEST(not_before_ms,$1) WHERE endpoint_key=$2 AND endpoint_revision=$3").bind(due as i64).bind(&d.route.endpoint_key).bind(d.route.endpoint_revision as i64).execute(&mut *tx).await?;
                }
            }
        }
        if exchange.outcome == TransportOutcome::Permanent(FailureReason::DestinationUnavailable) {
            sqlx::query("UPDATE notification_destinations SET disabled=TRUE WHERE endpoint_key=$1 AND endpoint_revision=$2").bind(&d.route.endpoint_key).bind(d.route.endpoint_revision as i64).execute(&mut *tx).await?;
        }
        let jitter = Uuid::new_v4().as_u128() as u64;
        let next = d.observe(
            aq_notifications::AttemptOutcome::new(claim.attempt, exchange.outcome),
            now,
            jitter,
        )?;
        save(&mut tx, &next, claim.revision).await?;
        tx.commit().await?;
        Ok(())
    }
}
