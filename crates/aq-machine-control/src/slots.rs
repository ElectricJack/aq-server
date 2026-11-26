//! One count and durable allocation per node. Callers hold management then node locks.
use crate::Error;
use serde::Serialize;
use sqlx::{PgConnection, Row};
use uuid::Uuid;

pub async fn available(conn: &mut PgConnection) -> Result<bool, Error> {
    let version: String =
        sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
            .fetch_one(conn)
            .await?;
    Ok(version.parse::<u32>().unwrap_or(0) >= 135)
}

pub async fn count(conn: &mut PgConnection, node: Uuid) -> Result<u32, Error> {
    if !available(conn).await? {
        return Ok(1);
    }
    let count: i32 = sqlx::query_scalar("SELECT COALESCE((SELECT max_agents FROM worker_settings WHERE kind='node' AND id=$1),slots) FROM node_slot_default")
        .bind(node.to_string()).fetch_one(conn).await?;
    Ok(count as u32)
}

#[derive(Debug, Clone, Serialize)]
pub struct SlotView {
    pub total: u32,
    pub occupied: u32,
    pub idle: u32,
    #[serde(skip)]
    pub blocked: Vec<u32>,
    #[serde(skip)]
    pub all_blocked: bool,
}
impl SlotView {
    pub fn free(&self, slot: u32) -> bool {
        slot > 0
            && slot <= self.total
            && self.idle > 0
            && !self.all_blocked
            && !self.blocked.contains(&slot)
    }
    pub fn first_free(&self) -> Option<u32> {
        (1..=self.total).find(|slot| self.free(*slot))
    }
}

pub async fn view(conn: &mut PgConnection, node: Uuid) -> Result<SlotView, Error> {
    let total = count(conn, node).await?;
    if !available(conn).await? {
        let held: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM execution_attempts WHERE node_id=$1 AND holds_task)",
        )
        .bind(node)
        .fetch_one(conn)
        .await?;
        return Ok(SlotView {
            total,
            occupied: u32::from(held),
            idle: u32::from(!held),
            blocked: vec![],
            all_blocked: held,
        });
    }
    let rows = sqlx::query("SELECT s.slot, a.kind<>'profiled_v1' AS legacy FROM execution_attempts a LEFT JOIN execution_attempt_slots s USING(attempt_id) WHERE a.node_id=$1 AND a.holds_task AND native_attempt_holds_compute(a.attempt_id,$1) UNION ALL SELECT s.slot,FALSE FROM manual_agent_sessions m LEFT JOIN manual_agent_slots s ON s.session_id=m.id WHERE m.node_id=$1 AND m.stopped_at_ms IS NULL AND m.status<>'pending'")
        .bind(node).fetch_all(conn).await?;
    let occupied = rows.len() as u32;
    let mut all_blocked = false;
    let mut blocked = Vec::new();
    for row in rows {
        if row.get::<bool, _>("legacy") {
            all_blocked = true;
        }
        match row.get::<Option<i32>, _>("slot") {
            Some(slot) => blocked.push(slot as u32),
            None => all_blocked = true,
        }
    }
    Ok(SlotView {
        total,
        occupied,
        idle: if all_blocked {
            0
        } else {
            total.saturating_sub(occupied)
        },
        blocked,
        all_blocked,
    })
}

pub async fn poll_free(conn: &mut PgConnection, node: Uuid, slot: u32) -> Result<bool, Error> {
    Ok(view(conn, node).await?.free(slot) && registered(conn, node, slot).await?)
}
/// Legacy registrations remain one-slot even if the owner has raised Settings.
pub async fn registered(conn: &mut PgConnection, node: Uuid, slot: u32) -> Result<bool, Error> {
    let registered: Option<i32> = sqlx::query_scalar(
        "SELECT COALESCE((state->>'slots')::INTEGER,1) FROM execution_nodes WHERE node_id=$1",
    )
    .bind(node)
    .fetch_optional(conn)
    .await?;
    Ok(slot <= registered.unwrap_or(1) as u32)
}

pub async fn assign(
    conn: &mut PgConnection,
    attempt: Uuid,
    node: Uuid,
    slot: u32,
) -> Result<(), Error> {
    if available(conn).await? {
        sqlx::query(
            "INSERT INTO execution_attempt_slots(attempt_id,node_id,slot) VALUES($1,$2,$3)",
        )
        .bind(attempt)
        .bind(node)
        .bind(slot as i32)
        .execute(conn)
        .await?;
    }
    Ok(())
}

pub async fn assign_manual(
    conn: &mut PgConnection,
    session: Uuid,
    node: Uuid,
) -> Result<Option<u32>, Error> {
    if !available(conn).await? {
        return Ok(Some(1));
    }
    let Some(slot) = view(conn, node).await?.first_free() else {
        return Ok(None);
    };
    sqlx::query("INSERT INTO manual_agent_slots(session_id,node_id,slot) VALUES($1,$2,$3)")
        .bind(session)
        .bind(node)
        .bind(slot as i32)
        .execute(conn)
        .await?;
    Ok(Some(slot))
}

/// A slot-aware boot may leave only mapped native attempts held by their own slots.
pub async fn boot_can_register(conn: &mut PgConnection, node: Uuid) -> Result<bool, Error> {
    if !available(conn).await? {
        return Ok(false);
    }
    Ok(!sqlx::query_scalar::<_,bool>("SELECT EXISTS(SELECT 1 FROM execution_attempts a LEFT JOIN execution_attempt_slots s USING(attempt_id) WHERE a.node_id=$1 AND a.holds_task AND native_attempt_holds_compute(a.attempt_id,$1) AND (a.kind<>'profiled_v1' OR s.node_id IS DISTINCT FROM $1 OR s.slot IS NULL))")
        .bind(node).fetch_one(conn).await?)
}

pub async fn aware(conn: &mut PgConnection, node: Uuid) -> Result<bool, Error> {
    Ok(sqlx::query_scalar::<_,bool>("SELECT EXISTS(SELECT 1 FROM execution_nodes WHERE node_id=$1 AND jsonb_typeof(state->'slots')='number')").bind(node).fetch_one(conn).await?)
}

/// Startup verifies slot owners and fences, alongside the existing exact compute guard.
/// Format 185 moved the slot guard to `node_allocation` and stores compute releases.
pub async fn validate(conn: &mut PgConnection, supported: u32) -> Result<(), Error> {
    let present: bool=sqlx::query_scalar("SELECT to_regclass('node_slot_default') IS NOT NULL AND to_regclass('execution_attempt_slots') IS NOT NULL AND to_regclass('manual_agent_slots') IS NOT NULL AND (SELECT count(*) FROM pg_trigger WHERE NOT tgisinternal AND tgenabled='O' AND tgname IN ('execution_attempt_slot_guard','manual_agent_slot_guard','initialize_node_slots'))=3 AND to_regprocedure('native_attempt_holds_compute(uuid,uuid)') IS NOT NULL AND to_regprocedure('native_compute_reuse_proven(uuid,uuid)') IS NOT NULL AND (NOT $1 OR (to_regclass('node_allocation') IS NOT NULL AND to_regclass('execution_attempt_compute') IS NOT NULL AND (SELECT count(*) FROM pg_trigger WHERE NOT tgisinternal AND tgenabled='O' AND tgfoid=to_regprocedure('execution_attempt_compute_refresh()') AND tgname IN ('execution_attempt_compute_insert','execution_attempt_compute_refresh'))=10))")
        .bind(supported >= 185).fetch_one(&mut *conn).await?;
    if !present {
        return Err(Error::Invalid("node slot schema missing"));
    }
    let slots = include_str!("../../aq-server/migrations/0135_node_slots.sql");
    let allocation =
        include_str!("../../aq-server/migrations/0185_node_allocation_and_compute_release.sql");
    let current = if supported >= 185 { allocation } else { slots };
    let mut functions = vec![
        ("initialize_node_slots", current),
        ("native_attempt_holds_compute", slots),
        ("native_compute_reuse_proven", slots),
        ("node_slot_assignment_guard", current),
    ];
    if supported >= 185 {
        functions.push(("execution_attempt_compute_refresh", allocation));
    }
    for (name, source) in functions {
        let expected = source
            .split_once(&format!("FUNCTION {name}("))
            .and_then(|(_, tail)| tail.split_once("$$"))
            .and_then(|(_, body)| body.split_once("$$;"))
            .map(|(body, _)| body);
        let body: Option<String>=sqlx::query_scalar("SELECT prosrc FROM pg_proc WHERE proname=$1 AND pronamespace='public'::regnamespace AND NOT prosecdef AND proconfig IS NULL")
            .bind(name).fetch_optional(&mut *conn).await?;
        if expected.is_none() || body.as_deref() != expected {
            return Err(Error::Invalid("node slot guard drift"));
        }
    }
    Ok(())
}

/// Parse a single optional canonical slot header without coupling admission to HTTP.
pub fn header<'a>(mut values: impl Iterator<Item = Option<&'a str>>) -> Result<Option<u32>, Error> {
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(Error::Invalid("duplicate node slot header"));
    }
    let value = value.ok_or(Error::Invalid("invalid node slot header"))?;
    if !canonical_slot(value) {
        return Err(Error::Invalid("invalid node slot header"));
    }
    let slot = value
        .parse::<u32>()
        .map_err(|_| Error::Invalid("invalid node slot header"))?;
    if slot > 1024 {
        return Err(Error::Invalid("invalid node slot header"));
    }
    Ok(Some(slot))
}

fn canonical_slot(value: &str) -> bool {
    (1..=4).contains(&value.len())
        && !value.starts_with('0')
        && value.bytes().all(|v| v.is_ascii_digit())
}

/// Redelivery preserves an existing reservation even above a lowered count, but
/// retains owner stop/drain, retirement and login-installation holds.
pub async fn redelivery_allowed(conn: &mut PgConnection, node: Uuid) -> Result<bool, Error> {
    if !crate::has_format(conn).await? {
        return Ok(true);
    }
    let capacity = crate::management::capacity(conn, node).await?;
    Ok(!capacity.removed
        && !capacity.login_pending
        && crate::accepts_new_work(conn, node).await?
        && crate::owner_availability(conn).await?.state != crate::Availability::Off)
}

/// The oldest offered attempt of `slot` after offer `after`, with its attempt
/// and authority rows locked: the offer a poll of that slot redelivers.
pub async fn offered_attempt(
    conn: &mut PgConnection,
    node: Uuid,
    after: Option<Uuid>,
    slot: u32,
) -> Result<Option<Uuid>, Error> {
    let query = "SELECT a.attempt_id FROM execution_attempts a JOIN profiled_authority_hot h USING(attempt_id) LEFT JOIN execution_attempt_slots s USING(attempt_id) WHERE a.node_id=$1 AND a.kind='profiled_v1' AND a.holds_task AND COALESCE(s.slot,1)=$3 AND h.state->>'status'='offered' AND ($2::UUID IS NULL OR a.offer_id>$2) ORDER BY a.offer_id LIMIT 1 FOR UPDATE OF a,h";
    let query = if available(conn).await? {
        query.to_owned()
    } else {
        query
            .replace("LEFT JOIN execution_attempt_slots s USING(attempt_id)", "")
            .replace("COALESCE(s.slot,1)", "1")
    };
    let attempt = sqlx::query_scalar(&query).bind(node).bind(after);
    Ok(attempt.bind(slot as i32).fetch_optional(conn).await?)
}

/// Batched authoritative counts for slot-aware placement peers.
pub async fn aware_counts(
    conn: &mut PgConnection,
    nodes: &[Uuid],
) -> Result<std::collections::BTreeMap<Uuid, u32>, Error> {
    if !available(conn).await? {
        return Ok(Default::default());
    }
    let rows: Vec<(Uuid,i32)>=sqlx::query_as("SELECT n.node_id,COALESCE(s.max_agents,d.slots) FROM execution_nodes n CROSS JOIN node_slot_default d LEFT JOIN worker_settings s ON s.kind='node' AND s.id=n.node_id::text WHERE n.node_id=ANY($1) AND jsonb_typeof(n.state->'slots')='number'")
        .bind(nodes).fetch_all(conn).await?;
    Ok(rows
        .into_iter()
        .map(|(id, count)| (id, count as u32))
        .collect())
}

pub async fn active(
    conn: &mut PgConnection,
    nodes: &[Uuid],
    exclude_attempt: Option<Uuid>,
) -> Result<Vec<(Uuid, i64)>, Error> {
    let query = if available(conn).await? {
        "SELECT node_id,count(*) FROM (SELECT node_id,attempt_id FROM execution_attempts WHERE holds_task AND native_attempt_holds_compute(attempt_id,node_id) UNION ALL SELECT node_id,id FROM manual_agent_sessions WHERE stopped_at_ms IS NULL AND status<>'pending') a WHERE node_id=ANY($1) AND attempt_id IS DISTINCT FROM $2 GROUP BY node_id"
    } else {
        "SELECT node_id,count(*) FROM execution_attempts WHERE holds_task AND node_id=ANY($1) AND attempt_id IS DISTINCT FROM $2 GROUP BY node_id"
    };
    Ok(sqlx::query_as(query)
        .bind(nodes)
        .bind(exclude_attempt)
        .fetch_all(conn)
        .await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn header_rejects_duplicates_and_noncanonical_values() {
        assert!(matches!(
            header([Some("2"), Some("2")].into_iter()),
            Err(Error::Invalid(_))
        ));
        assert!(header([None].into_iter()).is_err());
        for value in ["0", "01", "1025", "+1", " 1"] {
            assert!(header([Some(value)].into_iter()).is_err());
        }
        assert_eq!(header([Some("1024")].into_iter()).unwrap(), Some(1024));
        assert_eq!(header(std::iter::empty()).unwrap(), None);
    }
    #[test]
    fn occupancy_above_a_lowered_limit_blocks_even_an_empty_lower_index() {
        let view = SlotView {
            total: 1,
            occupied: 1,
            idle: 0,
            blocked: vec![2],
            all_blocked: false,
        };
        assert!(!view.free(1));
        assert!(view.first_free().is_none());
    }
}
