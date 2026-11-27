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
/// A newer sample within this gap of the previous one is not stored.
pub const MIN_SAMPLE_GAP_MS: u64 = 5_000;

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
    /// Present while the machine cannot admit a sandbox and rechecks automatically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_admission: Option<AdmissionDeferral>,
}

/// Why a machine defers sandbox admission (`sandbox_disk_headroom_unavailable`, ...), since
/// when on the worker's clock, and for a disk shortfall the bytes available and required.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AdmissionDeferral {
    #[schemars(length(min = 1, max = 64), regex(pattern = "^[a-z0-9_]+$"))]
    pub code: String,
    pub since_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_bytes: Option<u64>,
}
impl AdmissionDeferral {
    fn validate(&self, observed_at_ms: u64) -> Result<(), &'static str> {
        let code = (1..=64).contains(&self.code.len())
            && (self.code.bytes())
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
        let bytes = self.available_bytes.is_some() == self.required_bytes.is_some();
        match code && bytes && self.since_ms <= observed_at_ms {
            true => Ok(()),
            false => Err("sandbox_admission"),
        }
    }
    /// How long the machine has deferred, on its own clock, at `observed_at_ms`.
    pub fn deferred_ms(&self, observed_at_ms: u64) -> u64 {
        observed_at_ms.saturating_sub(self.since_ms)
    }
}

impl MachineIdentity {
    fn validate(&self) -> Result<(), &'static str> {
        if let Some(key) = &self.key {
            let ok = (1..=63).contains(&key.len())
                && key
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                && key.as_bytes()[0] != b'-';
            if !ok {
                return Err("key");
            }
        }
        if self.label.as_ref().is_some_and(|label| !valid_label(label)) {
            return Err("label");
        }
        Ok(())
    }
}
fn valid_label(label: &str) -> bool {
    (1..=64).contains(&label.chars().count()) && !label.chars().any(char::is_control)
}
impl MachineReport {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.schema_version != 1 {
            return Err("schema_version");
        }
        self.machine.validate()?;
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
        (self.sandbox_admission.as_ref()).map_or(Ok(()), |d| d.validate(self.observed_at_ms))
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
    // At most one sample per MIN_SAMPLE_GAP_MS, so a chatty worker cannot skew the average.
    sqlx::query(
        "INSERT INTO node_machine_samples (node_id, received_at_ms, cpu_busy_bps, memory_used_bps) \
         SELECT $1,$2,$3,$4 WHERE NOT EXISTS (SELECT 1 FROM node_machine_samples \
           WHERE node_id=$1 AND received_at_ms > $2 - $5) ON CONFLICT DO NOTHING",
    )
    .bind(node_id)
    .bind(received)
    .bind(report.cpu_busy_bps as i16)
    .bind(report.memory_used_bps() as i16)
    .bind(MIN_SAMPLE_GAP_MS as i64)
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
    /// None when no sample fell in the window.
    pub cpu_avg_15m_bps: Option<u16>,
    pub memory_avg_15m_bps: Option<u16>,
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
            let samples = r.get::<i32, _>("samples").max(0) as u32;
            let avg = |n: u32, v: i32| (n > 0).then(|| v.clamp(0, 10_000) as u16);
            let report = serde_json::from_value(r.get("report")).ok()?;
            Some((
                r.get("node_id"),
                MachineFacts {
                    report,
                    received_at_ms: r.get::<i64, _>("received_at_ms").max(0) as u64,
                    cpu_avg_15m_bps: avg(samples, r.get("cpu_avg")),
                    memory_avg_15m_bps: avg(samples, r.get("mem_avg")),
                    samples_15m: samples,
                },
            ))
        })
        .collect())
}

/// Each node's sandbox admission deferral, from the fresh report of any node of its machine
/// (one node reports for the machine's slots, which share its disk and daemon).
pub async fn deferrals(
    conn: &mut sqlx::PgConnection,
    nodes: &[Uuid],
    now_ms: u64,
) -> sqlx::Result<BTreeMap<Uuid, (AdmissionDeferral, u64)>> {
    let rows: Vec<(Uuid, serde_json::Value)> = sqlx::query_as(
        "SELECT n.id, r.report FROM nodes n \
         JOIN nodes m ON m.id=n.id OR (m.machine_key IS NOT NULL AND m.machine_key=n.machine_key) \
         JOIN node_machine_reports r ON r.node_id=m.id \
         WHERE n.id = ANY($1) AND r.received_at_ms >= $2 AND r.report ? 'sandbox_admission'",
    )
    .bind(nodes)
    .bind(now_ms.saturating_sub(FRESH_MS) as i64)
    .fetch_all(conn)
    .await?;
    let mut out = BTreeMap::new();
    for (node, report) in rows {
        let Ok(report) = serde_json::from_value::<MachineReport>(report) else {
            continue;
        };
        if let Some(deferral) = report.sandbox_admission {
            out.insert(node, (deferral, report.observed_at_ms));
        }
    }
    Ok(out)
}
