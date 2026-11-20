//! Replay windows, rolling bounds, history and epoch fences. No unresolved
//! obligation is ever evicted.
//!
//! The retention clock (`RetentionState::clock_ms`) is the latest host time a
//! commit carried. A removal from the State is allowed only when the commit
//! writes its history (a settled cohort), or when it is a window entry whose
//! expiry the clock has reached or that the rolling bound evicts. Nothing
//! writes event tombstones, correlation fences or disposal records any more;
//! those an older engine left are drained ([`drain_step`]). [`verify_removals`] is that one check, shared by
//! every storage backend. It covers runs, operations, audit receipts, events
//! and keyed admission receipts, and the expiry records the window rule
//! reads (which may only move later). Waits, budgets and recovery requests
//! are live sub-records that ordinary transitions consume (a wait wakes, a
//! request is taken); the history-or-expiry rule cannot judge them, so they
//! are not fenced here.
use crate::lazy::{LazyMap, LazySeq};
use crate::{
    Error, Intent, IntentStatus, Receipt, Result, Run, SettledCohort, State, StorageError,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetentionPolicy {
    pub event_replay_ms: u64,
    /// Deprecated and ignored: a finished run leaves the record when it
    /// finishes. Accepted (and defaulted when absent) so stored policies
    /// decode.
    #[serde(default = "default_terminal_run_ms")]
    pub terminal_run_ms: u64,
    pub audit_ms: u64,
    /// Deprecated and ignored: there are no namespace limits. Accepted (and
    /// defaulted when absent) so stored policies decode.
    #[serde(default = "default_max_tombstones")]
    pub max_tombstones: usize,
    /// Window of a keyed admission receipt, from its acceptance.
    #[serde(default = "default_admission_replay_ms")]
    pub admission_replay_ms: u64,
    /// How many window entries (events and keyed admission receipts
    /// together) that no live work holds the engine remembers: past it the
    /// oldest are evicted, whatever their window ([`evict`]). Positive.
    #[serde(default = "default_replay_keep")]
    pub replay_keep: usize,
    /// How many finished runs history keeps: each commit that writes
    /// history deletes the oldest beyond it (a bounded number per commit,
    /// see [`crate::history::HISTORY_TRIM_RUNS_PER_COMMIT`]). `0` writes no
    /// history at all (inspection then finds no finished run).
    #[serde(default = "default_history_keep")]
    pub history_keep: u64,
    /// The drain of a namespace an older engine wrote ([`drain_step`]):
    /// at most this many roots examined, and settled cohorts moved to
    /// history, per commit (together with those [`settle`] moves).
    /// Positive.
    #[serde(default = "default_drain_cohorts")]
    pub drain_cohorts_per_commit: usize,
    /// The drain's other work per commit: at most this many legacy entries
    /// (tombstones, correlation fences, disposals, admin receipts, keyed
    /// receipts and events examined), and at most this many PostgreSQL
    /// legacy tombstone rows deleted. Positive.
    #[serde(default = "default_drain_entries")]
    pub drain_entries_per_commit: usize,
    /// At most this many expired window entries examined (dropped or
    /// deferred) per commit ([`drop_expired`]). Positive.
    #[serde(default = "default_window_drops")]
    pub window_drops_per_commit: usize,
    /// At most this many window entries evicted past `replay_keep` per
    /// commit ([`evict`]); the removal fences allow no more. Positive.
    #[serde(default = "default_replay_evictions")]
    pub replay_evictions_per_commit: usize,
    /// A commit that writes history trims, of each rolling set, at most
    /// this many more than it writes ([`crate::history::trim_budget`]).
    /// Positive.
    #[serde(default = "default_history_trim")]
    pub history_trim_per_commit: usize,
    /// The largest page `collect_garbage` accepts (its `limit`), which also
    /// bounds the PostgreSQL artifact catalog delete of a GC commit.
    /// Positive.
    #[serde(default = "default_gc_page_limit")]
    pub gc_page_limit: usize,
    /// At most this many cohorts that settle in a commit leave the State in
    /// it ([`settle`]); the rest re-arm the drain, which takes them in later
    /// commits. Positive.
    #[serde(default = "default_settle_cohorts")]
    pub settle_cohorts_per_commit: usize,
}
fn default_settle_cohorts() -> usize {
    COHORTS_PER_COMMIT
}
fn default_window_drops() -> usize {
    WINDOW_DROPS_PER_COMMIT
}
fn default_replay_evictions() -> usize {
    REPLAY_EVICTIONS_PER_COMMIT
}
fn default_history_trim() -> usize {
    crate::history::HISTORY_TRIM_RUNS_PER_COMMIT
}
fn default_gc_page_limit() -> usize {
    GC_PAGE_LIMIT
}
/// The default of `RetentionPolicy::gc_page_limit`.
pub const GC_PAGE_LIMIT: usize = 1_000;
fn default_drain_cohorts() -> usize {
    50
}
fn default_drain_entries() -> usize {
    500
}
fn default_replay_keep() -> usize {
    256
}
fn default_history_keep() -> u64 {
    10_000
}
fn default_admission_replay_ms() -> u64 {
    604_800_000
}
fn default_terminal_run_ms() -> u64 {
    604_800_000
}
fn default_max_tombstones() -> usize {
    100_000
}
impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            event_replay_ms: 86_400_000,
            terminal_run_ms: default_terminal_run_ms(),
            audit_ms: 604_800_000,
            max_tombstones: default_max_tombstones(),
            admission_replay_ms: default_admission_replay_ms(),
            replay_keep: default_replay_keep(),
            history_keep: default_history_keep(),
            drain_cohorts_per_commit: default_drain_cohorts(),
            drain_entries_per_commit: default_drain_entries(),
            window_drops_per_commit: default_window_drops(),
            replay_evictions_per_commit: default_replay_evictions(),
            history_trim_per_commit: default_history_trim(),
            gc_page_limit: default_gc_page_limit(),
            settle_cohorts_per_commit: default_settle_cohorts(),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventTombstone {
    pub producer: String,
    pub epoch: u64,
    pub digest: String,
    pub disposed_at_ms: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Disposal {
    pub kind: String,
    pub key: String,
    pub at_ms: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RetentionState {
    #[serde(default)]
    pub last_gc_at_ms: Option<i64>,
    pub policy: RetentionPolicy,
    pub producer_epochs: BTreeMap<String, u64>,
    pub event_epochs: LazyMap<String, u64>,
    #[serde(default)]
    pub event_expires_at_ms: LazyMap<String, i64>,
    #[serde(default)]
    pub expired_correlations: LazyMap<String, (String, u64)>,
    pub event_tombstones: LazyMap<String, EventTombstone>,
    pub artifact_disposals: BTreeMap<String, i64>,
    pub disposals: LazySeq<Disposal>,
    pub gc_cursor: Option<String>,
    /// The latest host time any commit carried; never decreases.
    #[serde(default)]
    pub clock_ms: Option<i64>,
    /// Window entries in expiry order, one record each, keyed
    /// [`window_key`]`(expiry, member)`; a member is `WINDOW_EVENT` or
    /// `WINDOW_ADMISSION` plus the entry's key.
    #[serde(default)]
    pub expiry: LazyMap<String, ()>,
    /// Expiry of each keyed admission receipt that has one recorded.
    #[serde(default)]
    pub admission_expires_at_ms: LazyMap<String, i64>,
    /// Set by a clock rebase: a request for a key the engine does not
    /// remember whose request time is at or before it is stale. Never
    /// decreases. Absent (and not written) until the first rebase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_floor_ms: Option<i64>,
    /// The window entries no live work holds, in acceptance order: one
    /// record each, keyed [`replay_key`]`(acceptance sequence, member)`.
    /// Past `policy.replay_keep` the oldest are evicted.
    #[serde(default)]
    pub replay_order: LazyMap<String, ()>,
    /// Keyed admission receipts whose run is live, by run id: each joins
    /// `replay_order` when its run leaves the record.
    #[serde(default)]
    pub replay_held: LazyMap<String, String>,
    /// Progress of the drain of what an older engine left in the record
    /// ([`drain_step`]). Absent in a State written before it: the drain
    /// then runs.
    #[serde(default)]
    pub drain: DrainState,
    /// Events still to be judged again because the uncertain operations of
    /// their contract and scope changed (a hold through an uncertain
    /// completion began, ended or lapsed): by contract key, the acceptance
    /// sequence after which [`rejoin`] continues. It judges at most
    /// `policy.window_drops_per_commit` of them per commit. Empty (and not
    /// written) when nothing is pending.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub rejudge: BTreeMap<String, u64>,
}
/// Progress of the drain ([`drain_step`]), stored with the State so a
/// restart continues where it stopped.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrainState {
    /// Nothing is left to drain: every commit's drain work is one check.
    pub done: bool,
    /// The last root the cohort scan examined (`None`: from the first).
    pub root_cursor: Option<String>,
    /// The cohort scan has passed the last root since it was (re)armed.
    #[serde(default)]
    pub roots_done: bool,
    /// The entry phase: one of the `DRAIN_*` phases.
    pub phase: u8,
    /// The last key the entry phase examined (`None`: from the first).
    pub cursor: Option<String>,
    /// The namespace sequence when the drain began: events at or before it
    /// were accepted by the older engine, and the event phase judges them.
    #[serde(default)]
    pub legacy_sequence: Option<u64>,
}
/// The [`RetentionState::replay_order`] key of `member` accepted at
/// `sequence`: the sequence as 16 lowercase hex digits, then the member.
/// Byte order of keys is acceptance order.
pub fn replay_key(sequence: u64, member: &str) -> String {
    format!("{sequence:016x}{member}")
}
/// The acceptance sequence and member of a [`replay_key`].
pub fn replay_entry(key: &str) -> Option<(u64, &str)> {
    Some((
        u64::from_str_radix(key.get(..16)?, 16).ok()?,
        key.get(16..)?,
    ))
}
/// The acceptance sequence of a keyed admission receipt: that of its run
/// id (`run-<16 hex>`, assigned by the admitting commit), else 0.
pub fn admission_sequence(receipt: &crate::KeyedAdmissionReceipt) -> u64 {
    receipt
        .run_id
        .strip_prefix("run-")
        .and_then(|hex| u64::from_str_radix(hex, 16).ok())
        .unwrap_or(0)
}
/// The [`RetentionState::replay_order`] key of a keyed admission receipt.
pub fn admission_order_key(receipt: &crate::KeyedAdmissionReceipt) -> String {
    replay_key(
        admission_sequence(receipt),
        &format!("{WINDOW_ADMISSION}{}", receipt.key),
    )
}
/// The [`RetentionState::replay_order`] key of an event.
pub fn event_order_key(event: &crate::AcceptedEvent) -> String {
    replay_key(event.sequence, &format!("{WINDOW_EVENT}{}", event.key))
}
/// The default of `RetentionPolicy::replay_evictions_per_commit`: entries
/// at the head of the eviction order examined (evicted, or taken out of the
/// order because live work holds them) per commit. Counted apart from the
/// window drop's own bound.
pub const REPLAY_EVICTIONS_PER_COMMIT: usize = 64;
/// Refusal of a commit that evicts a window entry other than as
/// [`evict`] does (see [`verify_removals`]).
pub const REFUSE_EVICTION: &str = "window entry evicted without its licence";
/// Refusal of a commit that changes an accepted event's identity (its key,
/// sequence, digest, acceptance time or envelope); only its destinations
/// change (fan-out).
pub const REFUSE_EVENT_CHANGE: &str = "immutable accepted event changed";
/// The `RetentionState::expiry` key of `member` expiring at
/// `expires_at_ms`: the expiry as 16 lowercase hex digits of its
/// order-preserving unsigned form, then the member. Byte order of keys is
/// expiry order, then member order.
pub fn window_key(expires_at_ms: i64, member: &str) -> String {
    format!("{:016x}{member}", (expires_at_ms as u64) ^ (1 << 63))
}
/// The expiry and member of a [`window_key`].
pub fn window_entry(key: &str) -> Option<(i64, &str)> {
    let at = u64::from_str_radix(key.get(..16)?, 16).ok()?;
    Some(((at ^ (1 << 63)) as i64, key.get(16..)?))
}
/// `RetentionState::expiry` member prefix of an admitted event.
pub const WINDOW_EVENT: &str = "e:";
/// `RetentionState::expiry` member prefix of a keyed admission receipt.
pub const WINDOW_ADMISSION: &str = "k:";
pub const REFUSE_RUN_REMOVAL: &str = "run removal requires a history record in the same commit";
pub const REFUSE_EARLY_REMOVAL: &str = "window entry removed before its expiry";
pub const REFUSE_CLOCK: &str = "retention clock cannot decrease";
/// Refusal of a commit that changes the stale floor without a licence, or
/// beyond its licence.
pub const REFUSE_STALE_FLOOR: &str = "stale floor changed without its licence";
/// Refusal of a commit whose clock rebase or floor receipt is not exactly
/// that commit (another clock or floor, a replayed receipt, or other work).
pub const REFUSE_LICENCE: &str = "clock rebase or floor receipt does not match this commit";
/// Kind of the admin receipt that lowers the stale floor
/// ([`crate::Engine::lower_stale_floor`]).
pub const STALE_FLOOR_LOWERED: &str = "stale_floor_lowered";
/// Kind of the admin receipt a clock rebase writes
/// ([`crate::Engine::rebase_retention_clock`]).
pub const CLOCK_REBASE: &str = "retention_clock_rebased";
const REFUSE_RUN_OBLIGATIONS: &str = "unresolved run obligations cannot be disposed";
const REFUSE_EFFECT: &str = "unresolved effect cannot be disposed";
const REFUSE_RECEIPT_CHANGE: &str = "immutable keyed admission receipt removed or changed";
/// Refusal of a commit that lowers, orphans or under-sets a window expiry.
pub const REFUSE_EXPIRY: &str = "window expiry cannot be shortened";
fn window(ms: u64) -> i64 {
    i64::try_from(ms).unwrap_or(i64::MAX)
}
/// Expiry of event `key`: its recorded expiry, else acceptance plus the
/// policy's replay window.
pub fn event_expiry(s: &State, key: &str) -> Option<i64> {
    if let Some(at) = s.retention.event_expires_at_ms.get(key) {
        return Some(*at);
    }
    s.events.get(key).map(|e| {
        e.accepted_at_ms
            .saturating_add(window(s.retention.policy.event_replay_ms))
    })
}
/// Expiry of keyed admission receipt `key`: its recorded expiry, else
/// acceptance plus the policy's admission window.
pub fn admission_expiry(s: &State, key: &str) -> Option<i64> {
    if let Some(at) = s.retention.admission_expires_at_ms.get(key) {
        return Some(*at);
    }
    s.keyed_admissions.get(key).map(|r| {
        r.accepted_at_ms
            .saturating_add(window(s.retention.policy.admission_replay_ms))
    })
}
/// Whether an entry expiring at `expiry` has expired at `clock_ms`. Nothing
/// expires before the first host time.
pub fn expired(expiry: i64, clock_ms: Option<i64>) -> bool {
    clock_ms.is_some_and(|now| expiry <= now)
}
/// The window of a keyed admission receipt or an event in `s` without its
/// recorded expiry: acceptance plus the policy's window.
fn fallback_expiry(s: &State, window: Window, key: &str) -> Option<i64> {
    let policy = &s.retention.policy;
    match window {
        Window::Event => s.events.get(key).map(|e| {
            e.accepted_at_ms
                .saturating_add(self::window(policy.event_replay_ms))
        }),
        Window::Admission => s.keyed_admissions.get(key).map(|r| {
            r.accepted_at_ms
                .saturating_add(self::window(policy.admission_replay_ms))
        }),
    }
}
/// Which expiry record: `event_expires_at_ms` or `admission_expires_at_ms`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Window {
    Event,
    Admission,
}
/// One change of an expiry record, from `old` to `new`, judged against the
/// State the commit stores (`item_kept`: the item is still in it;
/// `fallback`: its acceptance plus the policy window there). A recorded
/// expiry may only move later, may be deleted only with its item, and a new
/// one is at least the fallback.
pub fn check_expiry_change(
    old: Option<i64>,
    new: Option<i64>,
    item_kept: bool,
    fallback: Option<i64>,
) -> std::result::Result<(), StorageError> {
    let allowed = match (old, new) {
        (Some(old), Some(new)) => new >= old,
        (Some(_), None) => !item_kept,
        (None, Some(new)) => fallback.is_some_and(|fallback| new >= fallback),
        (None, None) => true,
    };
    if allowed {
        Ok(())
    } else {
        Err(refuse(REFUSE_EXPIRY))
    }
}
/// What an admin licence receipt in a commit's own history allows: exactly
/// one clock rebase, or exactly one lowering of the stale floor. Each
/// carries the receipt's sequence, which must be new in this commit (after
/// the stored sequence), so a receipt cannot be replayed, even after its
/// history row was deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Licence {
    /// [`CLOCK_REBASE`]: the clock from `from_ms` down to `to_ms`.
    Rebase {
        from_ms: i64,
        to_ms: i64,
        sequence: u64,
    },
    /// [`STALE_FLOOR_LOWERED`]: the stale floor from `from_ms` down to
    /// `to_ms`.
    FloorLowered {
        from_ms: i64,
        to_ms: i64,
        sequence: u64,
    },
}
/// The licence `next`'s commit carries: a [`CLOCK_REBASE`] or
/// [`STALE_FLOOR_LOWERED`] admin receipt in `next.settled`, the history this
/// commit writes. A receipt left in the live State licenses nothing. More
/// than one, or one without its clocks, is refused.
pub fn licence(next: &State) -> std::result::Result<Option<Licence>, StorageError> {
    let mut found = None;
    for r in next
        .settled
        .iter()
        .filter(|cohort| cohort.root == "admin")
        .flat_map(|cohort| &cohort.receipts)
        .filter(|r| r.run == "admin")
    {
        let clocks = || Some((r.detail["from_ms"].as_i64()?, r.detail["to_ms"].as_i64()?));
        let licence = match r.kind.as_str() {
            CLOCK_REBASE => clocks().map(|(from_ms, to_ms)| Licence::Rebase {
                from_ms,
                to_ms,
                sequence: r.sequence,
            }),
            STALE_FLOOR_LOWERED => clocks().map(|(from_ms, to_ms)| Licence::FloorLowered {
                from_ms,
                to_ms,
                sequence: r.sequence,
            }),
            _ => continue,
        }
        .ok_or_else(|| refuse(REFUSE_LICENCE))?;
        if found.replace(licence).is_some() {
            return Err(refuse(REFUSE_LICENCE));
        }
    }
    Ok(found)
}
/// Whether `next`'s commit carries an admin licence receipt at all (valid
/// or not): a format 3 store sends such a commit to its complete path,
/// where [`verify_removals`] judges it.
pub fn carries_licence(next: &State) -> bool {
    next.settled
        .iter()
        .filter(|cohort| cohort.root == "admin")
        .flat_map(|cohort| &cohort.receipts)
        .any(|r| r.run == "admin" && (r.kind == CLOCK_REBASE || r.kind == STALE_FLOOR_LOWERED))
}
/// The expiry a clock rebase to `to_ms` gives entry `key` of `window`:
/// `to_ms` plus the entry's own window (the admission window; for an event
/// `max(policy.event_replay_ms, contract.retention_ms)`).
pub fn rebase_cap(s: &State, window: Window, key: &str, to_ms: i64) -> i64 {
    let window_ms = match window {
        Window::Admission => self::window(s.retention.policy.admission_replay_ms),
        Window::Event => event_window(s, key),
    };
    to_ms.saturating_add(window_ms)
}
/// The clock, the stale floor and the licence of a commit from `previous`
/// to `next`. Without a licence the clock never decreases and the floor
/// never changes. With one, the commit must be exactly that licence: its
/// receipt's sequence is new in this commit, and
/// - a rebase lowers the clock from exactly `from_ms` to exactly `to_ms <
///   from_ms`, and may only raise the floor, to at most `to_ms`;
/// - a floor lowering leaves the clock alone and lowers the floor from
///   exactly `from_ms` to exactly `to_ms < from_ms`.
///
/// What else such a commit may not do (add or remove a run, operation,
/// event or keyed receipt, remove an audit receipt, change an expiry other
/// than by a rebase cap) is checked where those records are.
fn check_licence(
    previous: &State,
    next: &State,
    licence: Option<Licence>,
) -> std::result::Result<(), StorageError> {
    let (old_clock, new_clock) = (previous.retention.clock_ms, next.retention.clock_ms);
    let (old_floor, new_floor) = (
        previous.retention.stale_floor_ms,
        next.retention.stale_floor_ms,
    );
    let Some(licence) = licence else {
        if old_clock.is_some_and(|old| new_clock.is_none_or(|new| new < old)) {
            return Err(refuse(REFUSE_CLOCK));
        }
        return check_floor_unchanged(previous, next);
    };
    check_licensed_header(previous, next)?;
    let (Licence::Rebase { sequence, .. } | Licence::FloorLowered { sequence, .. }) = licence;
    if sequence <= previous.sequence || sequence > next.sequence {
        return Err(refuse(REFUSE_LICENCE));
    }
    match licence {
        Licence::Rebase { from_ms, to_ms, .. } => {
            if old_clock != Some(from_ms) || new_clock != Some(to_ms) || to_ms >= from_ms {
                return Err(refuse(REFUSE_LICENCE));
            }
            let raised = match (old_floor, new_floor) {
                (_, Some(new)) => new <= to_ms && old_floor.is_none_or(|old| new >= old),
                (None, None) => true,
                (Some(_), None) => false,
            };
            if !raised {
                return Err(refuse(REFUSE_STALE_FLOOR));
            }
        }
        Licence::FloorLowered { from_ms, to_ms, .. } => {
            if old_clock != new_clock
                || old_floor != Some(from_ms)
                || new_floor != Some(to_ms)
                || to_ms >= from_ms
            {
                return Err(refuse(REFUSE_LICENCE));
            }
        }
    }
    Ok(())
}
/// In a licensed commit every header field the licence does not name is
/// unchanged: the retention policy, producer epochs, GC state and artifact
/// disposals, the artifacts, bindings, catalog revision and history,
/// execution denials, authority revocations and recovery requests. (The
/// clock, the floor and the sequence are judged by [`check_licence`].)
pub(crate) fn check_licensed_header(
    previous: &State,
    next: &State,
) -> std::result::Result<(), StorageError> {
    fn same<T: Serialize>(a: &T, b: &T) -> bool {
        matches!((serde_json::to_vec(a), serde_json::to_vec(b)), (Ok(a), Ok(b)) if a == b)
    }
    // Every field is named (no `..`): a field added to `State` or
    // `RetentionState` does not compile here until someone decides whether a
    // licence may change it. `_` marks a field this comparison does not
    // judge, with the reason.
    let State {
        // Containers: removals are judged per key by `check_removals`, which
        // a licence does not relax; their content (and waits and budgets,
        // live sub-records) any commit may change, so a licence adds nothing.
        keyed_admissions: _,
        runs: _,
        receipts: _,
        intents: _,
        waits: _,
        events: _,
        budgets: _,
        // Derived from the State and rebuilt by `indexes::sync`.
        indexes: _,
        // Judged by `check_licence` (it must increase).
        sequence: _,
        // Not stored.
        protection_available: _,
        settled: _,
        replay_touched: _,
        catalog_history,
        catalog_history_at_ms,
        retention,
        execution_denied,
        authority_revocations,
        recovery_requests,
        artifacts,
        bindings,
        binding_schedules,
        deactivated_bindings,
        catalog_revision,
    } = previous;
    let RetentionState {
        // Retained-history containers and the window: fenced per key
        // (`check_removals`, `check_expiry_record`), not here.
        event_epochs: _,
        event_expires_at_ms: _,
        expired_correlations: _,
        event_tombstones: _,
        disposals: _,
        expiry: _,
        admission_expires_at_ms: _,
        replay_order: _,
        replay_held: _,
        // What a licence names: judged by `check_licence`.
        clock_ms: _,
        stale_floor_ms: _,
        last_gc_at_ms,
        policy,
        producer_epochs,
        artifact_disposals,
        gc_cursor,
        // A licensed commit drains nothing and judges no event again.
        drain,
        rejudge,
    } = retention;
    let (n, r) = (next, &next.retention);
    let unchanged = same(policy, &r.policy)
        && *drain == r.drain
        && *rejudge == r.rejudge
        && *producer_epochs == r.producer_epochs
        && *last_gc_at_ms == r.last_gc_at_ms
        && *gc_cursor == r.gc_cursor
        && *artifact_disposals == r.artifact_disposals
        && *catalog_revision == n.catalog_revision
        && *catalog_history_at_ms == n.catalog_history_at_ms
        && *execution_denied == n.execution_denied
        && *authority_revocations == n.authority_revocations
        && same(catalog_history, &n.catalog_history)
        && same(bindings, &n.bindings)
        && same(binding_schedules, &n.binding_schedules)
        && *deactivated_bindings == n.deactivated_bindings
        && same(recovery_requests, &n.recovery_requests)
        && same(artifacts, &n.artifacts);
    if unchanged {
        Ok(())
    } else {
        Err(refuse(REFUSE_LICENCE))
    }
}
/// Without a licence the stale floor never changes (a format 3 bounded
/// commit never carries one).
pub fn check_floor_unchanged(
    previous: &State,
    next: &State,
) -> std::result::Result<(), StorageError> {
    if previous.retention.stale_floor_ms != next.retention.stale_floor_ms {
        return Err(refuse(REFUSE_STALE_FLOOR));
    }
    Ok(())
}
/// [`check_expiry_change`] for record `key` of `window`, against `next`. In
/// a commit with a licence the only allowed change is a rebase cap: a
/// recorded expiry lowered to exactly [`rebase_cap`] computed from
/// `previous` (its policy, artifacts and the stored event), with the clock
/// at the rebase's `to_ms`. `previous` needs its header and, for an event,
/// that event's stored record.
pub(crate) fn check_expiry_record(
    previous: &State,
    next: &State,
    window: Window,
    key: &str,
    old: Option<i64>,
    new: Option<i64>,
) -> std::result::Result<(), StorageError> {
    match licence(next)? {
        Some(Licence::Rebase { to_ms, .. }) => {
            if next.retention.clock_ms != Some(to_ms) {
                return Err(refuse(REFUSE_LICENCE));
            }
            let capped = matches!((old, new), (Some(old), Some(new))
                if new < old && new == rebase_cap(previous, window, key, to_ms));
            return if capped {
                Ok(())
            } else {
                Err(refuse(REFUSE_EXPIRY))
            };
        }
        Some(Licence::FloorLowered { .. }) => return Err(refuse(REFUSE_EXPIRY)),
        None => {}
    }
    let item_kept = match window {
        Window::Event => next.events.contains_key(key),
        Window::Admission => next.keyed_admissions.contains_key(key),
    };
    let fallback = match (old, new) {
        (None, Some(_)) => fallback_expiry(next, window, key),
        _ => None,
    };
    check_expiry_change(old, new, item_kept, fallback)
}
/// Every expiry record change between two complete States.
fn check_expiry_records(previous: &State, next: &State) -> std::result::Result<(), StorageError> {
    for (window, old, new) in [
        (
            Window::Event,
            &previous.retention.event_expires_at_ms,
            &next.retention.event_expires_at_ms,
        ),
        (
            Window::Admission,
            &previous.retention.admission_expires_at_ms,
            &next.retention.admission_expires_at_ms,
        ),
    ] {
        for (key, at) in old {
            let after = new.get(key).copied();
            if after != Some(*at) {
                check_expiry_record(previous, next, window, key, Some(*at), after)?;
            }
        }
        for (key, at) in new {
            if !old.contains_key(key) {
                check_expiry_record(previous, next, window, key, None, Some(*at))?;
            }
        }
    }
    Ok(())
}
/// Move the clock to `now_ms` unless it is already later.
pub(crate) fn advance_clock(s: &mut State, now_ms: i64) {
    let clock = s
        .retention
        .clock_ms
        .map_or(now_ms, |clock| clock.max(now_ms));
    s.retention.clock_ms = Some(clock);
}
fn refuse(text: &str) -> StorageError {
    StorageError::Backend(text.into())
}
fn intent_settled(status: &IntentStatus) -> bool {
    matches!(
        status,
        IntentStatus::Completed | IntentStatus::Rejected | IntentStatus::Cancelled
    )
}
/// The one removal check every backend runs on a commit from `previous` to
/// `next` (see the module docs). Each record of `previous` missing from
/// `next` must be allowed:
///
/// - a run: its history is in `next.settled`, it is terminal, its children
///   are terminal and its operations settled;
/// - an operation: its own history and its run's are in `next.settled` and it
///   is settled;
/// - an audit receipt: its history is in `next.settled`;
/// - an event: its expiry has been reached, or it is evicted as
///   [`check_evictions`] licenses;
/// - a keyed admission receipt: its expiry has been reached, or it is
///   evicted as [`check_evictions`] licenses (a changed receipt is always
///   refused).
///
/// The clock never decreases, the stale floor never changes, and expiry
/// records follow [`check_expiry_change`], except in a commit that carries
/// an admin licence ([`licence`]), which must be exactly that licence and do
/// nothing else (see [`check_licence`]): a clock rebase lowers the clock and
/// caps expiries to [`rebase_cap`]; a floor lowering lowers the floor.
/// Legacy event tombstones, correlation fences and disposal records (which
/// no engine writes any more; the drain removes them) may always be removed
/// and license nothing. Artifact removals are checked by
/// [`verify_artifact_removals`].
pub fn verify_removals(previous: &State, next: &State) -> std::result::Result<(), StorageError> {
    if licence(next)?.is_some() {
        let added = next.runs.keys().any(|k| !previous.runs.contains_key(k))
            || next
                .intents
                .keys()
                .any(|k| !previous.intents.contains_key(k))
            || next.events.keys().any(|k| !previous.events.contains_key(k))
            || next
                .keyed_admissions
                .keys()
                .any(|k| !previous.keyed_admissions.contains_key(k));
        if added {
            return Err(refuse(REFUSE_LICENCE));
        }
    }
    let kept: BTreeSet<u64> = next.receipts.iter().map(|r| r.sequence).collect();
    let stored = &previous.retention.replay_order;
    let evicted = check_evictions(
        previous,
        next,
        &mut next
            .retention
            .replay_order
            .keys()
            .filter(|key| !stored.contains_key(*key))
            .cloned(),
        &Stored {
            ordered: &|key| Ok(stored.contains_key(key)),
            memberships: &|key| Ok(previous.indexes.event_memberships.get(key).cloned()),
            waiters: &|key| Ok(previous.indexes.waiters.get(key).cloned()),
        },
    )?;
    check_removals(
        previous,
        next,
        &|sequence| kept.contains(&sequence),
        &|window, key| evicted.contains(&(window, key.to_string())),
    )?;
    check_expiry_records(previous, next)
}
/// [`verify_removals`] without the expiry records, with the receipts `next`
/// keeps given by `receipt_kept`. `previous` need only hold the removed records, the
/// operations of removed runs and the children of removed runs (and its
/// header fields), so a record backend can check a patch.
pub(crate) fn check_removals(
    previous: &State,
    next: &State,
    receipt_kept: &dyn Fn(u64) -> bool,
    evicted: &dyn Fn(Window, &str) -> bool,
) -> std::result::Result<(), StorageError> {
    verify_artifact_removals(previous, next)?;
    let clock = next.retention.clock_ms;
    let licensed = licence(next)?;
    check_licence(previous, next, licensed)?;
    // A licensed commit removes nothing.
    let removal = || -> std::result::Result<(), StorageError> {
        if licensed.is_some() {
            return Err(refuse(REFUSE_LICENCE));
        }
        Ok(())
    };
    for (key, receipt) in &previous.keyed_admissions {
        match next.keyed_admissions.get(key) {
            Some(kept) if kept == receipt => {}
            Some(_) => return Err(refuse(REFUSE_RECEIPT_CHANGE)),
            None if admission_expiry(previous, key).is_some_and(|at| expired(at, clock))
                || evicted(Window::Admission, key) =>
            {
                removal()?
            }
            None => return Err(refuse(REFUSE_EARLY_REMOVAL)),
        }
    }
    for (key, event) in &previous.events {
        if let Some(kept) = next.events.get(key) {
            check_event_change(event, kept)?;
            continue;
        }
        removal()?;
        if !event_expiry(previous, key).is_some_and(|at| expired(at, clock))
            && !evicted(Window::Event, key)
        {
            return Err(refuse(REFUSE_EARLY_REMOVAL));
        }
    }
    let removed_receipts: Vec<&crate::Receipt> = previous
        .receipts
        .iter()
        .filter(|r| !receipt_kept(r.sequence))
        .collect();
    if !removed_receipts.is_empty() {
        removal()?;
        let settled: BTreeSet<u64> = next
            .settled
            .iter()
            .flat_map(|c| &c.receipts)
            .map(|r| r.sequence)
            .collect();
        for receipt in removed_receipts {
            if !settled.contains(&receipt.sequence) {
                return Err(refuse(REFUSE_RUN_REMOVAL));
            }
        }
    }
    let removed_runs: Vec<(&String, &Run)> = previous
        .runs
        .iter()
        .filter(|(id, _)| !next.runs.contains_key(*id))
        .collect();
    let removed_intents: Vec<(&String, &Intent)> = previous
        .intents
        .iter()
        .filter(|(id, _)| !next.intents.contains_key(*id))
        .collect();
    if removed_runs.is_empty() && removed_intents.is_empty() {
        return Ok(());
    }
    removal()?;
    // A cohort's runs belong to its root.
    let mut settled_runs: BTreeMap<&str, &Run> = BTreeMap::new();
    for cohort in &next.settled {
        for run in &cohort.runs {
            if run.root != cohort.root {
                return Err(refuse(REFUSE_RUN_OBLIGATIONS));
            }
            settled_runs.insert(run.id.as_str(), run);
        }
    }
    let settled_intents: BTreeMap<&str, &Intent> = next
        .settled
        .iter()
        .flat_map(|c| &c.intents)
        .map(|i| (i.id.as_str(), i))
        .collect();
    let mut intents_of: BTreeMap<&str, Vec<&Intent>> = BTreeMap::new();
    if !removed_runs.is_empty() {
        for intent in previous
            .intents
            .values()
            .chain(settled_intents.values().copied())
        {
            intents_of
                .entry(intent.run.as_str())
                .or_default()
                .push(intent);
        }
    }
    for (id, _) in removed_runs {
        let Some(settled) = settled_runs.get(id.as_str()) else {
            return Err(refuse(REFUSE_RUN_REMOVAL));
        };
        let unresolved = {
            // Each run and operation as this commit leaves it.
            let terminal = |child: &String| {
                settled_runs
                    .get(child.as_str())
                    .copied()
                    .or_else(|| next.runs.get(child))
                    .is_none_or(|r| r.status.terminal())
            };
            // An operation of a run that leaves goes with it: one still
            // live would be stranded (its run's history is written).
            let settled_intent = |intent: &&Intent| {
                !next.intents.contains_key(&intent.id)
                    && intent_settled(
                        &settled_intents
                            .get(intent.id.as_str())
                            .copied()
                            .unwrap_or(intent)
                            .status,
                    )
            };
            !settled.status.terminal()
                || !settled.children.iter().all(terminal)
                || !intents_of
                    .get(id.as_str())
                    .into_iter()
                    .flatten()
                    .all(settled_intent)
        };
        if unresolved {
            return Err(refuse(REFUSE_RUN_OBLIGATIONS));
        }
    }
    for (id, intent) in removed_intents {
        let Some(settled) = settled_intents
            .get(id.as_str())
            .filter(|_| settled_runs.contains_key(intent.run.as_str()))
        else {
            return Err(refuse(REFUSE_RUN_REMOVAL));
        };
        if !intent_settled(&settled.status) {
            return Err(refuse(REFUSE_EFFECT));
        }
    }
    Ok(())
}
/// The default of `RetentionPolicy::settle_cohorts_per_commit`: settled
/// cohorts that leave the State in one commit.
pub const COHORTS_PER_COMMIT: usize = 50;
/// What one commit moved out of the State.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CommitReport {
    pub cohorts: usize,
    pub runs: usize,
    pub receipts: usize,
    pub events: usize,
    pub admissions: usize,
    pub deferred: usize,
    /// Window entries evicted by count ([`evict`]).
    pub evicted: usize,
    pub drained: usize,
    /// More bounded work is due than this commit's bounds allowed: settled
    /// cohorts past the settle bound, the drain of an older engine's
    /// namespace not done, expired window entries past the drop bound, or
    /// entries past `replay_keep` past the eviction bound.
    pub more: bool,
}
/// The roots this commit touched: the roots of its dirty runs and operations.
/// Call before `indexes::sync`, which clears the dirty sets. O(dirty).
pub(crate) fn candidate_roots(s: &State) -> BTreeSet<String> {
    let run_root = |run: &str| {
        s.runs
            .get(run)
            .map(|r| r.root.clone())
            .or_else(|| s.indexes.run_roots.get(run).cloned())
    };
    let mut roots = BTreeSet::new();
    for id in &s.indexes.dirty_runs {
        roots.extend(run_root(id));
    }
    for id in &s.indexes.dirty_intents {
        let root = s
            .intents
            .get(id)
            .and_then(|i| run_root(&i.run))
            .or_else(|| s.indexes.intent_roots.get(id).cloned());
        roots.extend(root);
    }
    roots
}
/// Whether the cohort of `root` is settled: it has members, none of them is
/// active and none of its operations is unsettled (the root indexes after
/// `indexes::sync`). O(1) index lookups.
fn cohort_settled(s: &State, root: &str) -> bool {
    let nonempty = |ids: Option<&BTreeSet<String>>| ids.is_some_and(|ids| !ids.is_empty());
    nonempty(s.indexes.root_members.get(root))
        && !nonempty(s.indexes.root_active.get(root))
        && !nonempty(s.indexes.root_effects.get(root))
}
/// Remove every settled cohort among `candidates` (at most `limit`) from the
/// State and hand each to storage as history (`State::settled`). A settled
/// candidate beyond `limit` sets `report.more` and re-arms the drain's
/// cohort scan ([`drain_step`]), which finds it in a later commit, so no
/// settled cohort is stranded.
pub(crate) fn settle(
    s: &mut State,
    candidates: BTreeSet<String>,
    limit: usize,
    report: &mut CommitReport,
) -> Result<()> {
    let mut roots = vec![];
    for root in candidates {
        if !cohort_settled(s, &root) {
            continue;
        }
        if roots.len() >= limit {
            report.more = true;
            let drain = &mut s.retention.drain;
            drain.done = false;
            drain.roots_done = false;
            drain.root_cursor = None;
            break;
        }
        roots.push(root);
    }
    take_cohorts(s, &roots, report)
}
/// [`remove_cohorts`] for `roots`, handing each to storage as history and
/// counting it in `report`.
fn take_cohorts(s: &mut State, roots: &[String], report: &mut CommitReport) -> Result<()> {
    for cohort in remove_cohorts(s, roots)? {
        report.cohorts += 1;
        report.runs += cohort.runs.len();
        report.receipts += cohort.receipts.len();
        s.settled.push(cohort);
    }
    Ok(())
}
/// Take the receipts at `offsets` (positions in `s.receipts`) out of the
/// State, compact the positional sequence and rebuild the receipt offset
/// indexes. O(live receipts).
fn take_receipts(s: &mut State, offsets: &BTreeSet<usize>) -> Vec<Receipt> {
    if offsets.is_empty() {
        return vec![];
    }
    let taken: Vec<Receipt> = offsets
        .iter()
        .filter_map(|&offset| s.receipts.get(offset).cloned())
        .collect();
    let mut position = 0;
    s.receipts.retain(|_| {
        let keep = !offsets.contains(&position);
        position += 1;
        keep
    });
    s.indexes.receipts.clear();
    s.indexes.all_receipts.clear();
    let live: Vec<(String, u64)> = s
        .receipts
        .iter()
        .map(|r| (r.run.clone(), r.sequence))
        .collect();
    for (offset, (run, sequence)) in live.into_iter().enumerate() {
        s.indexes
            .receipts
            .entry(run)
            .or_default()
            .insert(sequence, offset);
        s.indexes.all_receipts.insert(sequence, offset);
    }
    taken
}
/// Offsets of the receipts owned by `run`, checked against the receipt.
fn receipt_offsets(s: &State, run: &str) -> Vec<usize> {
    s.indexes
        .receipts
        .get(run)
        .into_iter()
        .flat_map(|offsets| offsets.values().copied())
        .filter(|&offset| s.receipts.get(offset).is_some_and(|r| r.run == run))
        .collect()
}
/// Remove the cohorts of `roots` from the State: their runs, operations,
/// their recovery requests, waits, receipts and budgets. Their runs and
/// operations are marked dirty, so the next `indexes::sync` drops their
/// index entries. O(cohorts), plus one compaction of the live receipts and
/// one pass over the live waits for all of them together.
pub(crate) fn remove_cohorts(s: &mut State, roots: &[String]) -> Result<Vec<SettledCohort>> {
    if roots.is_empty() {
        return Ok(vec![]);
    }
    struct Taken {
        root: String,
        members: Vec<String>,
        intents: Vec<String>,
        finished_at_ms: Option<i64>,
    }
    let mut taken = vec![];
    let mut offsets = BTreeSet::new();
    let mut owner: BTreeMap<String, usize> = BTreeMap::new();
    for root in roots {
        let members: Vec<String> = s
            .indexes
            .root_members
            .get(root)
            .map(|ids| ids.iter().cloned().collect())
            .unwrap_or_default();
        let intents: Vec<String> = s
            .indexes
            .root_intents
            .get(root)
            .map(|ids| ids.iter().cloned().collect())
            .unwrap_or_default();
        let mut finished_at_ms = None;
        for id in &members {
            offsets.extend(receipt_offsets(s, id));
            owner.insert(id.clone(), taken.len());
            let terminal = s
                .runs
                .get(id)
                .and_then(|r| r.terminal_sequence)
                .and_then(|sequence| s.indexes.all_receipts.get(&sequence).copied())
                .and_then(|offset| s.receipts.get(offset));
            if let Some(receipt) = terminal {
                finished_at_ms = finished_at_ms.max(Some(receipt.at_ms));
            }
        }
        taken.push(Taken {
            root: root.clone(),
            members,
            intents,
            finished_at_ms,
        });
    }
    // Their waits end with them: the events those waits held are judged
    // again (`rejoin`) before the next index sync drops the subscriptions.
    for t in &taken {
        for token in t
            .members
            .iter()
            .cloned()
            .chain(t.intents.iter().map(|id| format!("intent:{id}")))
        {
            if let Some(keys) = s.indexes.run_subscriptions.get(&token) {
                let keys = keys.clone();
                s.replay_touched.wait_keys.extend(keys);
            }
        }
    }
    let mut cohorts: Vec<SettledCohort> = taken
        .iter()
        .map(|t| SettledCohort {
            root: t.root.clone(),
            finished_at_ms: 0,
            runs: vec![],
            intents: vec![],
            receipts: vec![],
        })
        .collect();
    let mut all_intents = BTreeSet::new();
    let mut all_members = BTreeSet::new();
    for (cohort, t) in cohorts.iter_mut().zip(&taken) {
        for id in &t.members {
            if let Some(run) = s.runs.remove(id) {
                cohort.runs.push(run);
            }
            crate::indexes::mark_run(s, id);
            all_members.insert(id.clone());
        }
        for id in &t.intents {
            if let Some(intent) = s.intents.remove(id) {
                cohort.intents.push(intent);
            }
            crate::indexes::mark_intent(s, id);
            all_intents.insert(id.clone());
        }
        s.budgets.remove(&t.root);
    }
    if !all_intents.is_empty() && !s.recovery_requests.is_empty() {
        s.recovery_requests
            .retain(|_, request| !all_intents.contains(&request.intent));
    }
    if !s.waits.is_empty() {
        s.waits.retain(|_, wait| !all_members.contains(&wait.run));
    }
    for receipt in take_receipts(s, &offsets) {
        if let Some(&i) = owner.get(&receipt.run) {
            cohorts[i].receipts.push(receipt);
        }
    }
    for (cohort, t) in cohorts.iter_mut().zip(&taken) {
        cohort.finished_at_ms = t
            .finished_at_ms
            .or_else(|| cohort.receipts.iter().map(|r| r.at_ms).max())
            .or(s.retention.clock_ms)
            .unwrap_or(0);
    }
    Ok(cohorts)
}
/// Move the receipts owned by `owner`, which belongs to no cohort (`"admin"`,
/// `"event:<key>"`), out of the State as a cohort of root `owner` with no
/// runs. `None` when it owns none. O(its receipts) plus compaction.
pub(crate) fn move_unowned_receipts(s: &mut State, owner: &str) -> Result<Option<SettledCohort>> {
    let offsets: BTreeSet<usize> = receipt_offsets(s, owner).into_iter().collect();
    if offsets.is_empty() {
        return Ok(None);
    }
    let receipts = take_receipts(s, &offsets);
    let finished_at_ms = receipts
        .iter()
        .map(|r| r.at_ms)
        .max()
        .or(s.retention.clock_ms)
        .unwrap_or(0);
    Ok(Some(SettledCohort {
        root: owner.into(),
        finished_at_ms,
        runs: vec![],
        intents: vec![],
        receipts,
    }))
}
/// Move the `"admin"` receipts this commit wrote (sequence after `started`,
/// the sequence it began at) to history. Older ones were written by an
/// older engine and leave through the drain, a bounded number per commit.
fn move_admin_receipts(s: &mut State, started: u64, report: &mut CommitReport) -> Result<()> {
    let offsets: BTreeSet<usize> = s
        .indexes
        .receipts
        .get("admin")
        .into_iter()
        .flat_map(|offsets| {
            offsets
                .range((
                    std::ops::Bound::Excluded(started),
                    std::ops::Bound::Unbounded,
                ))
                .map(|(_, offset)| *offset)
        })
        .filter(|&offset| s.receipts.get(offset).is_some_and(|r| r.run == "admin"))
        .collect();
    if offsets.is_empty() {
        return Ok(());
    }
    let receipts = take_receipts(s, &offsets);
    report.receipts += receipts.len();
    let finished_at_ms = receipts
        .iter()
        .map(|r| r.at_ms)
        .max()
        .or(s.retention.clock_ms)
        .unwrap_or(0);
    s.settled.push(SettledCohort {
        root: "admin".into(),
        finished_at_ms,
        runs: vec![],
        intents: vec![],
        receipts,
    });
    Ok(())
}
/// The default of `RetentionPolicy::window_drops_per_commit`: window
/// entries examined (dropped or deferred) per commit, oldest expiry first.
pub const WINDOW_DROPS_PER_COMMIT: usize = 64;
/// The time a window entry written now counts from: the later of the
/// commit's host time and the clock, so a backwards host time never
/// shortens a window.
pub(crate) fn window_start(s: &State, now_ms: i64) -> i64 {
    s.retention
        .clock_ms
        .map_or(now_ms, |clock| clock.max(now_ms))
}
/// Keep `member` (`WINDOW_EVENT` or `WINDOW_ADMISSION` plus its key) in
/// expiry order under `expires_at_ms`.
pub(crate) fn window_insert(s: &mut State, expires_at_ms: i64, member: String) {
    s.retention
        .expiry
        .insert(window_key(expires_at_ms, &member), ());
}
fn window_remove(s: &mut State, expires_at_ms: i64, member: &str) {
    s.retention
        .expiry
        .remove(&window_key(expires_at_ms, member));
}
/// Whether the keyed admission receipt `key` may leave at `clock`: its
/// window has passed and its run is no longer live (SC-10).
pub(crate) fn admission_droppable(s: &State, key: &str, clock: i64) -> bool {
    s.keyed_admissions.get(key).is_some_and(|receipt| {
        admission_expiry(s, key).is_some_and(|at| at <= clock)
            && !s.runs.contains_key(&receipt.run_id)
    })
}
/// Lower the retention clock to `to_ms` (a clock rebase) and cap every
/// recorded expiry later than its [`rebase_cap`] at that: `to_ms` plus the
/// entry's own window. An expiry at or below its cap stays. Walks the expiry
/// order from the smallest possible cap upwards, so the cost is the number
/// of entries above it (at most the remembered window). Returns (events,
/// admissions) capped. The caller writes the [`CLOCK_REBASE`] receipt that
/// licenses the commit.
pub(crate) fn rebase_clock(s: &mut State, to_ms: i64) -> (usize, usize) {
    let policy = &s.retention.policy;
    let lowest = to_ms.saturating_add(
        window(policy.admission_replay_ms).min(window(policy.event_replay_ms).max(1)),
    );
    let mut keys = vec![];
    let mut after = window_key(lowest, "");
    loop {
        let page = s.retention.expiry.keys_after(Some(&after), 256);
        let Some(last) = page.last().cloned() else {
            break;
        };
        keys.extend(page);
        after = last;
    }
    let (mut events, mut admissions) = (0, 0);
    for key in keys {
        let Some((_, member)) = window_entry(&key) else {
            continue;
        };
        let member = member.to_string();
        let (window, item) = if let Some(item) = member.strip_prefix(WINDOW_ADMISSION) {
            (Window::Admission, item)
        } else if let Some(item) = member.strip_prefix(WINDOW_EVENT) {
            (Window::Event, item)
        } else {
            continue;
        };
        let cap = rebase_cap(s, window, item, to_ms);
        let records = match window {
            Window::Admission => &mut s.retention.admission_expires_at_ms,
            Window::Event => &mut s.retention.event_expires_at_ms,
        };
        if records.get(item).is_some_and(|at| *at > cap) {
            records.insert(item.to_string(), cap);
            s.retention.expiry.remove(&key);
            window_insert(s, cap, member.clone());
            match window {
                Window::Admission => admissions += 1,
                Window::Event => events += 1,
            }
        }
    }
    s.retention.clock_ms = Some(to_ms);
    (events, admissions)
}
/// The end of a licensed admin commit (a clock rebase or a floor
/// lowering): only its own `"admin"` receipt (written after `started`)
/// moves to history. The clock is not advanced, nothing settles, nothing is
/// dropped or drained, so the commit is exactly its licence.
pub(crate) fn finish_licensed_commit(s: &mut State, started: u64) -> Result<CommitReport> {
    let mut report = CommitReport::default();
    move_admin_receipts(s, started, &mut report)?;
    Ok(report)
}
/// Remove the keyed admission receipt `key` ahead of its drop, with its
/// expiry record and window member. A receipt is never replaced in place, so
/// an expired key admitted again leaves in a commit of its own first.
pub(crate) fn forget_admission(s: &mut State, key: &str) {
    let at = admission_expiry(s, key);
    remove_admission(s, key);
    if let Some(at) = at {
        window_remove(s, at, &format!("{WINDOW_ADMISSION}{key}"));
    }
}
fn remove_admission(s: &mut State, key: &str) {
    if let Some(receipt) = s.keyed_admissions.remove(key) {
        s.retention
            .replay_order
            .remove(&admission_order_key(&receipt));
        if s.retention.replay_held.get(&receipt.run_id) == Some(&receipt.key) {
            s.retention.replay_held.remove(&receipt.run_id);
        }
    }
    s.retention.admission_expires_at_ms.remove(key);
}
/// Whether an event is still needed past its window (SC-9): a live waiter on
/// one of its correlation keys, a queued destination that its input does not
/// block, or an uncertain completion of its contract and scope.
pub(crate) fn event_held(s: &State, key: &str) -> Result<bool> {
    let Some(event) = s.events.get(key) else {
        return Ok(false);
    };
    let memberships = s.indexes.event_memberships.get(key);
    event_hold(s, key, event, memberships.map(Vec::as_slice), None)
}
/// The waiters and uncertain operations a stored State recorded, which a
/// removal check adds to those of the State it judges: a hold is judged from
/// the live records (wait records, operations), and a commit cannot release
/// one by rewriting a derived index.
pub struct StoredHolds<'a> {
    /// The stored waiters of a correlation key (`indexes.waiters`).
    pub waiters: StoredWaiters<'a>,
    /// The stored uncertain completions by contract and scope.
    pub uncertain: &'a BTreeMap<String, BTreeSet<String>>,
}
/// [`event_held`] for `event` (key `key`, correlation keys `memberships`),
/// judged by the live work of `s`, which need not hold the event itself (a
/// commit that removes it): its `event:<key>` receipts are read from `s` and
/// from the history `s` writes.
fn event_hold(
    s: &State,
    key: &str,
    event: &crate::AcceptedEvent,
    memberships: Option<&[String]>,
    stored: Option<&StoredHolds<'_>>,
) -> Result<bool> {
    if event.destinations.iter().any(|d| d.run.is_none()) {
        let owner = format!("event:{key}");
        let blocked: BTreeSet<String> = receipt_offsets(s, &owner)
            .into_iter()
            .filter_map(|offset| s.receipts.get(offset).cloned())
            .chain(
                s.settled
                    .iter()
                    .filter(|cohort| cohort.root == owner)
                    .flat_map(|cohort| cohort.receipts.iter().cloned()),
            )
            .filter(|r| {
                r.kind == "destination_blocked"
                    && r.detail.get("blocked_kind").and_then(|v| v.as_str()) == Some("input")
            })
            .filter_map(|r| {
                r.detail
                    .get("destination")
                    .and_then(|v| v.as_str())
                    .map(String::from)
            })
            .collect();
        if event
            .destinations
            .iter()
            .any(|d| d.run.is_none() && !blocked.contains(&d.key))
        {
            return Ok(true);
        }
    }
    // A waiter holds the event when its live record still waits on one of
    // the event's correlation keys: a run waiting on a wait record whose key
    // it is, or an unsettled operation waiting for its completion. The
    // candidates come from the waiter index (and the stored one).
    let waiting = |target: &str, k: &str| -> Result<bool> {
        if let Some(intent) = target.strip_prefix("intent:") {
            return Ok(s
                .intents
                .get(intent)
                .is_some_and(|i| !intent_settled(&i.status)));
        }
        let Some(run) = s.runs.get(target) else {
            return Ok(false);
        };
        let wait = match (&run.status, &run.pending) {
            (crate::RunStatus::Waiting, Some(crate::Pending::Event { wait })) => wait,
            _ => return Ok(false),
        };
        let Some(record) = s.waits.get(wait) else {
            return Ok(false);
        };
        let Some(contract) = &record.event else {
            return Ok(false);
        };
        Ok(crate::indexes::correlation_key(contract, &record.scope, &record.correlation)? == k)
    };
    for k in memberships.into_iter().flatten() {
        let mut targets: BTreeSet<String> = s.indexes.waiters.get(k).cloned().unwrap_or_default();
        if let Some(stored) = stored {
            targets.extend((stored.waiters)(k)?.unwrap_or_default());
        }
        for target in &targets {
            if waiting(target, k)? {
                return Ok(true);
            }
        }
    }
    // An operation dispatched without a handle (yet) may complete through
    // any event of its completion contract and scope accepted after its
    // first dispatch.
    let contract = crate::indexes::trigger_key(&event.envelope.contract, &event.envelope.scope)?;
    let mut uncertain: BTreeSet<String> = s
        .indexes
        .uncertain_completion_contracts
        .get(&contract)
        .cloned()
        .unwrap_or_default();
    if let Some(stored) = stored {
        uncertain.extend(stored.uncertain.get(&contract).cloned().unwrap_or_default());
    }
    Ok(uncertain.iter().any(|id| {
        s.intents.get(id).is_some_and(|i| {
            i.command.asynchronous.is_some()
                && crate::indexes::uncertain(i)
                // Its completion is accepted after its (first) dispatch.
                && i.dispatch_sequence.is_some_and(|d| d < event.sequence)
                // Until its command deadline plus the window (ruling C).
                && !crate::indexes::hold_lapsed(s, i)
        })
    }))
}
/// An accepted event's identity (key, sequence, digest, acceptance time
/// and envelope) never changes; only its destinations do (fan-out).
pub fn check_event_change(
    old: &crate::AcceptedEvent,
    new: &crate::AcceptedEvent,
) -> std::result::Result<(), StorageError> {
    if old.key != new.key
        || old.sequence != new.sequence
        || old.digest != new.digest
        || old.accepted_at_ms != new.accepted_at_ms
        || old.envelope != new.envelope
    {
        return Err(refuse(REFUSE_EVENT_CHANGE));
    }
    Ok(())
}
/// Whether event `key` may leave at `clock`: its window has passed and
/// nothing holds it.
pub(crate) fn event_droppable(s: &State, key: &str, clock: i64) -> Result<bool> {
    Ok(event_expiry(s, key).is_some_and(|at| at <= clock) && !event_held(s, key)?)
}
/// Remove event `key` ahead of its drop, with its window member (see
/// [`forget_admission`]).
pub(crate) fn forget_event(s: &mut State, key: &str) -> Result<()> {
    let at = event_expiry(s, key);
    remove_event(s, key, &mut CommitReport::default())?;
    if let Some(at) = at {
        window_remove(s, at, &format!("{WINDOW_EVENT}{key}"));
    }
    Ok(())
}
/// Remove event `key`, its epoch, expiry, correlation memberships and index
/// entries; its `event:<key>` receipts go to history.
fn remove_event(s: &mut State, key: &str, report: &mut CommitReport) -> Result<()> {
    let Some(event) = s.events.remove(key) else {
        return Ok(());
    };
    s.retention.replay_order.remove(&event_order_key(&event));
    crate::indexes::unindex_contract_event(s, &event)?;
    s.retention.event_epochs.remove(key);
    s.retention.event_expires_at_ms.remove(key);
    if let Some(keys) = s.indexes.event_memberships.remove(key) {
        for k in keys {
            let Some(events) = s.indexes.events.get_mut(&k) else {
                continue;
            };
            if events.get(&event.sequence).is_some_and(|id| id == key) {
                events.remove(&event.sequence);
            } else {
                events.retain(|_, id| id != key);
            }
            if events.is_empty() {
                s.indexes.events.remove(&k);
            }
        }
    }
    if let Some(cohort) = move_unowned_receipts(s, &format!("event:{key}"))? {
        report.receipts += cohort.receipts.len();
        s.settled.push(cohort);
    }
    report.events += 1;
    Ok(())
}
/// Drop the window entries whose expiry the clock has reached, oldest first,
/// examining at most `limit` members. An entry still needed is deferred: a
/// keyed receipt of a live run by the admission window, an event (see
/// [`event_held`]) by its window ([`event_window`]), both from the clock, so it neither
/// leaves early nor blocks the entries behind it. Finding the oldest entry
/// is one ordered lookup; the work is O(members examined). A commit without
/// a clock drops nothing.
pub(crate) fn drop_expired(s: &mut State, limit: usize, report: &mut CommitReport) -> Result<()> {
    let Some(clock) = s.retention.clock_ms else {
        return Ok(());
    };
    window_step();
    let mut head = s.retention.expiry.next_key_by_one::<String>(None);
    for _ in 0..limit {
        let Some(key) = head.clone() else {
            break;
        };
        let Some((at, member)) = window_entry(&key) else {
            // Not a window key: it is no entry; take it out of the order.
            s.retention.expiry.remove(&key);
            window_step();
            head = s.retention.expiry.next_key_by_one(Some(&key));
            continue;
        };
        if at > clock {
            break;
        }
        let member = member.to_string();
        s.retention.expiry.remove(&key);
        // Deferred entries go after the clock, so nothing is added behind
        // this cursor.
        window_step();
        head = s.retention.expiry.next_key_by_one(Some(&key));
        window_step();
        if let Some(key) = member.strip_prefix(WINDOW_ADMISSION) {
            drop_admission(s, key, clock, report);
        } else if let Some(key) = member.strip_prefix(WINDOW_EVENT) {
            drop_event(s, key, clock, report)?;
        }
    }
    // Stopped at the bound with an expired entry still at the head.
    if head
        .as_deref()
        .and_then(window_entry)
        .is_some_and(|(at, _)| at <= clock)
    {
        report.more = true;
    }
    Ok(())
}
#[cfg(test)]
thread_local! {
    /// Ordered expiry lookups and members examined by `drop_expired`.
    static WINDOW_STEPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
fn window_step() {
    #[cfg(test)]
    WINDOW_STEPS.with(|steps| steps.set(steps.get() + 1));
}
fn drop_admission(s: &mut State, key: &str, clock: i64, report: &mut CommitReport) {
    let member = || format!("{WINDOW_ADMISSION}{key}");
    let Some(run_id) = s.keyed_admissions.get(key).map(|r| r.run_id.clone()) else {
        return;
    };
    let Some(at) = admission_expiry(s, key) else {
        return;
    };
    if at > clock {
        // Its expiry moved later: keep it in order there.
        window_insert(s, at, member());
        return;
    }
    if s.runs.contains_key(&run_id) {
        let next = clock.saturating_add(window(s.retention.policy.admission_replay_ms));
        s.retention.admission_expires_at_ms.insert(key.into(), next);
        window_insert(s, next, member());
        report.deferred += 1;
        return;
    }
    remove_admission(s, key);
    report.admissions += 1;
}
/// The window of event `key`: `max(policy.event_replay_ms,
/// contract.retention_ms)`, as when it was accepted (the policy's alone if
/// its contract is no longer installed); at least 1 ms.
fn event_window(s: &State, key: &str) -> i64 {
    let window_ms = match s.events.get(key) {
        Some(e) => contract_event_window(s, &e.envelope.contract),
        None => s.retention.policy.event_replay_ms,
    };
    window(window_ms).max(1)
}
/// The window of an event of `contract`: `max(policy.event_replay_ms,
/// contract.retention_ms)`, the policy value alone when the contract is no
/// longer installed. One scan of the installed artifacts.
pub(crate) fn contract_event_window(s: &State, contract: &aq_playbooks_model::ContractRef) -> u64 {
    let contract_ms = crate::find_event(s, contract).map_or(0, |c| c.retention_ms);
    s.retention.policy.event_replay_ms.max(contract_ms)
}
fn drop_event(s: &mut State, key: &str, clock: i64, report: &mut CommitReport) -> Result<()> {
    let member = || format!("{WINDOW_EVENT}{key}");
    if !s.events.contains_key(key) {
        return Ok(());
    }
    let Some(at) = event_expiry(s, key) else {
        return Ok(());
    };
    if at > clock {
        window_insert(s, at, member());
        return Ok(());
    }
    if event_held(s, key)? {
        let next = clock.saturating_add(event_window(s, key));
        s.retention.event_expires_at_ms.insert(key.into(), next);
        window_insert(s, next, member());
        report.deferred += 1;
        return Ok(());
    }
    remove_event(s, key, report)
}
/// Ruling C (Task 10): the uncertain operations whose hold has lapsed (the
/// clock is past their command deadline plus their completion event's
/// window, [`crate::indexes::hold_lapsed`]) leave the uncertain index, so
/// [`rejoin`] judges the events they held again: unheld now, those join
/// the eviction order at their acceptance positions. O(uncertain
/// operations): nothing when no handle is lost.
fn lapse_holds(s: &mut State) -> Result<()> {
    if s.indexes.uncertain_contract_membership.is_empty() {
        return Ok(());
    }
    let lapsed: Vec<String> = s
        .indexes
        .uncertain_contract_membership
        .keys()
        .filter(|id| {
            s.intents
                .get(*id)
                .is_some_and(|i| crate::indexes::hold_lapsed(s, i))
        })
        .cloned()
        .collect();
    if lapsed.is_empty() {
        return Ok(());
    }
    for id in &lapsed {
        crate::indexes::mark_intent(s, id);
    }
    crate::indexes::sync(s)
}
/// The window entries a commit may have changed the hold of, for
/// [`rejoin`]: events it accepted or fanned out, and the correlation keys
/// whose waiters it changed. Never stored.
#[derive(Debug, Clone, Default)]
pub struct ReplayTouched {
    pub events: BTreeSet<String>,
    pub wait_keys: BTreeSet<String>,
    /// Contract and scope keys whose uncertain completions changed, each
    /// with the earliest dispatch sequence concerned: only events accepted
    /// after it are judged again.
    pub contract_keys: BTreeMap<String, u64>,
}
/// Put the window entries whose hold this commit may have ended or begun
/// in or out of the eviction order: a keyed admission receipt joins it when
/// its run leaves the record (in this commit's history); an event is judged
/// again ([`event_held`]) when the commit accepted it, fanned it out, or
/// changed the waiters of one of its correlation keys or the uncertain
/// operations of its contract and scope. O(touched).
///
/// A hold through an uncertain completion (an operation whose admission
/// handle is lost) begins or ends when the uncertain operations of a
/// contract and scope change. Such an operation holds only events accepted
/// after its dispatch, so only those are judged again (from
/// `indexes.contract_events` by sequence): a dispatch judges none, the
/// binding of its handle those that arrived during the host call.
pub(crate) fn rejoin(s: &mut State, report: &mut CommitReport) -> Result<()> {
    let runs: Vec<String> = s
        .settled
        .iter()
        .flat_map(|cohort| cohort.runs.iter().map(|run| run.id.clone()))
        .collect();
    for run in runs {
        let Some(key) = s.retention.replay_held.remove(&run) else {
            continue;
        };
        if let Some(receipt) = s.keyed_admissions.get(&key)
            && receipt.run_id == run
        {
            let order = admission_order_key(receipt);
            s.retention.replay_order.insert(order, ());
        }
    }
    let touched = std::mem::take(&mut s.replay_touched);
    let mut events = touched.events;
    for key in touched.wait_keys {
        if let Some(ids) = s.indexes.events.get(&key) {
            events.extend(ids.values().cloned());
        }
    }
    // A change of a contract's uncertain operations queues its events after
    // the earliest dispatch concerned; at most `window_drops_per_commit` of
    // the queued events are judged per commit, the rest in later commits
    // (an unjudged event that has become held is taken out of the order by
    // `evict` rather than evicted; one that has become unheld waits outside
    // it, or leaves by its window). Events an older engine accepted that the
    // drain has not judged yet are left to it.
    for (key, after) in touched.contract_keys {
        let entry = s.retention.rejudge.entry(key).or_insert(after);
        *entry = (*entry).min(after);
    }
    let mut budget = s.retention.policy.window_drops_per_commit;
    let drain = &s.retention.drain;
    let mut progress = vec![];
    for (key, after) in &s.retention.rejudge {
        if budget == 0 {
            break;
        }
        let batch: Vec<(u64, String)> = s
            .indexes
            .contract_events
            .get(key)
            .into_iter()
            .flat_map(|ids| {
                ids.range((
                    std::ops::Bound::Excluded(*after),
                    std::ops::Bound::Unbounded,
                ))
            })
            .filter(|(sequence, id)| !drain.event_pending(id, **sequence))
            .take(budget)
            .map(|(sequence, id)| (*sequence, id.clone()))
            .collect();
        budget -= batch.len();
        // A batch shorter than the budget reached the contract's last
        // event: done. A full one continues after its last event.
        let next = if budget > 0 {
            None
        } else {
            batch.last().map(|(sequence, _)| *sequence)
        };
        progress.push((key.clone(), next));
        events.extend(batch.into_iter().map(|(_, id)| id));
    }
    for (key, next) in progress {
        match next {
            Some(cursor) => {
                s.retention.rejudge.insert(key, cursor);
            }
            None => {
                s.retention.rejudge.remove(&key);
            }
        }
    }
    if !s.retention.rejudge.is_empty() {
        report.more = true;
    }
    for key in events {
        let Some(order) = s.events.get(&key).map(event_order_key) else {
            continue;
        };
        let held = event_held(s, &key)?;
        let in_order = s.retention.replay_order.contains_key(&order);
        if held && in_order {
            s.retention.replay_order.remove(&order);
        } else if !held && !in_order {
            s.retention.replay_order.insert(order, ());
        }
    }
    Ok(())
}
/// Evict the oldest window entries past `policy.replay_keep`: while the
/// eviction order holds more, take its head (at most `limit` examined). An
/// entry live work holds leaves the order without being evicted (a keyed
/// receipt waits for its run in `replay_held`; an event rejoins when
/// [`rejoin`] judges it unheld). Otherwise the entry is forgotten: its item,
/// expiry record, expiry order key and (for an event) indexes, as when its
/// window passes. Never refuses, never needs host time. Nothing to evict
/// costs one count; k examined cost O(k).
pub(crate) fn evict(s: &mut State, limit: usize, report: &mut CommitReport) -> Result<()> {
    let keep = s.retention.policy.replay_keep;
    if s.retention.replay_order.len() <= keep {
        return Ok(());
    }
    window_step();
    let mut head = s.retention.replay_order.next_key_by_one::<String>(None);
    for _ in 0..limit {
        if s.retention.replay_order.len() <= keep {
            break;
        }
        let Some(key) = head else {
            break;
        };
        s.retention.replay_order.remove(&key);
        window_step();
        head = s.retention.replay_order.next_key_by_one(Some(&key));
        window_step();
        let Some((_, member)) = replay_entry(&key) else {
            continue;
        };
        if let Some(item) = member.strip_prefix(WINDOW_ADMISSION) {
            let Some(receipt) = s.keyed_admissions.get(item).cloned() else {
                continue;
            };
            if admission_order_key(&receipt) != key {
                continue;
            }
            if s.runs.contains_key(&receipt.run_id) {
                s.retention.replay_held.insert(receipt.run_id, item.into());
                continue;
            }
            let at = admission_expiry(s, item);
            remove_admission(s, item);
            if let Some(at) = at {
                window_remove(s, at, member);
            }
            report.evicted += 1;
        } else if let Some(item) = member.strip_prefix(WINDOW_EVENT) {
            let Some(order) = s.events.get(item).map(event_order_key) else {
                continue;
            };
            if order != key || event_held(s, item)? {
                continue;
            }
            let at = event_expiry(s, item);
            let mut removed = CommitReport::default();
            remove_event(s, item, &mut removed)?;
            report.receipts += removed.receipts;
            if let Some(at) = at {
                window_remove(s, at, member);
            }
            report.evicted += 1;
        }
    }
    if s.retention.replay_order.len() > keep {
        report.more = true;
    }
    Ok(())
}
/// The window entries `next` holds in its eviction order that `previous`
/// did not must each be the order key of an item `next` holds and no live
/// work in `next` holds ([`REFUSE_EVICTION`] otherwise), so the order's
/// count, which licenses evictions, counts only real unheld entries.
fn check_order_entry(next: &State, key: &str) -> std::result::Result<(), StorageError> {
    let genuine = match replay_entry(key) {
        Some((_, member)) => {
            if let Some(item) = member.strip_prefix(WINDOW_ADMISSION) {
                next.keyed_admissions.get(item).is_some_and(|r| {
                    admission_order_key(r) == key && !next.runs.contains_key(&r.run_id)
                })
            } else if let Some(item) = member.strip_prefix(WINDOW_EVENT) {
                match next.events.get(item) {
                    Some(e) if event_order_key(e) == key => {
                        !event_held(next, item).map_err(|e| refuse(&e.to_string()))?
                    }
                    _ => false,
                }
            } else {
                false
            }
        }
        None => false,
    };
    if genuine {
        Ok(())
    } else {
        Err(refuse(REFUSE_EVICTION))
    }
}
/// What [`check_evictions`] reads of the stored State.
pub struct Stored<'a> {
    /// Whether the stored eviction order holds `key` (it decides only the
    /// refusal's text).
    pub ordered: &'a dyn Fn(&str) -> std::result::Result<bool, StorageError>,
    /// The stored correlation keys of event `key`.
    pub memberships: &'a dyn Fn(&str) -> std::result::Result<Option<Memberships>, StorageError>,
    /// The stored waiters of a correlation key (`indexes.waiters`).
    pub waiters: StoredWaiters<'a>,
}
/// An event's correlation keys (`indexes.event_memberships`).
pub type Memberships = Vec<String>;
/// The stored waiters of a correlation key (`indexes.waiters`).
pub type StoredWaiters<'a> =
    &'a dyn Fn(&str) -> std::result::Result<Option<BTreeSet<String>>, StorageError>;
/// The evictions of a commit from `previous` to `next`, which a removal
/// check then allows: each keyed admission receipt or event of `previous`
/// that `next` no longer holds, whose expiry the clock has not reached, and
/// whose eviction order key `next` does not hold (it was in the order, or
/// joined and left it in this commit). Such an eviction is licensed only
/// as [`evict`] makes it, else it is refused ([`REFUSE_EVICTION`] when the
/// stored order held it, [`REFUSE_EARLY_REMOVAL`] when not):
///
/// - no live work in `next` holds the entry (a keyed receipt's run is not in
///   `next`; for an event, [`event_held`] judged by `next`);
/// - at most `policy.replay_evictions_per_commit` (of `next`) in the
///   commit, and `next`'s
///   order still holds at least `policy.replay_keep` entries (so the order
///   held more before them);
/// - oldest first: every evicted key is before every key `next`'s order
///   holds.
///
/// `added` are the order keys `next` holds that `previous` did not: each
/// must be genuine ([`check_order_entry`]), so the count is real. A
/// removal before expiry whose order key `next` still holds is no eviction:
/// the removal check refuses it as early. `previous` need hold only the
/// removed items, their expiry records and the stored header. Cost
/// O(evicted + added), plus one ordered lookup.
pub fn check_evictions(
    previous: &State,
    next: &State,
    added: &mut dyn Iterator<Item = String>,
    stored: &Stored<'_>,
) -> std::result::Result<BTreeSet<(Window, String)>, StorageError> {
    // The refusal of an unlicensed eviction: of an entry the order held,
    // or (no eviction at all) an early removal.
    let refusal = |ordered: &str| -> StorageError {
        match (stored.ordered)(ordered) {
            Ok(true) => refuse(REFUSE_EVICTION),
            Ok(false) => refuse(REFUSE_EARLY_REMOVAL),
            Err(error) => error,
        }
    };
    for key in added {
        check_order_entry(next, &key)?;
    }
    let clock = next.retention.clock_ms;
    let order = &next.retention.replay_order;
    let mut evicted = vec![];
    for (key, receipt) in &previous.keyed_admissions {
        if next.keyed_admissions.contains_key(key)
            || admission_expiry(previous, key).is_some_and(|at| expired(at, clock))
        {
            continue;
        }
        let ordered = admission_order_key(receipt);
        if order.contains_key(&ordered) {
            continue;
        }
        if next.runs.contains_key(&receipt.run_id) {
            return Err(refusal(&ordered));
        }
        evicted.push((ordered, Window::Admission, key.clone()));
    }
    for (key, event) in &previous.events {
        if next.events.contains_key(key)
            || event_expiry(previous, key).is_some_and(|at| expired(at, clock))
        {
            continue;
        }
        let ordered = event_order_key(event);
        if order.contains_key(&ordered) {
            continue;
        }
        let memberships = (stored.memberships)(key)?;
        let holds = StoredHolds {
            waiters: stored.waiters,
            uncertain: &previous.indexes.uncertain_completion_contracts,
        };
        if event_hold(next, key, event, memberships.as_deref(), Some(&holds))
            .map_err(|e| refuse(&e.to_string()))?
        {
            return Err(refusal(&ordered));
        }
        evicted.push((ordered, Window::Event, key.clone()));
    }
    if evicted.is_empty() {
        return Ok(BTreeSet::new());
    }
    let first = |evicted: &[(String, Window, String)]| refusal(&evicted[0].0);
    if evicted.len() > next.retention.policy.replay_evictions_per_commit
        || order.len() < next.retention.policy.replay_keep
    {
        return Err(first(&evicted));
    }
    if let Some(head) = order.next_key_by_one::<String>(None)
        && let Some(late) = evicted.iter().find(|(ordered, ..)| *ordered >= head)
    {
        return Err(refusal(&late.0));
    }
    Ok(evicted
        .into_iter()
        .map(|(_, window, key)| (window, key))
        .collect())
}
/// The retention work of every commit that changes the State, after its
/// indexes are synchronized: move the clock to the commit's host time, remove
/// the settled cohorts among `candidates` (see [`candidate_roots`]), drain a
/// bounded amount of what an older engine left ([`drain_step`]), put the
/// window entries whose hold this commit may have changed in or out of the
/// eviction order ([`rejoin`]), drop expired window entries
/// ([`drop_expired`]), evict the oldest entries past `policy.replay_keep`
/// ([`evict`]) and move the `"admin"` receipts this commit wrote (after
/// `started`, the sequence it began at) to history. The clock step and the
/// drop need host time; eviction and the drain do not.
pub(crate) fn finish_commit(
    s: &mut State,
    now_ms: Option<i64>,
    candidates: BTreeSet<String>,
    started: u64,
) -> Result<CommitReport> {
    let mut report = CommitReport::default();
    if let Some(now_ms) = now_ms {
        advance_clock(s, now_ms);
    }
    let settle_bound = s.retention.policy.settle_cohorts_per_commit;
    settle(s, candidates, settle_bound, &mut report)?;
    drain_step(s, started, &mut report)?;
    if !s.retention.drain.done {
        report.more = true;
    }
    lapse_holds(s)?;
    rejoin(s, &mut report)?;
    if now_ms.is_some() {
        let drops = s.retention.policy.window_drops_per_commit;
        drop_expired(s, drops, &mut report)?;
    }
    let evictions = s.retention.policy.replay_evictions_per_commit;
    evict(s, evictions, &mut report)?;
    move_admin_receipts(s, started, &mut report)?;
    Ok(report)
}
/// Drain phase: legacy event tombstones are dropped.
pub const DRAIN_TOMBSTONES: u8 = 0;
/// Drain phase: legacy correlation fences are dropped.
pub const DRAIN_FENCES: u8 = 1;
/// Drain phase: legacy disposal records are dropped.
pub const DRAIN_DISPOSALS: u8 = 2;
/// Drain phase: legacy `"admin"` receipts go to history.
pub const DRAIN_ADMIN: u8 = 3;
/// Drain phase: legacy keyed receipts join the window and eviction order.
pub const DRAIN_KEYED: u8 = 4;
/// Drain phase: legacy events join the window and eviction order.
pub const DRAIN_EVENTS: u8 = 5;
/// Every entry phase is complete.
pub const DRAIN_DONE: u8 = 6;
impl DrainState {
    /// Whether event `key` (accepted at `sequence`) was accepted by an older
    /// engine and the drain has not judged it yet.
    pub fn event_pending(&self, key: &str, sequence: u64) -> bool {
        !self.done
            && self.phase <= DRAIN_EVENTS
            && self
                .legacy_sequence
                .is_some_and(|legacy| sequence <= legacy)
            && (self.phase < DRAIN_EVENTS || self.cursor.as_deref().is_none_or(|c| key > c))
    }
}
#[cfg(test)]
thread_local! {
    /// Calls of `drain_step` that did any work beyond the `done` check.
    static DRAIN_STEPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
/// Move what an older engine left in the record out of it, a bounded amount
/// per commit, in every commit that changes the State (not a licensed
/// one), timed or not; `started` is the sequence the commit began at. Two
/// parts, each with its own bound from the policy:
///
/// - **Cohorts.** The roots after `drain.root_cursor` are examined, at most
///   `policy.drain_cohorts_per_commit`; the settled ones leave as history
///   ([`remove_cohorts`]), so that with those [`settle`] moved the commit
///   moves at most that many. Live ones stay and leave when they settle.
/// - **Entries**, at most `policy.drain_entries_per_commit`, in phases:
///   event tombstones and correlation fences are dropped, disposal records
///   cleared, `"admin"` receipts written before this commit moved to history
///   (each dated by its own time); then each keyed receipt and event gets
///   its expiry record and window key and joins the eviction order unless
///   live work holds it (a keyed receipt of a live run then waits in
///   `replay_held`), and each event joins `indexes.contract_events`.
///
/// When both parts are complete, `drain.done` is set and every later commit
/// costs one check. A [`settle`] beyond its bound re-arms the cohort part;
/// an index rebuild re-arms all of it.
pub(crate) fn drain_step(s: &mut State, started: u64, report: &mut CommitReport) -> Result<()> {
    if s.retention.drain.done {
        return Ok(());
    }
    #[cfg(test)]
    DRAIN_STEPS.with(|steps| steps.set(steps.get() + 1));
    // The cohorts this commit removed leave the root index first, so the
    // scan's bound is not spent on them.
    crate::indexes::sync(s)?;
    if s.retention.drain.legacy_sequence.is_none() {
        s.retention.drain.legacy_sequence = Some(started);
    }
    let cohorts = s.retention.policy.drain_cohorts_per_commit;
    let entries = s.retention.policy.drain_entries_per_commit;
    drain_cohorts(s, cohorts, report)?;
    drain_entries(s, started, entries, report)?;
    let drain = &mut s.retention.drain;
    if drain.roots_done && drain.phase >= DRAIN_DONE {
        drain.done = true;
        drain.root_cursor = None;
        drain.cursor = None;
    }
    Ok(())
}
fn drain_cohorts(s: &mut State, limit: usize, report: &mut CommitReport) -> Result<()> {
    if s.retention.drain.roots_done {
        return Ok(());
    }
    let mut removable = limit.saturating_sub(report.cohorts);
    let mut cursor = s.retention.drain.root_cursor.clone();
    let mut roots = vec![];
    let mut end = false;
    for _ in 0..limit {
        let Some(root) = s.indexes.root_members.next_key(cursor.as_ref()) else {
            end = true;
            break;
        };
        if cohort_settled(s, &root) {
            if removable == 0 {
                break;
            }
            removable -= 1;
            roots.push(root.clone());
        }
        cursor = Some(root);
    }
    take_cohorts(s, &roots, report)?;
    let drain = &mut s.retention.drain;
    drain.root_cursor = cursor;
    drain.roots_done = end;
    Ok(())
}
fn drain_entries(
    s: &mut State,
    started: u64,
    limit: usize,
    report: &mut CommitReport,
) -> Result<()> {
    let mut budget = limit;
    while budget > 0 && s.retention.drain.phase < DRAIN_DONE {
        let used = match s.retention.drain.phase {
            DRAIN_TOMBSTONES => {
                let keys = s
                    .retention
                    .event_tombstones
                    .keys_after::<String>(None, budget);
                for key in &keys {
                    s.retention.event_tombstones.remove(key);
                }
                keys.len()
            }
            DRAIN_FENCES => {
                let keys = s
                    .retention
                    .expired_correlations
                    .keys_after::<String>(None, budget);
                for key in &keys {
                    s.retention.expired_correlations.remove(key);
                }
                keys.len()
            }
            DRAIN_DISPOSALS => {
                let len = s.retention.disposals.len();
                let n = len.min(budget);
                if n > 0 {
                    s.retention.disposals.truncate(len - n);
                }
                n
            }
            DRAIN_ADMIN => drain_admin_receipts(s, started, budget, report),
            DRAIN_KEYED => {
                let cursor = s.retention.drain.cursor.clone();
                let keys = s.keyed_admissions.keys_after(cursor.as_ref(), budget);
                for key in &keys {
                    index_legacy_admission(s, key);
                }
                if let Some(last) = keys.last() {
                    s.retention.drain.cursor = Some(last.clone());
                }
                keys.len()
            }
            _ => {
                let cursor = s.retention.drain.cursor.clone();
                let keys = s.events.keys_after(cursor.as_ref(), budget);
                for key in &keys {
                    index_legacy_event(s, key)?;
                }
                if let Some(last) = keys.last() {
                    s.retention.drain.cursor = Some(last.clone());
                }
                keys.len()
            }
        };
        report.drained += used;
        if used < budget {
            // The phase is exhausted.
            if s.retention.drain.phase == DRAIN_EVENTS {
                s.indexes.contract_events_complete = true;
            }
            let drain = &mut s.retention.drain;
            drain.phase += 1;
            drain.cursor = None;
        }
        budget -= used;
    }
    Ok(())
}
/// Up to `budget` of the `"admin"` receipts written before this commit
/// (sequence at or before `started`), oldest first, each to history as a
/// cohort of its own dated by its own time. Returns how many.
fn drain_admin_receipts(
    s: &mut State,
    started: u64,
    budget: usize,
    report: &mut CommitReport,
) -> usize {
    let offsets: BTreeSet<usize> = s
        .indexes
        .receipts
        .get("admin")
        .into_iter()
        .flat_map(|offsets| offsets.range(..=started).map(|(_, offset)| *offset))
        .filter(|&offset| s.receipts.get(offset).is_some_and(|r| r.run == "admin"))
        .take(budget)
        .collect();
    let receipts = take_receipts(s, &offsets);
    report.receipts += receipts.len();
    let n = receipts.len();
    for receipt in receipts {
        s.settled.push(SettledCohort {
            root: "admin".into(),
            finished_at_ms: receipt.at_ms,
            runs: vec![],
            intents: vec![],
            receipts: vec![receipt],
        });
    }
    n
}
/// A keyed admission receipt an older engine wrote: its expiry record
/// (acceptance plus the admission window) and window key, and its place in
/// the eviction order, or in `replay_held` while its run is live. Each is
/// added only if missing.
fn index_legacy_admission(s: &mut State, key: &str) {
    let Some(receipt) = s.keyed_admissions.get(key).cloned() else {
        return;
    };
    let at = match s.retention.admission_expires_at_ms.get(key) {
        Some(at) => *at,
        None => {
            let at = receipt
                .accepted_at_ms
                .saturating_add(window(s.retention.policy.admission_replay_ms));
            s.retention
                .admission_expires_at_ms
                .insert(key.to_string(), at);
            at
        }
    };
    let member = window_key(at, &format!("{WINDOW_ADMISSION}{key}"));
    if !s.retention.expiry.contains_key(&member) {
        s.retention.expiry.insert(member, ());
    }
    if s.runs.contains_key(&receipt.run_id) {
        if !s.retention.replay_held.contains_key(&receipt.run_id) {
            s.retention
                .replay_held
                .insert(receipt.run_id.clone(), key.to_string());
        }
    } else {
        let order = admission_order_key(&receipt);
        if !s.retention.replay_order.contains_key(&order) {
            s.retention.replay_order.insert(order, ());
        }
    }
}
/// An event an older engine accepted: its expiry record and window key, its
/// `indexes.contract_events` entry, and its place in or out of the eviction
/// order as live work holds it. Each is added only if missing.
fn index_legacy_event(s: &mut State, key: &str) -> Result<()> {
    let Some((sequence, order, contract)) = s.events.get(key).map(|e| {
        (
            e.sequence,
            event_order_key(e),
            crate::indexes::trigger_key(&e.envelope.contract, &e.envelope.scope),
        )
    }) else {
        return Ok(());
    };
    let contract = contract?;
    let Some(at) = event_expiry(s, key) else {
        return Ok(());
    };
    if !s.retention.event_expires_at_ms.contains_key(key) {
        s.retention.event_expires_at_ms.insert(key.to_string(), at);
    }
    let member = window_key(at, &format!("{WINDOW_EVENT}{key}"));
    if !s.retention.expiry.contains_key(&member) {
        s.retention.expiry.insert(member, ());
    }
    if !s
        .indexes
        .contract_events
        .get(&contract)
        .is_some_and(|ids| ids.contains_key(&sequence))
    {
        s.indexes
            .contract_events
            .entry(contract)
            .or_default()
            .insert(sequence, key.to_string());
    }
    let held = event_held(s, key)?;
    let in_order = s.retention.replay_order.contains_key(&order);
    if held && in_order {
        s.retention.replay_order.remove(&order);
    } else if !held && !in_order {
        s.retention.replay_order.insert(order, ());
    }
    Ok(())
}
/// What one `collect_garbage` call did. The call is one ordinary commit
/// (the same bounded settle, drain, window drop and eviction every commit
/// does), plus a bounded page of the artifact phase; nothing requires
/// calling it.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GcReport {
    /// Finished runs this commit moved to history (cohorts that settled,
    /// or that the drain of an older engine's namespace moved). A run never
    /// leaves without its history.
    pub runs: usize,
    /// Events forgotten by the window drop (their window passed).
    pub events: usize,
    /// Artifacts disposed of by the artifact phase.
    pub artifacts: usize,
    /// Receipts moved to history.
    pub receipts: usize,
    /// Artifacts the artifact phase kept because something still references
    /// them. (It counted kept runs and events too before spec 0003; those
    /// are no longer examined, so only artifacts count.)
    pub preserved: usize,
    /// More bounded work is due than this call's bounds allowed: settled
    /// cohorts, expired window entries, entries past `replay_keep`, the
    /// drain of an older engine's namespace, or the artifact phase. Call
    /// again (or let later commits do it).
    pub more: bool,
    /// Keyed admission receipts forgotten by the window drop.
    #[serde(default)]
    pub admissions: usize,
    /// Window entries evicted past `policy.replay_keep`.
    #[serde(default)]
    pub evicted: usize,
}
pub fn artifact_referenced(s: &State, id: &str) -> bool {
    let mut pending = s
        .bindings
        .values()
        .map(|a| a.artifact.clone())
        .collect::<Vec<_>>();
    for bindings in s.catalog_history.values() {
        pending.extend(bindings.values().map(|a| a.artifact.clone()));
    }
    for run in s.runs.values() {
        pending.push(run.artifact.clone());
        pending.extend(run.dependencies.values().cloned());
    }
    for event in s.events.values() {
        for destination in &event.destinations {
            pending.push(destination.artifact.clone());
            pending.extend(destination.dependencies.values().cloned());
        }
    }
    let mut seen = BTreeSet::new();
    while let Some(hash) = pending.pop() {
        if hash == id {
            return true;
        }
        if !seen.insert(hash.clone()) {
            continue;
        }
        if let Some(artifact) = s.artifacts.get(&hash) {
            for dependency in artifact.content.dependencies.values() {
                pending.push(dependency.artifact.clone());
                if let aq_playbooks_model::DependencySelection::Catalog { allowed_artifacts } =
                    &dependency.selection
                {
                    pending.extend(allowed_artifacts.iter().cloned());
                    if let Some(binding) = s.bindings.get(&dependency.binding) {
                        pending.push(binding.artifact.clone());
                    }
                }
            }
        }
    }
    false
}
pub fn verify_artifact_removals(
    previous: &State,
    next: &State,
) -> std::result::Result<(), crate::StorageError> {
    for (id, old) in &previous.artifacts {
        match next.artifacts.get(id) {
            Some(new) if new != old => {
                return Err(crate::StorageError::Backend(
                    "immutable artifact replaced".into(),
                ));
            }
            None if !next.retention.artifact_disposals.contains_key(id)
                || artifact_referenced(next, id) =>
            {
                return Err(crate::StorageError::Backend(
                    "artifact removal lacks safe disposal".into(),
                ));
            }
            _ => {}
        }
    }
    Ok(())
}
/// The artifact phase of `collect_garbage`, run in its commit before the
/// commit's ordinary bounded work ([`finish_commit`]): dispose of up to
/// `limit` unreferenced artifacts (from the stored cursor on), drop catalog
/// history no live run, event or audit window needs, prune artifact
/// disposals past `audit_ms`, and move the GC clock to `now` (never back).
/// It removes no run, operation, receipt or event, and writes no tombstone,
/// correlation fence or disposal record.
pub(crate) fn gc(s: &mut State, now: i64, limit: usize) -> Result<GcReport> {
    let most = s.retention.policy.gc_page_limit;
    if limit == 0 || limit > most {
        return Err(Error::Refused(format!("GC page limit must be 1..{most}")));
    }
    if s.retention
        .last_gc_at_ms
        .is_some_and(|previous| now < previous)
    {
        return Err(Error::Refused("GC clock cannot move backwards".into()));
    }
    s.retention.last_gc_at_ms = Some(now);
    let audit_ms = s.retention.policy.audit_ms;
    let cutoff = now.saturating_sub(i64::try_from(audit_ms).unwrap_or(i64::MAX));
    let mut report = GcReport::default();
    // The cursor of an older engine's GC (its run and event phases come
    // before the artifact phase, 2) starts the artifact phase afresh.
    let mut cursor = s
        .retention
        .gc_cursor
        .as_deref()
        .and_then(|v| serde_json::from_str::<(u8, String, bool)>(v).ok())
        .filter(|(phase, ..)| *phase == GC_ARTIFACTS)
        .map(|(_, cursor, _)| cursor)
        .unwrap_or_default();
    let mut examined = 0;
    let mut finished = false;
    while examined < limit {
        let key = s
            .artifacts
            .range((
                std::ops::Bound::Excluded(cursor.clone()),
                std::ops::Bound::Unbounded,
            ))
            .next()
            .map(|(key, _)| key.clone());
        let Some(id) = key else {
            finished = true;
            break;
        };
        examined += 1;
        cursor = id.clone();
        if artifact_referenced(s, &id) {
            report.preserved += 1;
            continue;
        }
        s.artifacts.remove(&id);
        s.retention.artifact_disposals.insert(id, now);
        report.artifacts += 1;
    }
    report.more = !finished;
    s.retention.gc_cursor = if report.more {
        Some(
            serde_json::to_string(&(GC_ARTIFACTS, cursor, false))
                .map_err(|e| Error::Refused(e.to_string()))?,
        )
    } else {
        None
    };
    let mut revisions = s
        .runs
        .values()
        .map(|r| r.catalog_revision)
        .chain(s.events.values().map(|e| e.catalog_revision))
        .collect::<BTreeSet<_>>();
    revisions.insert(s.catalog_revision);
    for (revision, at_ms) in &s.catalog_history_at_ms {
        if *at_ms >= cutoff {
            revisions.insert(*revision);
        }
    }
    s.catalog_history
        .retain(|revision, _| revisions.contains(revision));
    s.catalog_history_at_ms
        .retain(|revision, _| revisions.contains(revision));
    s.retention.artifact_disposals.retain(|_, at| *at >= cutoff);
    Ok(report)
}
/// The artifact phase's number in the stored GC cursor (an older engine's
/// phases 0 and 1 were its run and event removals).
const GC_ARTIFACTS: u8 = 2;
#[cfg(test)]
mod window_tests {
    use super::*;
    use crate::KeyedAdmissionReceipt;
    /// A plain (format 2) State at clock 1_000 with `due` expired keyed
    /// receipts (each its own expiry) and `kept` unexpired ones.
    fn state(kept: usize, due: usize) -> State {
        let mut s = State::default();
        s.retention.clock_ms = Some(1_000);
        let expiries = (0..due)
            .map(|i| 500 + i as i64)
            .chain((0..kept).map(|i| 2_000 + i as i64));
        for (i, at) in expiries.enumerate() {
            let key = format!("k-{i:05}");
            s.keyed_admissions.insert(
                key.clone(),
                KeyedAdmissionReceipt {
                    key: key.clone(),
                    request_digest: "digest".into(),
                    run_id: format!("run-{i}"),
                    activation_revision: 1,
                    catalog_revision: 1,
                    accepted_at_ms: 0,
                },
            );
            s.retention.admission_expires_at_ms.insert(key.clone(), at);
            window_insert(&mut s, at, format!("{WINDOW_ADMISSION}{key}"));
        }
        s
    }
    fn steps(kept: usize, due: usize) -> (usize, CommitReport) {
        let mut s = state(kept, due);
        let mut report = CommitReport::default();
        WINDOW_STEPS.with(|steps| steps.set(0));
        drop_expired(&mut s, WINDOW_DROPS_PER_COMMIT, &mut report).unwrap();
        assert_eq!(s.keyed_admissions.len(), kept + due - report.admissions);
        (WINDOW_STEPS.with(std::cell::Cell::get), report)
    }
    #[test]
    fn window_work_on_a_plain_state_depends_on_entries_dropped_not_on_entries_kept() {
        // Nothing expired: one ordered lookup, however many entries are kept.
        assert_eq!(steps(10, 0).0, 1);
        assert_eq!(steps(10_000, 0).0, 1);
        // k dropped: one lookup, then one member and one lookup per entry.
        for k in [1, 8, 64] {
            let (work, report) = steps(10_000, k);
            assert_eq!((work, report.admissions), (1 + 2 * k, k), "k = {k}");
        }
        // More than 64 due: still 64.
        let (work, report) = steps(10_000, 200);
        assert_eq!((work, report.admissions), (1 + 2 * 64, 64));
    }
    #[test]
    fn a_member_behind_its_items_expiry_moves_to_it() {
        let mut s = state(0, 1);
        // The record says 2_000; the member is still at 500.
        s.retention
            .admission_expires_at_ms
            .insert("k-00000".into(), 2_000);
        let mut report = CommitReport::default();
        drop_expired(&mut s, WINDOW_DROPS_PER_COMMIT, &mut report).unwrap();
        assert_eq!(report, CommitReport::default());
        assert!(s.keyed_admissions.contains_key("k-00000"));
        assert_eq!(
            s.retention.expiry.into_plain(),
            BTreeMap::from([(window_key(2_000, "k:k-00000"), ())])
        );
    }
    #[test]
    fn window_keys_sort_by_expiry_then_member() {
        let at = [i64::MIN, -1, 0, 1, 999, 1_000, i64::MAX];
        let keys: Vec<String> = at.iter().map(|at| window_key(*at, "k:x")).collect();
        assert!(keys.windows(2).all(|w| w[0] < w[1]), "{keys:?}");
        assert!(window_key(5, "e:z") < window_key(6, "e:a"));
        assert!(window_key(5, "e:a") < window_key(5, "k:a"));
        for at in at {
            assert_eq!(window_entry(&window_key(at, "k:x")), Some((at, "k:x")));
        }
    }
    /// A plain (format 2) State with `unheld` finished keyed receipts in the
    /// eviction order and `replay_keep` `keep`.
    fn ordered(unheld: usize, keep: usize) -> State {
        let mut s = State::default();
        s.retention.clock_ms = Some(1_000);
        s.retention.policy.replay_keep = keep;
        for i in 0..unheld {
            let key = format!("k-{i:05}");
            let receipt = KeyedAdmissionReceipt {
                key: key.clone(),
                request_digest: "digest".into(),
                run_id: format!("run-{:016x}", i + 1),
                activation_revision: 1,
                catalog_revision: 1,
                accepted_at_ms: 0,
            };
            s.retention
                .replay_order
                .insert(admission_order_key(&receipt), ());
            s.keyed_admissions.insert(key.clone(), receipt);
            s.retention
                .admission_expires_at_ms
                .insert(key.clone(), 9_000);
            window_insert(&mut s, 9_000, format!("{WINDOW_ADMISSION}{key}"));
        }
        s
    }
    fn eviction_steps(unheld: usize, keep: usize) -> (usize, CommitReport) {
        let mut s = ordered(unheld, keep);
        let mut report = CommitReport::default();
        WINDOW_STEPS.with(|steps| steps.set(0));
        evict(&mut s, REPLAY_EVICTIONS_PER_COMMIT, &mut report).unwrap();
        assert_eq!(s.keyed_admissions.len(), unheld - report.evicted);
        assert_eq!(s.retention.replay_order.len(), unheld - report.evicted);
        assert_eq!(s.retention.expiry.len(), unheld - report.evicted);
        (WINDOW_STEPS.with(std::cell::Cell::get), report)
    }
    /// Eviction work depends on the entries evicted, not on those kept: at
    /// or under `replay_keep` a commit only counts (no lookup), and k
    /// evictions are one lookup plus two steps each, at most 64.
    #[test]
    fn eviction_work_on_a_plain_state_depends_on_entries_evicted_not_on_entries_kept() {
        assert_eq!(eviction_steps(10, 256).0, 0);
        assert_eq!(eviction_steps(256, 256).0, 0);
        assert_eq!(eviction_steps(5_000, 5_000).0, 0);
        for k in [1, 8, 64] {
            for kept in [10, 5_000] {
                let (work, report) = eviction_steps(kept + k, kept);
                assert_eq!(
                    (work, report.evicted),
                    (1 + 2 * k, k),
                    "k = {k}, kept {kept}"
                );
            }
        }
        let (work, report) = eviction_steps(500, 100);
        assert_eq!((work, report.evicted), (1 + 2 * 64, 64));
    }
    /// The order is one record per entry: a burst in one millisecond adds
    /// records, it does not grow one.
    #[test]
    fn a_same_millisecond_burst_is_one_record_per_entry() {
        let mut s = State::default();
        for i in 0..100 {
            window_insert(&mut s, 7, format!("{WINDOW_EVENT}e-{i:03}"));
        }
        assert_eq!(s.retention.expiry.len(), 100);
        assert!(s.retention.expiry.keys().all(|k| k.len() == 16 + 2 + 5));
    }
    /// A plain State with `n` settled one-run cohorts, indexed.
    fn settled_cohorts(n: usize) -> State {
        let mut s = State::default();
        for i in 0..n {
            let id = format!("run-{i:04}");
            s.sequence += 1;
            s.receipts.push(Receipt {
                sequence: s.sequence,
                run: id.clone(),
                step: "ok".into(),
                at_ms: 1,
                kind: "terminal".into(),
                detail: serde_json::Value::Null,
            });
            s.runs.insert(
                id.clone(),
                Run {
                    catalog_revision: 0,
                    id: id.clone(),
                    root: id.clone(),
                    parent: None,
                    origin: None,
                    artifact: "artifact".into(),
                    entry: "main".into(),
                    dependencies: BTreeMap::new(),
                    authority: crate::Authority {
                        actor: "a".into(),
                        scope: "project".into(),
                        capabilities: BTreeSet::new(),
                    },
                    step: "ok".into(),
                    status: crate::RunStatus::Completed,
                    input: serde_json::Value::Null,
                    variables: BTreeMap::new(),
                    outcome: Some("ok".into()),
                    output: None,
                    reason: None,
                    admitted_at_ms: 1,
                    visits: 1,
                    depth: 0,
                    children: vec![],
                    pending: None,
                    loops: vec![],
                    attempts: BTreeMap::new(),
                    pending_return: None,
                    pending_failure: false,
                    terminal_sequence: Some(s.sequence),
                },
            );
        }
        crate::indexes::rebuild(&mut s).unwrap();
        s
    }
    /// The brief's unit test (Review Focus 2): a commit with more settled
    /// candidates than `COHORTS_PER_COMMIT` re-arms the drain's cohort scan,
    /// which then moves the rest, so none is stranded.
    #[test]
    fn the_settle_bound_comes_from_the_policy() {
        assert_eq!(RetentionPolicy::default().settle_cohorts_per_commit, 50);
        let mut s = settled_cohorts(20);
        s.retention.policy.settle_cohorts_per_commit = 7;
        s.retention.drain = DrainState {
            done: true,
            roots_done: true,
            phase: DRAIN_DONE,
            ..DrainState::default()
        };
        let candidates: BTreeSet<String> = s.runs.keys().cloned().collect();
        let started = s.sequence;
        let report = finish_commit(&mut s, None, candidates, started).unwrap();
        // 7 settled by the commit; the re-armed drain takes its own bound
        // (50, less the 7) of the rest in the same commit.
        assert_eq!((report.cohorts, s.runs.len()), (20, 0));
        let mut s = settled_cohorts(20);
        s.retention.policy.settle_cohorts_per_commit = 7;
        s.retention.policy.drain_cohorts_per_commit = 7;
        s.retention.drain = DrainState {
            done: true,
            roots_done: true,
            phase: DRAIN_DONE,
            ..DrainState::default()
        };
        let candidates: BTreeSet<String> = s.runs.keys().cloned().collect();
        let started = s.sequence;
        let report = finish_commit(&mut s, None, candidates, started).unwrap();
        assert_eq!((report.cohorts, s.runs.len()), (7, 13));
        assert!(report.more);
    }
    #[test]
    fn settle_cap_rearms_the_drain() {
        let mut s = settled_cohorts(60);
        s.retention.drain = DrainState {
            done: true,
            roots_done: true,
            phase: DRAIN_DONE,
            ..DrainState::default()
        };
        let candidates: BTreeSet<String> = s.runs.keys().cloned().collect();
        let mut report = CommitReport::default();
        settle(&mut s, candidates, COHORTS_PER_COMMIT, &mut report).unwrap();
        assert_eq!(s.runs.len(), 10);
        assert!(report.more);
        assert!(!s.retention.drain.done && !s.retention.drain.roots_done);
        assert_eq!(s.retention.drain.root_cursor, None);
        let started = s.sequence;
        drain_step(&mut s, started, &mut CommitReport::default()).unwrap();
        assert_eq!(s.runs.len(), 0);
        assert!(s.retention.drain.done);
    }
    /// A drained namespace pays one check per commit: `drain_step` does no
    /// work, however large the State.
    #[test]
    fn a_drained_namespace_does_no_drain_work() {
        let mut s = settled_cohorts(200);
        s.retention.drain.done = true;
        DRAIN_STEPS.with(|steps| steps.set(0));
        let mut report = CommitReport::default();
        drain_step(&mut s, 0, &mut report).unwrap();
        assert_eq!(DRAIN_STEPS.with(std::cell::Cell::get), 0);
        assert_eq!((report, s.runs.len()), (CommitReport::default(), 200));
        s.retention.drain.done = false;
        drain_step(&mut s, 0, &mut CommitReport::default()).unwrap();
        assert_eq!(DRAIN_STEPS.with(std::cell::Cell::get), 1);
    }
    /// A State written by Task 6's first commit (`expiry` as expiry → set of
    /// members) is refused when decoded, not silently emptied.
    #[test]
    fn the_first_task_6_expiry_shape_is_refused() {
        let old = serde_json::json!({"1000": ["k:x"]});
        assert!(serde_json::from_value::<LazyMap<String, ()>>(old).is_err());
    }
}
