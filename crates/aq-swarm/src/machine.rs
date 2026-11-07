//! Machine reports (spec 0007 §6): the wire contract a worker posts every
//! 15 s, its validation and its storage (latest report plus a load window).
use aq_dashboard_contract::{HostProbe, LoadSource, MachineKind};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::collections::BTreeMap;
use uuid::Uuid;

/// The mean of samples received in this window is the 15-minute average.
pub const SAMPLE_WINDOW_MS: u64 = 900_000;
/// A report received within this long is fresh.
pub const FRESH_MS: u64 = 60_000;

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MachineIdentity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub kind: MachineKind,
}

/// One machine load report. Load and memory are the machine's own (the
/// Windows host's when `source` is `windows_host`); `cpu_busy_bps` is 0-10000.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MachineReport {
    pub schema_version: u32,
    pub machine: MachineIdentity,
    pub observed_at_ms: u64,
    pub source: LoadSource,
    pub host_probe: HostProbe,
    pub logical_cpus: u32,
    pub memory_total_bytes: u64,
    pub memory_used_bytes: u64,
    #[schemars(range(max = 10000))]
    pub cpu_busy_bps: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_total_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_free_bytes: Option<u64>,
}

impl MachineReport {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.schema_version != 1 {
            return Err("schema_version");
        }
        if let Some(key) = &self.machine.key {
            let ok = (1..=63).contains(&key.len())
                && key
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                && key.as_bytes()[0] != b'-';
            if !ok {
                return Err("key");
            }
        }
        if let Some(label) = &self.machine.label
            && (!(1..=64).contains(&label.chars().count()) || label.chars().any(char::is_control))
        {
            return Err("label");
        }
        if !(1..=4096).contains(&self.logical_cpus) {
            return Err("logical_cpus");
        }
        if self.memory_total_bytes == 0 || self.memory_used_bytes > self.memory_total_bytes {
            return Err("memory");
        }
        if self.cpu_busy_bps > 10_000 {
            return Err("cpu_busy_bps");
        }
        match (self.disk_total_bytes, self.disk_free_bytes) {
            (None, None) => {}
            (Some(total), Some(free)) if free <= total => {}
            _ => return Err("disk"),
        }
        Ok(())
    }

    pub fn memory_used_bps(&self) -> u16 {
        (self.memory_used_bytes as u128 * 10_000 / self.memory_total_bytes as u128) as u16
    }
}

pub async fn record(
    pool: &sqlx::PgPool,
    node_id: Uuid,
    report: &MachineReport,
    received_at_ms: u64,
) -> sqlx::Result<()> {
    let received = received_at_ms as i64;
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO node_machine_reports (node_id, report, received_at_ms) VALUES ($1,$2,$3) \
         ON CONFLICT(node_id) DO UPDATE SET report=EXCLUDED.report, received_at_ms=EXCLUDED.received_at_ms",
    )
    .bind(node_id)
    .bind(serde_json::to_value(report).unwrap_or_default())
    .bind(received)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO node_machine_samples (node_id, received_at_ms, cpu_busy_bps, memory_used_bps) \
         VALUES ($1,$2,$3,$4) ON CONFLICT DO NOTHING",
    )
    .bind(node_id)
    .bind(received)
    .bind(report.cpu_busy_bps as i16)
    .bind(report.memory_used_bps() as i16)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM node_machine_samples WHERE node_id=$1 AND received_at_ms < $2")
        .bind(node_id)
        .bind(received - SAMPLE_WINDOW_MS as i64)
        .execute(&mut *tx)
        .await?;
    tx.commit().await
}

/// The latest report of a node with its 15-minute averages.
pub struct MachineFacts {
    pub report: MachineReport,
    pub received_at_ms: u64,
    pub cpu_avg_15m_bps: u16,
    pub memory_avg_15m_bps: u16,
    pub samples_15m: u32,
}

pub async fn latest(
    conn: &mut sqlx::PgConnection,
    nodes: &[Uuid],
    now_ms: u64,
) -> sqlx::Result<BTreeMap<Uuid, MachineFacts>> {
    let rows = sqlx::query(
        "SELECT r.node_id, r.report, r.received_at_ms, \
           COALESCE(round(avg(s.cpu_busy_bps)),0)::int AS cpu_avg, \
           COALESCE(round(avg(s.memory_used_bps)),0)::int AS mem_avg, \
           count(s.node_id)::int AS samples \
         FROM node_machine_reports r \
         LEFT JOIN node_machine_samples s ON s.node_id=r.node_id AND s.received_at_ms >= $2 \
         WHERE r.node_id = ANY($1) \
         GROUP BY r.node_id, r.report, r.received_at_ms",
    )
    .bind(nodes)
    .bind(now_ms.saturating_sub(SAMPLE_WINDOW_MS) as i64)
    .fetch_all(conn)
    .await?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            let report = serde_json::from_value(r.get("report")).ok()?;
            Some((
                r.get("node_id"),
                MachineFacts {
                    report,
                    received_at_ms: r.get::<i64, _>("received_at_ms").max(0) as u64,
                    cpu_avg_15m_bps: r.get::<i32, _>("cpu_avg").clamp(0, 10_000) as u16,
                    memory_avg_15m_bps: r.get::<i32, _>("mem_avg").clamp(0, 10_000) as u16,
                    samples_15m: r.get::<i32, _>("samples").max(0) as u32,
                },
            ))
        })
        .collect())
}
