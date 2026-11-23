//! Service machines (spec 0007 phase 1). A machine is the set of live nodes
//! sharing a machine key; slot 1 claims the key and mints its sibling slot
//! nodes. This crate owns the SQL for keys, desired state, handoff requests,
//! revocation and the owner's availability switch. It has no `AppState`.
use chrono::{DateTime, Utc};
use sqlx::{PgConnection, Row};
use std::collections::BTreeMap;
use uuid::Uuid;

pub use aq_intelligence_config::AttestationTime;
pub use aq_protocol::{DesiredState, HandoffReason, MAX_MACHINE_SLOTS, valid_machine_key};
pub mod attempts;
pub mod fresh;
mod handoffs;
pub mod holds;
pub use handoffs::{
    HandoffCandidate, MAX_LINEAGE_HOPS, export_scope, handoff_of, lineage_base, pending_counts,
    pending_handoffs, record_handoff, request_handoffs, set_handoff_candidate,
};
mod owner_availability;
pub use owner_availability::{
    owner_attested, owner_availability, owner_switch, set_owner_availability,
};
pub mod lapse;
pub mod launch;
pub mod limits;
pub mod lost;
pub mod management;
pub mod offer_assembly;
pub mod owner_retry;
pub mod recent;
pub mod slots;

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
/// Identity and revision fence for an owner change to machine controls.
#[derive(Clone, Copy)]
pub struct OwnerChange {
    pub expected_generation: u64,
    pub actor: Uuid,
    pub operation_id: Uuid,
    pub now: DateTime<Utc>,
}
/// Nodes and per-slot credential verifiers requested in one issuance.
pub struct SlotRequest<'a> {
    pub wanted: u32,
    pub template: &'a NewSlot<'a>,
    pub hashes: &'a [String],
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
/// Review I1(c): keeps the first `max` exempt flags (in attempt order); every later handoff is
/// charged as a normal attempt. `max` is Settings › Selection defaults
/// `max_uncharged_continuations`.
pub fn cap_exempt(flags: impl IntoIterator<Item = bool>, max: u32) -> Vec<bool> {
    let mut left = max;
    (flags.into_iter())
        .map(|e| {
            let keep = e && left > 0;
            left -= u32::from(keep);
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
/// N1/N2: an owner action that locks node rows waits at most this long for each.
/// It bounds how long the action can hold the rows it already has.
pub const OWNER_LOCK_TIMEOUT: &str = "1s";
async fn bound(tx: &mut PgConnection) -> Result<(), Error> {
    sqlx::query("SELECT set_config('lock_timeout',$1,true)")
        .bind(OWNER_LOCK_TIMEOUT)
        .execute(&mut *tx)
        .await?;
    Ok(())
}
/// N1: locks the live node rows (all of them, or one machine's) in id order, so
/// two owner actions that lock several nodes wait for each other instead of
/// deadlocking (40P01). Callers then update the rows they hold.
pub async fn lock_live_nodes(tx: &mut PgConnection, key: Option<&str>) -> Result<(), Error> {
    sqlx::query(
        "SELECT id FROM nodes WHERE revoked_at IS NULL AND ($1::text IS NULL OR machine_key=$1) \
         ORDER BY id FOR UPDATE",
    )
    .bind(key)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

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
    request: SlotRequest<'_>,
    now: DateTime<Utc>,
) -> Result<Vec<MintedSlot>, Error> {
    let SlotRequest {
        wanted,
        template: new,
        hashes,
    } = request;
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
    change: OwnerChange,
) -> Result<Control, Error> {
    let OwnerChange {
        expected_generation,
        actor,
        operation_id,
        now,
    } = change;
    let next = expected_generation as i64 + 1;
    // Review I2: touch the machine's live node rows first. A publication holds
    // its node row lock while it re-checks the gate, so it either commits
    // before this change (and its attempt is then handed off) or fails to
    // serialize after it. N1: locked in id order, with a bounded wait.
    bound(tx).await?;
    lock_live_nodes(tx, Some(key)).await?;
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
    if let Some(reason) = management::hold(conn, node).await? {
        return Ok(Some(reason));
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
    bound(tx).await?;
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
        let revoked: Vec<Uuid> = sqlx::query_scalar(
            "UPDATE nodes SET revoked_at=$2 WHERE id=$1 AND revoked_at IS NULL RETURNING id",
        )
        .bind(id)
        .bind(now)
        .fetch_all(&mut *tx)
        .await?;
        lost::release_revoked(tx, &revoked, now).await?;
        return Ok(revoked.len() as u32);
    }
    if !valid_machine_key(machine) {
        return Err(Error::Invalid("machine"));
    }
    lock_machine(tx, machine).await?;
    lock_live_nodes(tx, Some(machine)).await?;
    let revoked: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE nodes SET revoked_at=$2 WHERE machine_key=$1 AND revoked_at IS NULL RETURNING id",
    )
    .bind(machine)
    .bind(now)
    .fetch_all(&mut *tx)
    .await?;
    lost::release_revoked(tx, &revoked, now).await?;
    sqlx::query("DELETE FROM machine_controls WHERE machine_key=$1")
        .bind(machine)
        .execute(&mut *tx)
        .await?;
    Ok(revoked.len() as u32)
}

/// Fix round 2 (review I2): the publication's first lock, on its node row. A
/// serialization failure here means a machine change (Stop, Drain) or an
/// availability change committed after the transaction began, and a lock
/// timeout means one is still open: `Ok(false)`, which publishes no offer.
pub async fn lock_publishing_node(tx: &mut PgConnection, node: Uuid) -> Result<bool, Error> {
    management::lock(tx).await?;
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
/// A lock timeout, deadlock or serialization failure: contention a caller may retry.
pub fn is_contention(e: &sqlx::Error) -> bool {
    is_lock_timeout(e) || is_serialization_failure(e)
}
/// SQLSTATE 40001 (serialization failure) or 40P01 (deadlock).
pub fn is_serialization_failure(e: &sqlx::Error) -> bool {
    e.as_database_error()
        .and_then(|d| d.code())
        .is_some_and(|c| c == "40001" || c == "40P01")
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_the_first_exempt_handoffs_up_to_the_setting_stay_exempt() {
        let flags = [true, false, true, true, true, true, true, true];
        let capped = super::cap_exempt(flags, 5);
        assert_eq!(capped, [true, false, true, true, true, true, false, false]);
        assert_eq!(super::cap_exempt(flags, 1)[2..], [false; 6]);
        assert_eq!(super::cap_exempt([false; 3], 5), [false; 3]);
    }
}

pub mod install;

/// Whether the attempt no longer holds its task for this node: cancelled, abandoned or already
/// settled. A result for it can never be recorded, so the worker must stop offering it.
pub async fn attempt_released(pool: &sqlx::PgPool, attempt: uuid::Uuid, node: uuid::Uuid) -> bool {
    let query = "SELECT NOT holds_task FROM execution_attempts WHERE attempt_id=$1 AND node_id=$2";
    let released: Result<Option<bool>, _> = sqlx::query_scalar(query)
        .bind(attempt)
        .bind(node)
        .fetch_optional(pool)
        .await;
    released.ok().flatten().unwrap_or(false)
}
