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
    let version: String =
        sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
            .fetch_one(conn)
            .await?;
    Ok(version.parse::<u32>().unwrap_or(0) >= 110)
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
}

impl Capacity {
    pub fn maximum(&self, reported: u32) -> u32 {
        self.node_max
            .unwrap_or(reported)
            .min(self.machine_max.unwrap_or(u32::MAX))
    }
    pub fn full(&self) -> bool {
        self.removed
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
    ).bind(node).fetch_optional(conn).await?;
    if let Some(row) = row {
        result.node_max = row.try_get::<Option<i32>, _>("node_max")?.map(|v| v as u32);
        result.machine_max = row
            .try_get::<Option<i32>, _>("machine_max")?
            .map(|v| v as u32);
        result.node_used = row.try_get::<i64, _>("node_used")?.max(0) as u32;
        result.machine_used = row.try_get::<i64, _>("machine_used")?.max(0) as u32;
        result.removed = row.try_get("removed")?;
    }
    Ok(result)
}

/// Only new work uses this gate. Existing work can finish after a limit is lowered.
pub async fn hold(conn: &mut PgConnection, node: Uuid) -> Result<Option<&'static str>, Error> {
    let capacity = capacity(conn, node).await?;
    Ok(capacity
        .full()
        .then_some("worker or machine agent maximum reached"))
}
