//! Service machines (spec 0007 phase 1). A machine is the set of live nodes
//! sharing a machine key; slot 1 claims the key and mints its sibling slot
//! nodes. This crate owns the SQL for keys, desired state, handoff requests,
//! revocation and the owner's availability switch. It has no `AppState`.
use chrono::{DateTime, Utc};
use sqlx::{PgConnection, Row};
use std::collections::BTreeMap;
use uuid::Uuid;

pub use aq_protocol::{DesiredState, HandoffReason, MAX_MACHINE_SLOTS, valid_machine_key};
pub mod lost;

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
    /// Review I4: the node is a slot of this machine; revoke the machine.
    #[error("{MACHINE_NODE}: worker.revoke {0}")]
    MachineNode(String),
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
    /// The effective state: `Unset` when the user who set it is no longer an
    /// owner (fix round 3), whatever the row says.
    pub state: Availability,
    pub generation: u64,
    pub updated_at_ms: Option<u64>,
    pub operation_id: Option<Uuid>,
    /// The row was set by a user who is no longer an owner.
    pub set_by_former_owner: bool,
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
/// The node reports `note`; it is marked as worker-reported, never AQ's text.
pub fn handoff_commit_message(task: Uuid, predecessor: Uuid, note: &str) -> String {
    format!("AQ: handoff of task {task} from attempt {predecessor}\n\nworker-reported: {note}")
}
/// The task comment of a handoff; `conclusion` is quoted as worker text.
pub fn handoff_note(predecessor: Uuid, reason: HandoffReason, conclusion: &str) -> String {
    format!(
        "Handoff from attempt {predecessor} ({}). Worker-reported conclusion: {conclusion}",
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

/// Review I3: the per-machine-key transaction lock that serializes claiming,
/// minting and revocation. A caller that also locks node rows takes it first.
pub async fn lock_machine(tx: &mut PgConnection, key: &str) -> Result<(), Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('aq-machine:' || $1, 0))")
        .bind(key)
        .execute(&mut *tx)
        .await?;
    Ok(())
}

/// The caller has no key: sets (key, slot 1), unless another live node holds
/// (key, 1). A caller that already holds this key as slot 1 is unchanged.
pub async fn claim_key(tx: &mut PgConnection, node: Uuid, key: &str) -> Result<MachineNode, Error> {
    if !valid_machine_key(key) {
        return Err(Error::Invalid("machine_key"));
    }
    lock_machine(tx, key).await?;
    let me = machine_of(tx, node).await?;
    if let Some(held) = &me.machine_key {
        return if held == key && me.slot == Some(1) {
            Ok(me)
        } else {
            Err(Error::Invalid("not_slot_one"))
        };
    }
    // Review I4: any live node of the key (not only slot 1) keeps it in use.
    let taken: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM nodes WHERE machine_key=$1 AND revoked_at IS NULL AND id<>$2)",
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
        .await
        .map_err(|e| match e.as_database_error().and_then(|d| d.code()) {
            Some(code) if code == "23505" => Error::KeyInUse,
            _ => Error::Db(e),
        })?;
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
    lock_machine(tx, key).await?;
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

/// Live acceptance D5: the project, repository and binding generation of
/// `attempt`'s own source export, which its handoff candidate must share.
pub async fn export_scope(
    conn: &mut PgConnection,
    attempt: Uuid,
) -> Result<Option<(Uuid, Uuid, u64)>, Error> {
    let row = sqlx::query(
        "SELECT e.project_id, (q.v->>'repository_id')::uuid AS repository, \
         (q.v->>'binding_generation')::bigint AS generation \
         FROM profiled_launch_source_exports x JOIN profiled_source_exports e USING(operation_id), \
         LATERAL (SELECT convert_from(e.request_bytes,'UTF8')::jsonb AS v) q \
         WHERE x.attempt_id=$1 LIMIT 1",
    )
    .bind(attempt)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(row) = row else { return Ok(None) };
    let generation = u64::try_from(row.try_get::<i64, _>("generation")?)
        .map_err(|_| Error::Invalid("source export scope"))?;
    Ok(Some((
        row.try_get("project_id")?,
        row.try_get("repository")?,
        generation,
    )))
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

/// Review I6: what "enrolled" means for `allow_enrolled_nodes`: a live node
/// that redeemed an enrollment code (only owners issue codes), or a live slot
/// minted by a machine whose live slot 1 did. Other rows of `nodes` are not.
pub async fn enrolled(conn: &mut PgConnection, node: Uuid) -> Result<bool, Error> {
    let sibling = if has_format(conn).await? {
        " OR (n.slot > 1 AND p.machine_key = n.machine_key AND p.slot = 1 AND p.revoked_at IS NULL)"
    } else {
        ""
    };
    Ok(sqlx::query_scalar(&format!(
        "SELECT EXISTS(SELECT 1 FROM nodes n JOIN nodes p ON p.id = n.id{sibling} \
         JOIN enrollment_codes e ON e.node_id = p.id WHERE n.id = $1 AND n.revoked_at IS NULL)"
    ))
    .bind(node)
    .fetch_one(&mut *conn)
    .await?)
}

/// Review I6: logs (and returns) one warning per project binding whose explicit
/// node or group list the default-on `allow_enrolled_nodes` overrides. Called
/// once at server startup.
pub async fn warn_widened_bindings(pool: sqlx::PgPool) -> Vec<String> {
    let rows: Result<Vec<Uuid>, _> = sqlx::query_scalar(
        "SELECT project_id FROM project_execution_bindings \
         WHERE COALESCE((binding->>'allow_enrolled_nodes')::boolean, TRUE) \
         AND (jsonb_array_length(COALESCE(binding->'allowed_node_ids','[]')) > 0 \
           OR jsonb_array_length(COALESCE(binding->'allowed_worker_groups','[]')) > 0) \
         ORDER BY project_id LIMIT 64",
    )
    .fetch_all(&pool)
    .await;
    let lines: Vec<String> = (rows.unwrap_or_default().into_iter())
        .map(|p| format!("project binding {p} lists allowed_node_ids or allowed_worker_groups, but allow_enrolled_nodes is on (the default since spec 0007 Q3), so every enrolled node is eligible; set allow_enrolled_nodes = false in the binding to keep only the listed nodes"))
        .collect();
    for line in &lines {
        tracing::warn!("{line}");
    }
    lines
}

/// Review I7: a node renews qualifications at most once per this interval.
pub const RENEWAL_INTERVAL_MS: u64 = 600_000;
/// Review I7, under a transaction lock: why a renewal by `node` of the
/// configured facts `bases` is refused at `now_ms` (renewals take effect
/// `delay_ms` later): a renewal of one of them is still pending (not yet
/// effective), or the node renewed within `RENEWAL_INTERVAL_MS`.
pub async fn renewal_hold(
    tx: &mut PgConnection,
    node: Uuid,
    bases: &[&str],
    now_ms: u64,
    delay_ms: u64,
) -> Result<Option<&'static str>, Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('aq-context-renewal', 0))")
        .execute(&mut *tx)
        .await?;
    let now = i64::try_from(now_ms).map_err(|_| Error::Invalid("now"))?;
    let since = now + i64::try_from(delay_ms).map_err(|_| Error::Invalid("delay"))?
        - RENEWAL_INTERVAL_MS as i64;
    let (pending, recent): (bool, bool) = sqlx::query_as(
        "SELECT EXISTS(SELECT 1 FROM context_qualification_renewals WHERE base_renderer_digest = ANY($1) \
           AND observed_at_ms > $2 AND expires_at_ms > $2), \
         EXISTS(SELECT 1 FROM context_qualification_renewals WHERE node_id = $3 AND observed_at_ms > $4)",
    )
    .bind(bases)
    .bind(now)
    .bind(node)
    .bind(since)
    .fetch_one(&mut *tx)
    .await?;
    Ok(if pending {
        Some("renewal_pending: a renewal of this qualification is not yet effective")
    } else if recent {
        Some("renewal_too_soon: this node renewed a qualification less than 10 minutes ago")
    } else {
        None
    })
}

/// The machine key of a live node, if it holds one.
pub async fn live_machine_key(
    conn: &mut PgConnection,
    node: Uuid,
) -> Result<Option<String>, Error> {
    Ok(sqlx::query_scalar(
        "SELECT machine_key FROM nodes WHERE id=$1 AND revoked_at IS NULL AND machine_key IS NOT NULL",
    )
    .bind(node)
    .fetch_optional(&mut *conn)
    .await?)
}

/// Review I4: revoking one node of a machine is refused; revoke the machine.
pub const MACHINE_NODE: &str =
    "machine_node: this node belongs to a service machine; revoke the whole machine by its key";

/// `machine` is a key or `node:<uuid>`. Sets revoked_at on every live node of
/// it and deletes its machine_controls row. Returns the number of nodes revoked.
/// A `node:` that holds a machine key is refused (`MACHINE_NODE`).
pub async fn revoke_machine(
    tx: &mut PgConnection,
    machine: &str,
    now: DateTime<Utc>,
) -> Result<u32, Error> {
    if let Some(id) = machine.strip_prefix("node:") {
        let id: Uuid = id.parse().map_err(|_| Error::Invalid("machine"))?;
        // Fix round 2: under the node row lock, so a racing slot claim (which
        // locks this row first) is seen or waits for the revocation.
        if has_format(tx).await? {
            let key: Option<Option<String>> = sqlx::query_scalar(
                "SELECT machine_key FROM nodes WHERE id=$1 AND revoked_at IS NULL FOR UPDATE",
            )
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
            if let Some(Some(key)) = key {
                return Err(Error::MachineNode(key));
            }
        }
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
    lock_machine(tx, machine).await?;
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
        "SELECT o.available, o.generation, o.operation_id, COALESCE(u.role = 'owner', FALSE) AS by_owner, \
         (extract(epoch FROM o.updated_at) * 1000)::bigint AS at_ms \
         FROM owner_availability o LEFT JOIN users u ON u.id = o.updated_by",
    )
    .fetch_optional(&mut *conn)
    .await?;
    let Some(r) = row else {
        return Ok(OwnerAvailability {
            state: Availability::Unset,
            generation: 1,
            updated_at_ms: None,
            operation_id: None,
            set_by_former_owner: false,
        });
    };
    let by_owner: bool = r.try_get("by_owner")?;
    Ok(OwnerAvailability {
        state: match (by_owner, r.try_get("available")?) {
            (false, _) => Availability::Unset,
            (true, true) => Availability::On,
            (true, false) => Availability::Off,
        },
        generation: ms(r.try_get("generation")?),
        updated_at_ms: Some(ms(r.try_get("at_ms")?)),
        operation_id: Some(r.try_get("operation_id")?),
        set_by_former_owner: !by_owner,
    })
}

/// Spec 0007 Q2: the owner's attestation of `credentials` at `now_ms` while the
/// switch is on (`aq_intelligence_config::owner_attestations`), else none.
/// Its generation is the only fact a fence compares, so a flip refuses.
pub async fn owner_attested(
    conn: &mut PgConnection,
    credentials: &[aq_selection::AuthorizedCredential],
    observed: &[aq_selection::AvailabilityObservation],
    revision: u64,
    now_ms: u64,
) -> Result<Vec<aq_selection::AvailabilityObservation>, Error> {
    Ok(match owner_switch(conn).await? {
        Some(on) => {
            aq_intelligence_config::owner_attestations(credentials, observed, on, revision, now_ms)
        }
        None => vec![],
    })
}

/// Fix round 2 (review I2): the publication's first lock, on its node row. A
/// serialization failure here means a machine change (Stop, Drain) or an
/// availability change committed after the transaction began, and a lock
/// timeout means one is still open: `Ok(false)`, which publishes no offer.
pub async fn lock_publishing_node(tx: &mut PgConnection, node: Uuid) -> Result<bool, Error> {
    let locked = sqlx::query("SELECT 1 FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(node)
        .execute(&mut *tx)
        .await;
    match locked {
        Ok(_) => Ok(true),
        Err(e) if is_serialization_failure(&e) || is_lock_timeout(&e) => Ok(false),
        Err(e) => Err(e.into()),
    }
}
/// SQLSTATE 55P03: the publication's `lock_timeout` passed waiting for the row.
pub fn is_lock_timeout(e: &sqlx::Error) -> bool {
    e.as_database_error()
        .and_then(|d| d.code())
        .is_some_and(|c| c == "55P03")
}
/// SQLSTATE 40001 (serialization failure) or 40P01 (deadlock).
pub fn is_serialization_failure(e: &sqlx::Error) -> bool {
    e.as_database_error()
        .and_then(|d| d.code())
        .is_some_and(|c| c == "40001" || c == "40P01")
}

/// Spec 0007 Q2: `(principal, generation)` of the owner's switch while it is
/// on; None when unset or off, below 101, or (fix round 2) when the user who
/// switched it on is no longer an owner: that switch counts as unset.
/// Selection synthesizes the owner's attestation from it, so the admission
/// fences change only when the owner flips the switch.
pub async fn owner_switch(conn: &mut PgConnection) -> Result<Option<(Uuid, u64)>, Error> {
    if !has_format(conn).await? {
        return Ok(None);
    }
    let on: Option<(Uuid, i64)> = sqlx::query_as(
        "SELECT o.updated_by, o.generation FROM owner_availability o \
         JOIN users u ON u.id = o.updated_by AND u.role = 'owner' WHERE o.available FOR SHARE OF o",
    )
    .fetch_optional(&mut *conn)
    .await?;
    Ok(on.map(|(by, g)| (by, ms(g))))
}

/// Compare-and-swap on the singleton, as `set_desired`. Setting the state it
/// already has (on to on, off to off) at the current generation changes
/// nothing and returns the current generation (fix round 2).
pub async fn set_owner_availability(
    tx: &mut PgConnection,
    available: bool,
    expected_generation: u64,
    actor: Uuid,
    operation_id: Uuid,
    now: DateTime<Utc>,
) -> Result<OwnerAvailability, Error> {
    // Fix round 3 (I2 race): touch the live node rows first, as set_desired
    // does, so a publication racing this change waits or fails to serialize.
    sqlx::query("UPDATE nodes SET machine_key=machine_key WHERE revoked_at IS NULL")
        .execute(&mut *tx)
        .await?;
    // The effective state: a former owner's switch is unset, so setting it
    // again writes a new row under the current owner.
    let current = owner_availability(tx).await?;
    let same = matches!(
        (current.state, available),
        (Availability::On, true) | (Availability::Off, false)
    );
    if same && current.generation == expected_generation {
        return Ok(current);
    }
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
