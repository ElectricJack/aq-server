//! Owner names, retirement and concurrency limits, independent of worker reports.
use crate::Error;
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, Row};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Setting {
    pub kind: String,
    pub id: String,
    pub label: Option<String>,
    pub max_agents: Option<u32>,
    pub removed: bool,
    pub revision: u64,
}

pub async fn available(conn: &mut PgConnection) -> Result<bool, Error> {
    at_least(conn, 110).await
}

async fn at_least(conn: &mut PgConnection, minimum: u32) -> Result<bool, Error> {
    let version: String =
        sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
            .fetch_one(conn)
            .await?;
    Ok(version.parse::<u32>().unwrap_or(0) >= minimum)
}

pub async fn lock(conn: &mut PgConnection) -> Result<(), Error> {
    if available(conn).await? {
        sqlx::query("SELECT pg_advisory_xact_lock(110110)")
            .execute(conn)
            .await?;
    }
    Ok(())
}

pub async fn settings(conn: &mut PgConnection) -> Result<Vec<Setting>, Error> {
    if !available(conn).await? {
        return Ok(Vec::new());
    }
    let rows = sqlx::query(
        "SELECT kind,id,label,max_agents,removed,revision FROM worker_settings ORDER BY kind,id",
    )
    .fetch_all(conn)
    .await?;
    rows.into_iter()
        .map(|r| {
            Ok(Setting {
                kind: r.try_get("kind")?,
                id: r.try_get("id")?,
                label: r.try_get("label")?,
                max_agents: r.try_get::<Option<i32>, _>("max_agents")?.map(|v| v as u32),
                removed: r.try_get("removed")?,
                revision: r.try_get::<i64, _>("revision")? as u64,
            })
        })
        .collect()
}

pub struct Capacity {
    pub node_max: Option<u32>,
    pub machine_max: Option<u32>,
    pub node_used: u32,
    pub machine_used: u32,
    pub removed: bool,
    pub login_pending: bool,
}

impl Capacity {
    pub fn maximum(&self, reported: u32) -> u32 {
        self.node_max
            .unwrap_or(reported)
            .min(self.machine_max.unwrap_or(u32::MAX))
    }
    pub fn full(&self) -> bool {
        self.removed
            || self.login_pending
            || self.node_max.is_some_and(|v| self.node_used >= v)
            || self.machine_max.is_some_and(|v| self.machine_used >= v)
    }
}

pub async fn capacity(conn: &mut PgConnection, node: Uuid) -> Result<Capacity, Error> {
    let mut result = Capacity {
        node_max: None,
        machine_max: None,
        node_used: 0,
        machine_used: 0,
        removed: false,
        login_pending: false,
    };
    if !available(conn).await? {
        return Ok(result);
    }
    let row = sqlx::query(
        "SELECT s.max_agents AS node_max, COALESCE(s.removed,FALSE) AS removed, m.max_agents AS machine_max, \
         (SELECT count(*) FROM execution_attempts a WHERE a.node_id=n.node_id AND a.holds_task) + \
         (SELECT count(*) FROM manual_agent_sessions a WHERE a.node_id=n.node_id AND a.stopped_at_ms IS NULL AND a.status<>'pending') AS node_used, \
         (SELECT count(*) FROM execution_attempts a JOIN worker_machine_members w ON w.node_id=a.node_id WHERE w.machine_id=n.machine_id AND a.holds_task) + \
         (SELECT count(*) FROM manual_agent_sessions a JOIN worker_machine_members w ON w.node_id=a.node_id WHERE w.machine_id=n.machine_id AND a.stopped_at_ms IS NULL AND a.status<>'pending') AS machine_used \
         FROM worker_machine_members n \
         LEFT JOIN worker_settings s ON s.kind='node' AND s.id=n.node_id::text \
         LEFT JOIN worker_settings m ON m.kind='machine' AND m.id=n.machine_id WHERE n.node_id=$1"
    ).bind(node).fetch_optional(&mut *conn).await?;
    if let Some(row) = row {
        result.node_max = row.try_get::<Option<i32>, _>("node_max")?.map(|v| v as u32);
        result.machine_max = row
            .try_get::<Option<i32>, _>("machine_max")?
            .map(|v| v as u32);
        result.node_used = row.try_get::<i64, _>("node_used")?.max(0) as u32;
        result.machine_used = row.try_get::<i64, _>("machine_used")?.max(0) as u32;
        result.removed = row.try_get("removed")?;
    }
    if at_least(conn, 111).await? {
        // A live receiver reserves the machine while replacing provider configuration.
        // This uses the same admission lock as offers/manual claims, closing the gap
        // between its idle check and restarting the user service. A dead receiver's
        // hold expires; no login bytes are stored here.
        result.login_pending=sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM worker_login_deliveries d JOIN worker_machine_members w ON w.node_id=d.node_id \
             JOIN worker_machine_members target ON target.machine_id=w.machine_id \
             WHERE target.node_id=$1 AND w.revoked_at IS NULL AND NOT EXISTS(SELECT 1 FROM worker_settings s WHERE s.kind='node' AND s.id=w.node_id::text AND s.removed) AND d.status='pending' AND d.receiver_seen_at_ms > \
             (EXTRACT(EPOCH FROM clock_timestamp())*1000)::bigint-300000)"
        ).bind(node).fetch_one(conn).await?;
    }
    Ok(result)
}

/// Only new work uses this gate. Existing work can finish after a limit is lowered.
pub async fn hold(conn: &mut PgConnection, node: Uuid) -> Result<Option<&'static str>, Error> {
    let capacity = capacity(conn, node).await?;
    if capacity.login_pending {
        return Ok(Some("installing server subscription logins"));
    }
    Ok(capacity
        .full()
        .then_some("worker or machine agent maximum reached"))
}
