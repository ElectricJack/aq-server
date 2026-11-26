//! The per-node allocation row (migration 0185). The `nodes` row is only the
//! node's credential, checked with FOR KEY SHARE. Work that must exclude other
//! work on a node locks its `node_allocation` row instead: slot allocation and
//! offer publication, manual-session claims, runtime registration, and owner
//! fences, which also bump `generation` so a SERIALIZABLE publication that read
//! the old state fails to serialize. Rows are always locked in node id order.
use crate::Error;
use sqlx::PgConnection;
use uuid::Uuid;

/// Format 185 created the rows. A frozen fixture profile before it has none,
/// and locks nothing here, as the format checks around this code do.
pub async fn present(conn: &mut PgConnection) -> Result<bool, Error> {
    let version: String =
        sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
            .fetch_one(&mut *conn)
            .await?;
    Ok(version.parse::<u32>().unwrap_or(0) >= 185)
}

/// Locks `node`'s row. When its machine has a maximum, every row of that
/// machine is locked, so allocations on sibling nodes count each other.
pub async fn lock(conn: &mut PgConnection, node: Uuid) -> Result<(), Error> {
    rows(conn, node, false).await
}

/// Locks every row of `node`'s machine: for work that needs the whole machine
/// idle, such as installing provider logins.
pub async fn lock_machine(conn: &mut PgConnection, node: Uuid) -> Result<(), Error> {
    rows(conn, node, true).await
}

async fn rows(conn: &mut PgConnection, node: Uuid, machine: bool) -> Result<(), Error> {
    if !present(conn).await? {
        return Ok(());
    }
    sqlx::query(
        "SELECT a.node_id FROM node_allocation a \
         JOIN worker_machine_members w ON w.node_id=a.node_id \
         JOIN worker_machine_members me ON me.machine_id=w.machine_id AND me.node_id=$1 \
         WHERE a.node_id=$1 OR $2 OR EXISTS(SELECT 1 FROM worker_settings s \
          WHERE s.kind='machine' AND s.id=me.machine_id AND s.max_agents IS NOT NULL) \
         ORDER BY a.node_id FOR UPDATE OF a",
    )
    .bind(node)
    .bind(machine)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// An owner change that publication and claims must see: locks the rows of
/// `nodes` (every node when `None`) in id order and bumps their generation.
pub async fn fence(conn: &mut PgConnection, nodes: Option<&[Uuid]>) -> Result<(), Error> {
    if !present(conn).await? {
        return Ok(());
    }
    sqlx::query(
        "UPDATE node_allocation SET generation=generation+1 WHERE node_id IN \
         (SELECT node_id FROM node_allocation WHERE $1::uuid[] IS NULL OR node_id=ANY($1) \
          ORDER BY node_id FOR UPDATE)",
    )
    .bind(nodes)
    .execute(&mut *conn)
    .await?;
    Ok(())
}
