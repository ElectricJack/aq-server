//! Authenticated Boat lifecycle accounting. Node controllers report creation
//! and confirmed stop/archive; agent token costs never substitute for VM time.
use super::*;
use aq_accounting::vm_time::{Fact as VmFact, MonthlyBudget, RateCard};
use aq_admission_sandbox::sandbox::BackendKind;
use chrono::{Datelike, TimeZone, Utc};
const MAX_HEADS: i64 = 65_536;

pub fn month(now_ms: u64, ceiling_microunits: u64) -> Result<MonthlyBudget> {
    let now = chrono::DateTime::from_timestamp_millis(integer(now_ms)?).ok_or(Error::Invalid)?;
    let start = Utc
        .with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
        .single()
        .ok_or(Error::Invalid)?;
    let (year, month) = if now.month() == 12 {
        (now.year() + 1, 1)
    } else {
        (now.year(), now.month() + 1)
    };
    let end = Utc
        .with_ymd_and_hms(year, month, 1, 0, 0, 0)
        .single()
        .ok_or(Error::Invalid)?;
    Ok(MonthlyBudget {
        month_start_ms: u64::try_from(start.timestamp_millis()).map_err(|_| Error::Invalid)?,
        next_month_start_ms: u64::try_from(end.timestamp_millis()).map_err(|_| Error::Invalid)?,
        ceiling_microunits,
    })
}

pub async fn record(
    state: &AppState,
    node: &crate::worker_api::AuthenticatedNode,
    fact: &VmFact,
) -> Result<()> {
    let now_ms = u64::try_from((state.clock)().timestamp_millis()).map_err(|_| Error::Invalid)?;
    fact.validate(now_ms).map_err(|_| Error::Invalid)?;
    let mut tx = state.store.observation_pool.begin().await?;
    crate::worker_api::lock_current_node(&mut tx, node)
        .await
        .map_err(|_| Error::Refused)?;
    let kind: Option<String> = sqlx::query_scalar(
        "SELECT registration#>>'{backend,kind}' FROM delegated_node_boots WHERE node_id=$1",
    )
    .bind(node.id())
    .fetch_optional(&mut *tx)
    .await?;
    if kind.as_deref() != Some("boat") {
        return Err(Error::Refused);
    }
    let owns: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM execution_attempts WHERE attempt_id=$1 AND node_id=$2)",
    )
    .bind(fact.attempt_id)
    .bind(node.id())
    .fetch_one(&mut *tx)
    .await?;
    if !owns {
        return Err(Error::Refused);
    }
    // The node lock serializes first inserts, exact replays and the one stop
    // transition, including reports racing on another sandbox for this attempt.
    let previous: Option<Value> = sqlx::query_scalar(
        "SELECT fact FROM accounting_vm_time WHERE node_id=$1 AND (sandbox_id=$2 OR attempt_id=$3)",
    )
    .bind(node.id())
    .bind(&fact.sandbox_id)
    .bind(fact.attempt_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(previous) = previous {
        if !fact.succeeds(&parse::<VmFact>(previous)?) {
            return Err(Error::Conflict);
        }
        sqlx::query("UPDATE accounting_vm_time SET stopped_at_ms=$3,fact=$4 WHERE node_id=$1 AND sandbox_id=$2")
            .bind(node.id()).bind(&fact.sandbox_id).bind(fact.stopped_at_ms.map(integer).transpose()?).bind(value(fact)?)
            .execute(&mut *tx).await?;
    } else {
        sqlx::query("INSERT INTO accounting_vm_time(node_id,attempt_id,sandbox_id,started_at_ms,stopped_at_ms,fact,rate_card) VALUES($1,$2,$3,$4,$5,$6,$7)")
            .bind(node.id()).bind(fact.attempt_id).bind(&fact.sandbox_id).bind(integer(fact.started_at_ms)?)
            .bind(fact.stopped_at_ms.map(integer).transpose()?).bind(value(fact)?).bind(value(&RateCard::boat())?)
            .execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Read in the placement/fence transaction. Missing attempt facts are
/// unaccounted, including an old sandbox whose stop is still unknown. The
/// fence's own reservation may not have created its VM yet.
pub async fn node(
    conn: &mut sqlx::PgConnection,
    node: Uuid,
    limit: &aq_admission_sandbox::sandbox::Money,
    now_ms: u64,
    exclude_attempt: Option<Uuid>,
) -> Result<Option<aq_admission_sandbox::sandbox::Money>> {
    let window = month(now_ms, limit.micros)?;
    if limit.currency != "USD" {
        return Ok(None);
    }
    let missing: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM execution_attempts p WHERE p.node_id=$1 \
         AND p.attempt_id IS DISTINCT FROM $2 AND NOT EXISTS \
         (SELECT 1 FROM accounting_vm_time v WHERE v.node_id=p.node_id AND v.attempt_id=p.attempt_id))",
    ).bind(node).bind(exclude_attempt).fetch_one(&mut *conn).await?;
    let rows = sqlx::query(
        "SELECT fact,rate_card FROM accounting_vm_time WHERE node_id=$1 AND started_at_ms<=$2 \
         AND (stopped_at_ms IS NULL OR stopped_at_ms>$3) ORDER BY sandbox_id LIMIT $4",
    )
    .bind(node)
    .bind(integer(now_ms)?)
    .bind(integer(window.month_start_ms)?)
    .bind(MAX_HEADS + 1)
    .fetch_all(&mut *conn)
    .await?;
    if rows.len() as i64 > MAX_HEADS {
        return Err(Error::Capacity);
    }
    let mut total = 0u64;
    for row in rows {
        let fact: VmFact = parse(row.get("fact"))?;
        let card: RateCard = parse(row.get("rate_card"))?;
        if card.currency != limit.currency {
            return Ok(None);
        }
        total = total
            .checked_add(
                window
                    .cost(&fact, &card, now_ms)
                    .map_err(|_| Error::Invalid)?,
            )
            .ok_or(Error::Invalid)?;
    }
    // A known lower bound at the ceiling is sufficient to refuse even when
    // another VM has missing evidence.
    Ok(
        (!missing || window.reached(total)).then(|| aq_admission_sandbox::sandbox::Money {
            currency: limit.currency.clone(),
            micros: total,
        }),
    )
}

pub fn is_monthly_backend(kind: BackendKind) -> bool {
    kind == BackendKind::Boat
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn utc_month_rollover_includes_leap_february_and_december() {
        for (year, month_number, days) in [(2024, 2, 29), (2026, 11, 30), (2026, 12, 31)] {
            let start = Utc
                .with_ymd_and_hms(year, month_number, 1, 0, 0, 0)
                .unwrap()
                .timestamp_millis() as u64;
            let current = month(start, 18_000_000).unwrap();
            assert_eq!(current.month_start_ms, start);
            assert_eq!(current.next_month_start_ms - start, days * 86_400_000);
            let next = month(current.next_month_start_ms, 18_000_000).unwrap();
            assert_eq!(next.month_start_ms, current.next_month_start_ms);
        }
    }
}
