//! Service machines (spec 0007 phase 1). A machine is the set of live nodes
//! sharing a machine key; slot 1 claims the key and mints its sibling slot
//! nodes. This crate owns the SQL for keys, desired state, handoff requests,
//! revocation and the owner's availability switch. It has no `AppState`.
use chrono::{DateTime, Utc};
use sqlx::{PgConnection, Row};
use std::collections::BTreeMap;
use uuid::Uuid;

pub use aq_protocol::{DesiredState, HandoffReason, MAX_MACHINE_SLOTS, valid_machine_key};

/// First `state_format_version` with these tables.
pub const FORMAT: u32 = 101;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not found")]
    NotFound,
    #[error("generation conflict")]
    Conflict { generation: u64 },
    #[error("machine key in use")]
    KeyInUse,
    #[error("invalid: {0}")]
    Invalid(&'static str),
    /// A typed refusal (409); the text starts with its code.
    #[error("refused: {0}")]
    Refused(&'static str),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineNode {
    pub node_id: Uuid,
    pub machine_key: Option<String>,
    pub slot: Option<u32>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Control {
    pub machine_key: String,
    pub desired: DesiredState,
    pub generation: u64,
    pub updated_at_ms: Option<u64>,
    pub operation_id: Option<Uuid>,
}
pub struct NewSlot<'a> {
    pub base_name: &'a str,
    pub worker_version: &'a str,
    pub protocol_version: u32,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MintedSlot {
    pub slot: u32,
    pub node_id: Uuid,
    pub created: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffRow {
    pub attempt_id: Uuid,
    pub node_id: Uuid,
    pub reason: HandoffReason,
    pub candidate_operation_id: Option<Uuid>,
    pub candidate_oid: Option<String>,
    pub note_comment_id: Option<Uuid>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeService {
    pub node_id: Uuid,
    pub machine_key: String,
    pub slot: u32,
    pub desired: DesiredState,
    pub generation: u64,
    pub since_ms: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    Unset,
    On,
    Off,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerAvailability {
    pub state: Availability,
    pub generation: u64,
    pub updated_at_ms: Option<u64>,
    pub operation_id: Option<Uuid>,
}

const RUNNING_ATTEMPTS: &str = "SELECT a.attempt_id, a.node_id FROM execution_attempts a \
    JOIN nodes n ON n.id = a.node_id \
    JOIN profiled_authority_hot h USING (attempt_id) \
    WHERE n.machine_key = $1 AND n.revoked_at IS NULL AND a.kind = 'profiled_v1' AND a.holds_task \
    AND h.state->'stop'->>'kind' IN ('unknown','running') \
    ORDER BY a.attempt_id LIMIT 64";
const PENDING_FILTER: &str = "FROM attempt_handoffs f \
    JOIN execution_attempts a ON a.attempt_id = f.attempt_id \
    JOIN nodes n ON n.id = a.node_id \
    JOIN profiled_authority_hot h ON h.attempt_id = f.attempt_id \
    WHERE n.revoked_at IS NULL AND a.holds_task \
    AND h.state->'stop'->>'kind' IN ('unknown','running')";

fn desired_str(d: DesiredState) -> &'static str {
    match d {
        DesiredState::Running => "running",
        DesiredState::Draining => "draining",
        DesiredState::Stopped => "stopped",
    }
}
fn desired_of(s: &str) -> Result<DesiredState, Error> {
    match s {
        "running" => Ok(DesiredState::Running),
        "draining" => Ok(DesiredState::Draining),
        "stopped" => Ok(DesiredState::Stopped),
        _ => Err(Error::Invalid("desired")),
    }
}
pub fn reason_str(r: HandoffReason) -> &'static str {
    match r {
        HandoffReason::OwnerStop => "owner_stop",
        HandoffReason::ServiceStop => "service_stop",
        HandoffReason::Restart => "restart",
    }
}
/// Spec 0007 S7: the deterministic ids and texts of a handoff successor base.
pub fn handoff_candidate_operation(predecessor: Uuid) -> Uuid {
    Uuid::new_v5(&predecessor, b"aq.swarm.handoff.candidate")
}
pub fn handoff_note_operation(predecessor: Uuid) -> Uuid {
    Uuid::new_v5(&predecessor, b"aq.swarm.handoff.note")
}
pub fn handoff_commit_message(task: Uuid, predecessor: Uuid, note: &str) -> String {
    format!("AQ: handoff of task {task} from attempt {predecessor}\n\n{note}")
}
pub fn handoff_note(predecessor: Uuid, reason: HandoffReason, conclusion: &str) -> String {
    format!(
        "Handoff from attempt {predecessor} ({}). {conclusion}",
        reason_str(reason)
    )
}
/// The publication fence: `base`/`handle` are the fresh export of this
/// predecessor's recorded handoff candidate.
pub fn is_handoff_export(row: &HandoffRow, base: &str, handle: Uuid) -> bool {
    let op = handoff_candidate_operation(row.attempt_id);
    row.candidate_operation_id == Some(op)
        && op == handle
        && row.candidate_oid.as_deref() == Some(base)
}
/// Review I1(c): at most this many owner/service handoffs per lineage are exempt.
pub const MAX_EXEMPT_HANDOFFS: usize = 5;
/// Keeps the first `MAX_EXEMPT_HANDOFFS` exempt flags (in attempt order); every
/// later handoff is charged as a normal attempt.
pub fn cap_exempt(flags: impl IntoIterator<Item = bool>) -> Vec<bool> {
    let mut left = MAX_EXEMPT_HANDOFFS;
    (flags.into_iter())
        .map(|e| {
            let keep = e && left > 0;
            left -= usize::from(keep);
            keep
        })
        .collect()
}
fn reason_of(s: &str) -> Result<HandoffReason, Error> {
    match s {
        "owner_stop" => Ok(HandoffReason::OwnerStop),
        "service_stop" => Ok(HandoffReason::ServiceStop),
        "restart" => Ok(HandoffReason::Restart),
        _ => Err(Error::Invalid("reason")),
    }
}
fn ms(v: i64) -> u64 {
    v.max(0) as u64
}

/// state_format_version >= FORMAT (the `lineage_quarantined` pattern).
pub async fn has_format(conn: &mut PgConnection) -> Result<bool, Error> {
    let v: String = sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
        .fetch_one(&mut *conn)
        .await?;
    Ok(v.parse::<u32>().map_err(|_| Error::Invalid("format"))? >= FORMAT)
}

/// `<base>#<slot>`, with the base truncated to 60 characters.
pub fn slot_name(base: &str, slot: u32) -> String {
    format!("{}#{slot}", base.chars().take(60).collect::<String>())
}

pub async fn machine_of(conn: &mut PgConnection, node: Uuid) -> Result<MachineNode, Error> {
    let row = sqlx::query("SELECT machine_key, slot FROM nodes WHERE id=$1")
        .bind(node)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or(Error::NotFound)?;
    Ok(MachineNode {
        node_id: node,
        machine_key: row.try_get("machine_key")?,
        slot: row.try_get::<Option<i32>, _>("slot")?.map(|s| s as u32),
    })
}

/// The caller has no key: sets (key, slot 1), unless another live node holds
/// (key, 1). A caller that already holds this key as slot 1 is unchanged.
pub async fn claim_key(tx: &mut PgConnection, node: Uuid, key: &str) -> Result<MachineNode, Error> {
    if !valid_machine_key(key) {
        return Err(Error::Invalid("machine_key"));
    }
    let me = machine_of(tx, node).await?;
    if let Some(held) = &me.machine_key {
        return if held == key && me.slot == Some(1) {
            Ok(me)
        } else {
            Err(Error::Invalid("not_slot_one"))
        };
    }
    let taken: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM nodes WHERE machine_key=$1 AND slot=1 AND revoked_at IS NULL AND id<>$2)",
    )
    .bind(key)
    .bind(node)
    .fetch_one(&mut *tx)
    .await?;
    if taken {
        return Err(Error::KeyInUse);
    }
    sqlx::query("UPDATE nodes SET machine_key=$2, slot=1 WHERE id=$1")
        .bind(node)
        .bind(key)
        .execute(&mut *tx)
        .await?;
    Ok(MachineNode {
        node_id: node,
        machine_key: Some(key.into()),
        slot: Some(1),
    })
}

/// The caller must hold a key as slot 1. Creates the missing live slots
/// 2..=wanted and never re-issues an existing slot. `hashes[i]` is the
/// credential verifier for slot i+2; only the slots it creates use theirs.
pub async fn mint_slots(
    tx: &mut PgConnection,
    caller: &MachineNode,
    wanted: u32,
    new: &NewSlot<'_>,
    hashes: &[String],
    now: DateTime<Utc>,
) -> Result<Vec<MintedSlot>, Error> {
    if !(1..=MAX_MACHINE_SLOTS).contains(&wanted) {
        return Err(Error::Invalid("slots_out_of_range"));
    }
    let (Some(key), Some(1)) = (&caller.machine_key, caller.slot) else {
        return Err(Error::Invalid("not_slot_one"));
    };
    if hashes.len() + 1 < wanted as usize {
        return Err(Error::Invalid("credential_hashes"));
    }
    let mut out = Vec::new();
    for slot in 2..=wanted {
        let existing: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM nodes WHERE machine_key=$1 AND slot=$2 AND revoked_at IS NULL",
        )
        .bind(key)
        .bind(slot as i32)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(node_id) = existing {
            out.push(MintedSlot {
                slot,
                node_id,
                created: false,
            });
            continue;
        }
        let node_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO nodes (id, name, credential_hash, worker_version, protocol_version, \
             enrolled_at, machine_key, slot) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(node_id)
        .bind(slot_name(new.base_name, slot))
        .bind(&hashes[slot as usize - 2])
        .bind(new.worker_version)
        .bind(new.protocol_version as i32)
        .bind(now)
        .bind(key)
        .bind(slot as i32)
        .execute(&mut *tx)
        .await?;
        out.push(MintedSlot {
            slot,
            node_id,
            created: true,
        });
    }
    Ok(out)
}

/// A live node holds the key.
pub async fn machine_exists(conn: &mut PgConnection, key: &str) -> Result<bool, Error> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM nodes WHERE machine_key=$1 AND revoked_at IS NULL)",
    )
    .bind(key)
    .fetch_one(&mut *conn)
    .await?)
}

pub async fn control(conn: &mut PgConnection, key: &str) -> Result<Control, Error> {
    let row = sqlx::query(
        "SELECT desired, generation, operation_id, \
         (extract(epoch FROM updated_at) * 1000)::bigint AS at_ms \
         FROM machine_controls WHERE machine_key=$1",
    )
    .bind(key)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(row) = row else {
        return Ok(Control {
            machine_key: key.into(),
            desired: DesiredState::Running,
            generation: 1,
            updated_at_ms: None,
            operation_id: None,
        });
    };
    Ok(Control {
        machine_key: key.into(),
        desired: desired_of(&row.try_get::<String, _>("desired")?)?,
        generation: ms(row.try_get("generation")?),
        updated_at_ms: Some(ms(row.try_get("at_ms")?)),
        operation_id: Some(row.try_get("operation_id")?),
    })
}

/// Compare-and-swap on the generation. Writes `desired`, generation+1 and the
/// operation id, also when `desired` is unchanged.
pub async fn set_desired(
    tx: &mut PgConnection,
    key: &str,
    desired: DesiredState,
    expected_generation: u64,
    actor: Uuid,
    operation_id: Uuid,
    now: DateTime<Utc>,
) -> Result<Control, Error> {
    let next = expected_generation as i64 + 1;
    // Review I2: touch the machine's live node rows first. A publication holds
    // its node row lock while it re-checks the gate, so it either commits
    // before this change (and its attempt is then handed off) or fails to
    // serialize after it.
    sqlx::query(
        "UPDATE nodes SET machine_key=machine_key WHERE machine_key=$1 AND revoked_at IS NULL",
    )
    .bind(key)
    .execute(&mut *tx)
    .await?;
    let done = if expected_generation == 1 {
        sqlx::query(
            "INSERT INTO machine_controls(machine_key,desired,generation,operation_id,updated_at,updated_by) \
             VALUES($1,$2,2,$3,$4,$5) ON CONFLICT DO NOTHING",
        )
        .bind(key)
        .bind(desired_str(desired))
        .bind(operation_id)
        .bind(now)
        .bind(actor)
        .execute(&mut *tx)
        .await?
        .rows_affected()
    } else {
        0
    };
    if done == 0 {
        let updated = sqlx::query(
            "UPDATE machine_controls SET desired=$2, generation=$3, operation_id=$4, updated_at=$5, updated_by=$6 \
             WHERE machine_key=$1 AND generation=$7",
        )
        .bind(key)
        .bind(desired_str(desired))
        .bind(next)
        .bind(operation_id)
        .bind(now)
        .bind(actor)
        .bind(expected_generation as i64)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if updated == 0 {
            let current = control(tx, key).await?;
            return Err(Error::Conflict {
                generation: current.generation,
            });
        }
    }
    control(tx, key).await
}

/// Inserts an `owner_stop` row for every profiled attempt on the machine's live
/// nodes that holds its task and whose hot stop kind is 'unknown' or 'running'.
/// Returns the attempt ids now pending.
pub async fn request_handoffs(
    tx: &mut PgConnection,
    key: &str,
    now: DateTime<Utc>,
) -> Result<Vec<Uuid>, Error> {
    let rows: Vec<(Uuid, Uuid)> = sqlx::query_as(RUNNING_ATTEMPTS)
        .bind(key)
        .fetch_all(&mut *tx)
        .await?;
    for (attempt, node) in &rows {
        sqlx::query(
            "INSERT INTO attempt_handoffs(attempt_id,node_id,reason,requested_at) \
             VALUES($1,$2,'owner_stop',$3) ON CONFLICT DO NOTHING",
        )
        .bind(attempt)
        .bind(node)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    }
    Ok(rows.into_iter().map(|(a, _)| a).collect())
}

/// Attempts with any handoff row that still hold their task and are running.
pub async fn pending_handoffs(conn: &mut PgConnection, key: &str) -> Result<Vec<Uuid>, Error> {
    Ok(sqlx::query_scalar(&format!(
        "SELECT f.attempt_id {PENDING_FILTER} AND n.machine_key=$1 ORDER BY f.attempt_id LIMIT 64"
    ))
    .bind(key)
    .fetch_all(&mut *conn)
    .await?)
}

/// Pending handoff counts per key, for the swarm read.
pub async fn pending_counts(
    conn: &mut PgConnection,
    keys: &[String],
) -> Result<BTreeMap<String, u32>, Error> {
    let rows: Vec<(String, i64)> = sqlx::query_as(&format!(
        "SELECT n.machine_key, count(*) {PENDING_FILTER} AND n.machine_key = ANY($1) GROUP BY n.machine_key"
    ))
    .bind(keys)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows.into_iter().map(|(k, n)| (k, n as u32)).collect())
}

/// Node-announced handoff (review I1). The attempt must belong to `node`. An
/// exempt reason (`owner_stop`, `service_stop`) is refused unless the attempt
/// still holds its task and runs. A node-posted `owner_stop` counts only while
/// the machine is desired stopped by a stop requested before the handoff (one
/// minute of slack); otherwise it is stored as `restart`. Returns the reason
/// on record (an earlier row, such as the owner's own Stop, wins).
pub async fn record_handoff(
    conn: &mut PgConnection,
    node: Uuid,
    attempt: Uuid,
    reason: HandoffReason,
    now: DateTime<Utc>,
) -> Result<HandoffReason, Error> {
    let row = sqlx::query(
        "SELECT a.node_id, COALESCE(a.holds_task AND h.state->'stop'->>'kind' IN ('unknown','running'), FALSE) AS running, \
         COALESCE(c.desired = 'stopped' AND c.updated_at <= $2 + interval '1 minute', FALSE) AS stopped \
         FROM execution_attempts a JOIN nodes n ON n.id = a.node_id \
         LEFT JOIN profiled_authority_hot h ON h.attempt_id = a.attempt_id \
         LEFT JOIN machine_controls c ON c.machine_key = n.machine_key WHERE a.attempt_id=$1",
    )
    .bind(attempt)
    .bind(now)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(row) = row.filter(|r| r.get::<Uuid, _>("node_id") == node) else {
        return Err(Error::Invalid("attempt_not_owned"));
    };
    if reason != HandoffReason::Restart && !row.try_get::<bool, _>("running")? {
        return Err(Error::Refused(
            "handoff_not_running: the attempt is no longer running",
        ));
    }
    let reason = match reason {
        HandoffReason::OwnerStop if !row.try_get::<bool, _>("stopped")? => HandoffReason::Restart,
        r => r,
    };
    sqlx::query(
        "INSERT INTO attempt_handoffs(attempt_id,node_id,reason,requested_at) \
         VALUES($1,$2,$3,$4) ON CONFLICT (attempt_id) DO NOTHING",
    )
    .bind(attempt)
    .bind(node)
    .bind(reason_str(reason))
    .bind(now)
    .execute(&mut *conn)
    .await?;
    let stored: String =
        sqlx::query_scalar("SELECT reason FROM attempt_handoffs WHERE attempt_id=$1")
            .bind(attempt)
            .fetch_one(&mut *conn)
            .await?;
    reason_of(&stored)
}

pub async fn handoff_of(
    conn: &mut PgConnection,
    attempt: Uuid,
) -> Result<Option<HandoffRow>, Error> {
    let Some(r) = sqlx::query(
        "SELECT attempt_id,node_id,reason,candidate_operation_id,candidate_oid,note_comment_id \
         FROM attempt_handoffs WHERE attempt_id=$1",
    )
    .bind(attempt)
    .fetch_optional(&mut *conn)
    .await?
    else {
        return Ok(None);
    };
    Ok(Some(HandoffRow {
        attempt_id: r.try_get("attempt_id")?,
        node_id: r.try_get("node_id")?,
        reason: reason_of(&r.try_get::<String, _>("reason")?)?,
        candidate_operation_id: r.try_get("candidate_operation_id")?,
        candidate_oid: r.try_get("candidate_oid")?,
        note_comment_id: r.try_get("note_comment_id")?,
    }))
}

/// Only fills NULL columns; equal values are a no-op.
pub async fn set_handoff_candidate(
    conn: &mut PgConnection,
    attempt: Uuid,
    operation: Uuid,
    oid: &str,
    note_comment: Option<Uuid>,
) -> Result<(), Error> {
    let n = sqlx::query(
        "UPDATE attempt_handoffs SET candidate_operation_id=COALESCE(candidate_operation_id,$2), \
         candidate_oid=COALESCE(candidate_oid,$3), note_comment_id=COALESCE(note_comment_id,$4) \
         WHERE attempt_id=$1",
    )
    .bind(attempt)
    .bind(operation)
    .bind(oid)
    .bind(note_comment)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if n == 0 { Err(Error::NotFound) } else { Ok(()) }
}

/// desired == Running for the node's key; true for a legacy node (no key).
pub async fn accepts_new_work(conn: &mut PgConnection, node: Uuid) -> Result<bool, Error> {
    let row = sqlx::query(
        "SELECT n.machine_key, c.desired FROM nodes n \
         LEFT JOIN machine_controls c ON c.machine_key = n.machine_key WHERE n.id=$1",
    )
    .bind(node)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(Error::NotFound)?;
    let desired: Option<String> = row.try_get("desired")?;
    Ok(desired.is_none_or(|d| d == "running"))
}

/// The offer gate of a polling node: why it gets no new offer, or None. Holds
/// when its machine is not running (S4) or the owner's availability switch is
/// off (spec 0007 Q2: no selection runs, so no decision row). None below 101.
pub async fn offer_hold(
    conn: &mut PgConnection,
    node: Uuid,
) -> Result<Option<&'static str>, Error> {
    if !has_format(conn).await? {
        return Ok(None);
    }
    if !accepts_new_work(conn, node).await? {
        return Ok(Some("machine is not running (draining or stopped)"));
    }
    let off = owner_availability(conn).await?.state == Availability::Off;
    Ok(off.then_some("owner availability is off"))
}

/// The service facts of the given nodes that hold a key (legacy nodes are absent).
pub async fn node_services(
    conn: &mut PgConnection,
    nodes: &[Uuid],
) -> Result<Vec<NodeService>, Error> {
    let rows = sqlx::query(
        "SELECT n.id, n.machine_key, n.slot, COALESCE(c.desired,'running') AS desired, \
         COALESCE(c.generation,1) AS generation, \
         (extract(epoch FROM COALESCE(c.updated_at, n.enrolled_at)) * 1000)::bigint AS since_ms \
         FROM nodes n LEFT JOIN machine_controls c ON c.machine_key = n.machine_key \
         WHERE n.id = ANY($1) AND n.machine_key IS NOT NULL",
    )
    .bind(nodes)
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter()
        .map(|r| {
            Ok(NodeService {
                node_id: r.try_get("id")?,
                machine_key: r.try_get("machine_key")?,
                slot: r.try_get::<i32, _>("slot")? as u32,
                desired: desired_of(&r.try_get::<String, _>("desired")?)?,
                generation: ms(r.try_get("generation")?),
                since_ms: ms(r.try_get("since_ms")?),
            })
        })
        .collect()
}

/// `machine` is a key or `node:<uuid>`. Sets revoked_at on every live node of
/// it and deletes its machine_controls row. Returns the number of nodes revoked.
pub async fn revoke_machine(
    tx: &mut PgConnection,
    machine: &str,
    now: DateTime<Utc>,
) -> Result<u32, Error> {
    if let Some(id) = machine.strip_prefix("node:") {
        let id: Uuid = id.parse().map_err(|_| Error::Invalid("machine"))?;
        let n = sqlx::query("UPDATE nodes SET revoked_at=$2 WHERE id=$1 AND revoked_at IS NULL")
            .bind(id)
            .bind(now)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        return Ok(n as u32);
    }
    if !valid_machine_key(machine) {
        return Err(Error::Invalid("machine"));
    }
    let n =
        sqlx::query("UPDATE nodes SET revoked_at=$2 WHERE machine_key=$1 AND revoked_at IS NULL")
            .bind(machine)
            .bind(now)
            .execute(&mut *tx)
            .await?
            .rows_affected();
    sqlx::query("DELETE FROM machine_controls WHERE machine_key=$1")
        .bind(machine)
        .execute(&mut *tx)
        .await?;
    Ok(n as u32)
}

pub async fn owner_availability(conn: &mut PgConnection) -> Result<OwnerAvailability, Error> {
    let row = sqlx::query(
        "SELECT available, generation, operation_id, \
         (extract(epoch FROM updated_at) * 1000)::bigint AS at_ms FROM owner_availability",
    )
    .fetch_optional(&mut *conn)
    .await?;
    let Some(r) = row else {
        return Ok(OwnerAvailability {
            state: Availability::Unset,
            generation: 1,
            updated_at_ms: None,
            operation_id: None,
        });
    };
    Ok(OwnerAvailability {
        state: if r.try_get("available")? {
            Availability::On
        } else {
            Availability::Off
        },
        generation: ms(r.try_get("generation")?),
        updated_at_ms: Some(ms(r.try_get("at_ms")?)),
        operation_id: Some(r.try_get("operation_id")?),
    })
}

/// Spec 0007 Q2: `(principal, generation)` of the owner's switch while it is
/// on; None when unset or off, and below 101. Selection synthesizes the
/// owner's attestation from it, so the admission fences change only when the
/// owner flips the switch (never from a refreshed observation row).
pub async fn owner_switch(conn: &mut PgConnection) -> Result<Option<(Uuid, u64)>, Error> {
    if !has_format(conn).await? {
        return Ok(None);
    }
    let on: Option<(Uuid, i64)> = sqlx::query_as(
        "SELECT updated_by, generation FROM owner_availability WHERE available FOR SHARE",
    )
    .fetch_optional(&mut *conn)
    .await?;
    Ok(on.map(|(by, g)| (by, ms(g))))
}

/// Compare-and-swap on the singleton, as `set_desired`.
pub async fn set_owner_availability(
    tx: &mut PgConnection,
    available: bool,
    expected_generation: u64,
    actor: Uuid,
    operation_id: Uuid,
    now: DateTime<Utc>,
) -> Result<OwnerAvailability, Error> {
    let done = if expected_generation == 1 {
        sqlx::query(
            "INSERT INTO owner_availability(available,generation,operation_id,updated_at,updated_by) \
             VALUES($1,2,$2,$3,$4) ON CONFLICT DO NOTHING",
        )
        .bind(available)
        .bind(operation_id)
        .bind(now)
        .bind(actor)
        .execute(&mut *tx)
        .await?
        .rows_affected()
    } else {
        0
    };
    if done == 0 {
        let updated = sqlx::query(
            "UPDATE owner_availability SET available=$1, generation=$2, operation_id=$3, updated_at=$4, updated_by=$5 \
             WHERE generation=$6",
        )
        .bind(available)
        .bind(expected_generation as i64 + 1)
        .bind(operation_id)
        .bind(now)
        .bind(actor)
        .bind(expected_generation as i64)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if updated == 0 {
            let current = owner_availability(tx).await?;
            return Err(Error::Conflict {
                generation: current.generation,
            });
        }
    }
    owner_availability(tx).await
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_the_first_five_exempt_handoffs_stay_exempt() {
        let flags = [true, false, true, true, true, true, true, true];
        let capped = super::cap_exempt(flags);
        assert_eq!(capped, [true, false, true, true, true, true, false, false]);
        assert_eq!(super::cap_exempt([false; 3]), [false; 3]);
    }
}
