//! Minute snapshots of the swarm's machine inventory and the Graph ready queue.
use crate::AppState;
use anyhow::Result;
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};
use uuid::Uuid;

mod sampler;
pub(crate) use sampler::spawn;

const MINUTE: i64 = 60_000;
const DAY: i64 = 1_440 * MINUTE;
// Reserve room for the ready series, machine metadata and the dashboard
// envelope, even when every detailed sample uses three 64-bit counters.
const MAX_DETAIL_SAMPLES: usize = 4_000;
const MAX_DISPLAY_MACHINES: usize = 256;

#[derive(Serialize)]
struct FleetMinute {
    minute_ms: i64,
    #[serde(skip)]
    observed_at_ms: i64,
    #[serde(skip)]
    inventory_complete: bool,
    count: Option<i64>,
    accuracy: String,
}

#[derive(Serialize)]
struct MachineMinute {
    minute_ms: i64,
    #[serde(skip)]
    machine_id: String,
    #[serde(skip)]
    label: String,
    state: String,
    enabled_slots: i64,
    occupied_slots: i64,
    free_slots: i64,
    #[serde(skip)]
    enabled_occupied_slots: i64,
}

fn idle_with_queue(samples: &[MachineMinute], queued: &BTreeSet<i64>) -> Vec<Value> {
    let mut runs = Vec::new();
    let (mut start, mut end) = (0, 0);
    for sample in samples {
        let idle = sample.state == "online"
            && sample.free_slots > 0
            && sample.occupied_slots == 0
            && queued.contains(&sample.minute_ms);
        if !idle || end != sample.minute_ms {
            if end - start >= 10 * MINUTE {
                runs.push(json!({ "start_ms": start, "end_ms": end }));
            }
            start = sample.minute_ms;
            end = start;
        }
        if idle {
            end = sample.minute_ms + MINUTE;
        }
    }
    if end - start >= 10 * MINUTE {
        runs.push(json!({ "start_ms": start, "end_ms": end }));
    }
    runs
}

async fn read_snapshot(pool: &PgPool) -> Result<sqlx::Transaction<'_, sqlx::Postgres>> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

pub(super) async fn machines(pool: &PgPool, cutoff: i64) -> Result<Value> {
    // The active minute is included when observed; all rows use the same grid.
    let first = cutoff.div_euclid(MINUTE) * MINUTE - DAY + MINUTE;
    let mut tx = read_snapshot(pool).await?;
    let ready = sqlx::query(
        "SELECT minute_ms,observed_at_ms,inventory_complete,ready_count AS count,ready_accuracy AS accuracy
         FROM performance_fleet_minutes WHERE minute_ms >= $1 AND observed_at_ms <= $2
         ORDER BY minute_ms",
    )
    .bind(first)
    .bind(cutoff)
    .try_map(|row| {
        Ok(FleetMinute {
            minute_ms: row.try_get("minute_ms")?,
            observed_at_ms: row.try_get("observed_at_ms")?,
            inventory_complete: row.try_get("inventory_complete")?,
            count: row.try_get("count")?,
            accuracy: row.try_get("accuracy")?,
        })
    })
    .fetch_all(&mut *tx)
    .await?;
    let samples = sqlx::query(
        "SELECT m.* FROM performance_machine_minutes m JOIN performance_fleet_minutes f USING(minute_ms)
         WHERE m.minute_ms >= $1 AND f.observed_at_ms <= $2
         ORDER BY m.minute_ms,m.machine_id",
    )
    .bind(first)
    .bind(cutoff)
    .try_map(|row| {
        Ok(MachineMinute {
            minute_ms: row.try_get("minute_ms")?,
            machine_id: row.try_get("machine_id")?,
            label: row.try_get("label")?,
            state: row.try_get("state")?,
            enabled_slots: row.try_get("enabled_slots")?,
            occupied_slots: row.try_get("occupied_slots")?,
            free_slots: row.try_get("free_slots")?,
            enabled_occupied_slots: row.try_get("enabled_occupied_slots")?,
        })
    })
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;

    let queued = ready
        .iter()
        .filter(|tick| tick.inventory_complete && tick.count.is_some_and(|count| count > 0))
        .map(|tick| tick.minute_ms)
        .collect();
    let mut grouped = BTreeMap::<String, Vec<MachineMinute>>::new();
    for sample in samples {
        grouped
            .entry(sample.machine_id.clone())
            .or_default()
            .push(sample);
    }
    let mut grouped = grouped.into_iter().collect::<Vec<_>>();
    grouped.sort_by_key(|(_, samples)| std::cmp::Reverse(samples[samples.len() - 1].minute_ms));
    let fleet_limited = grouped.len() > MAX_DISPLAY_MACHINES;
    let mut machines: Vec<Value> = Vec::new();
    let mut partial = false;
    let mut detail_left = MAX_DETAIL_SAMPLES;
    let mut detail_limited = false;
    for (id, samples) in grouped.into_iter().take(MAX_DISPLAY_MACHINES) {
        let label = &samples[samples.len() - 1].label;
        partial |= samples.iter().any(|sample| sample.state == "partial");
        let (enabled, occupied) = samples
            .iter()
            .filter(|sample| sample.state == "online")
            .fold((0, 0), |(total, used), sample| {
                (
                    total + sample.enabled_slots,
                    used + sample.enabled_occupied_slots,
                )
            });
        let detail = if samples.len() <= detail_left {
            detail_left -= samples.len();
            samples.as_slice()
        } else {
            detail_limited = true;
            &[]
        };
        machines.push(json!({
            "id": id, "label": label, "samples": detail,
            "detail_complete": detail.len() == samples.len(),
            "idle_with_queue": idle_with_queue(detail, &queued),
            "utilization": {
                "occupied_slot_minutes": occupied, "enabled_slot_minutes": enabled,
                "value": (enabled > 0).then(|| occupied as f64 / enabled as f64),
            },
        }));
    }
    let expected = DAY / MINUTE;
    let observed = ready
        .iter()
        .filter(|minute| minute.inventory_complete)
        .count() as i64;
    let (state, reason) = if ready.is_empty() {
        ("unavailable", Some("No sampled minutes in this window"))
    } else if fleet_limited {
        (
            "partial",
            Some("Only the 256 most recently sampled machines are shown"),
        )
    } else if detail_limited {
        (
            "partial",
            Some(
                "Timeline detail exceeds the response limit. Utilization totals still include all machines.",
            ),
        )
    } else if observed < expected || ready.iter().any(|tick| tick.accuracy != "exact") || partial {
        (
            "partial",
            Some("Some inventory minutes or Graph ready counts are incomplete"),
        )
    } else {
        ("complete", None)
    };
    Ok(json!({
        "state": state, "reason": reason,
        "observed_at_ms": ready.last().map(|minute| minute.observed_at_ms),
        "coverage": {"observed_minutes": observed, "expected_minutes": expected},
        "machines": machines, "ready": ready,
    }))
}

fn parse_hold_cursor(raw: &str) -> Option<(i64, Uuid, Uuid)> {
    if raw.len() > 93 {
        return None;
    }
    let mut fields = raw.split(':');
    let since = fields.next()?.parse::<i64>().ok()?;
    let project = fields.next()?.parse().ok()?;
    let task = fields.next()?.parse().ok()?;
    (since >= 0 && fields.next().is_none()).then_some((since, project, task))
}

pub(super) fn valid_hold_cursor(raw: &str) -> bool {
    parse_hold_cursor(raw).is_some()
}

/// Current recorded waits used by the supervisor sweep. The observation time
/// belongs to this read even when the historical timeline uses an older cutoff.
pub(super) async fn holds(
    pool: &PgPool,
    project: Option<Uuid>,
    observation_ms: i64,
    cursor: Option<&str>,
    limit: u32,
) -> Result<Value> {
    let (after, after_project, after_task) =
        cursor
            .and_then(parse_hold_cursor)
            .unwrap_or((-1, Uuid::nil(), Uuid::nil()));
    let limit = i64::from(limit.clamp(1, 100));
    Ok(sqlx::query_scalar(
        "WITH holds AS MATERIALIZED (
            SELECT w.project_id,w.task_id,w.reason,w.since_ms,f.next_check_at_ms,
                   CASE WHEN f.owner_attention
                                  OR (f.next_check_at_ms IS NOT NULL
                                      AND (NOT COALESCE(c.enabled,FALSE) OR w.workspace_id IS NULL))
                        THEN 'project_owner'
                        WHEN f.next_check_at_ms IS NOT NULL THEN 'project_supervisor'
                   END AS responsible_party
            FROM supervisor_hold_waits w
            LEFT JOIN supervisor_hold_followups f
              ON f.project_id=w.project_id AND f.identity_digest=w.key AND f.since_ms=w.since_ms
              AND f.closed_at_ms IS NULL
            LEFT JOIN supervisor_conversation_ids i ON i.project_id=w.project_id
            LEFT JOIN supervisor_controls c ON c.conversation_id=i.conversation_id
            WHERE ($1::uuid IS NULL OR w.project_id=$1) AND w.since_ms <= $2
         ), page AS (
            SELECT *, $2 - since_ms AS age_ms FROM holds
            WHERE (since_ms,project_id,task_id) > ($3,$4,$5)
            ORDER BY since_ms,project_id,task_id LIMIT $6
         ), next AS (
            SELECT 1 FROM holds
            WHERE (since_ms,project_id,task_id) >
              (SELECT since_ms,project_id,task_id FROM page ORDER BY since_ms DESC,project_id DESC,task_id DESC LIMIT 1)
            LIMIT 1
         )
         SELECT jsonb_build_object(
            'state','complete','reason',NULL,'observed_at_ms',$2,
            'count',count(*),'oldest_age_ms',$2-min(since_ms),
            'missed_checks',count(*) FILTER (WHERE next_check_at_ms < $2),
            'rows',COALESCE((SELECT jsonb_agg(to_jsonb(page) ORDER BY since_ms,project_id,task_id) FROM page),'[]'::jsonb),
            'next_cursor',(SELECT concat(since_ms,':',project_id,':',task_id) FROM page
                WHERE EXISTS (SELECT 1 FROM next) ORDER BY since_ms DESC,project_id DESC,task_id DESC LIMIT 1)
         ) FROM holds",
    )
    .bind(project)
    .bind(observation_ms)
    .bind(after)
    .bind(after_project)
    .bind(after_task)
    .bind(limit)
    .fetch_one(pool)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queued_idleness_requires_ten_consecutive_online_minutes() {
        let mut samples = Vec::new();
        let queued = (0..24).map(|n| n * MINUTE).collect();
        for n in 0..24 {
            samples.push(MachineMinute {
                minute_ms: n * MINUTE,
                machine_id: "worker".into(),
                label: "Worker".into(),
                state: if n == 9 { "stale" } else { "online" }.into(),
                enabled_slots: 1,
                occupied_slots: 0,
                free_slots: 1,
                enabled_occupied_slots: 0,
            });
        }
        samples.remove(13);
        assert_eq!(
            idle_with_queue(&samples, &queued),
            vec![json!({ "start_ms": 14 * MINUTE, "end_ms": 24 * MINUTE })]
        );
    }

    #[test]
    fn hold_cursor_requires_complete_key() {
        let project = Uuid::from_u128(1);
        let task = Uuid::from_u128(2);
        assert_eq!(
            parse_hold_cursor(&format!("12:{project}:{task}")),
            Some((12, project, task))
        );
        assert!(!valid_hold_cursor(&format!("12:{project}")));
        assert!(!valid_hold_cursor(&format!("-1:{project}:{task}")));
    }
}
