//! Versioned transition capability `transition_v1` (D10, PB-04).
//!
//! A backend that implements the capability runs the engine's transition plan
//! against a State whose retained-history containers are bound to the
//! backend's authoritative records (see [`crate::lazy`]), keeps an exact count
//! of each container's records, and commits only the touched records in one
//! atomic revision. Backends without the capability return
//! [`TransitionOutcome::Unsupported`] before running the plan, and the engine
//! then uses the full `load`/`compare_and_swap` path unchanged.
use crate::lazy::{
    Changes, ContainerChange, ContainerPath, LazyField, RecordSource, Shape, TreeKey, header_bytes,
};
use crate::storage::{Storage, StorageError, VersionedState};
use crate::{Error, Result, State};
use std::collections::BTreeMap;
use std::panic::Location;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionOutcome {
    /// Capability unavailable for this namespace; the plan was not run.
    Unsupported,
    /// The plan made no change; nothing was committed.
    Unchanged,
    Committed {
        revision: u64,
    },
}
pub struct TransitionRequest<'a> {
    pub operation: &'static str,
    /// Runs the engine transition on the bound State; returns whether it changed.
    pub plan: &'a mut dyn FnMut(&mut State) -> Result<bool>,
}

impl State {
    /// Every retained-history container, by schema path. The list (with each
    /// field's [`Shape`]) is the schema certificate a record backend stores.
    pub fn lazy_fields(&mut self) -> Vec<(&'static str, &mut dyn LazyField)> {
        let i = &mut self.indexes;
        let r = &mut self.retention;
        vec![
            ("keyed_admissions", &mut self.keyed_admissions),
            ("runs", &mut self.runs),
            ("receipts", &mut self.receipts),
            ("intents", &mut self.intents),
            ("waits", &mut self.waits),
            ("events", &mut self.events),
            ("budgets", &mut self.budgets),
            ("indexes.root_members", &mut i.root_members),
            ("indexes.root_active", &mut i.root_active),
            ("indexes.root_effects", &mut i.root_effects),
            ("indexes.root_intents", &mut i.root_intents),
            ("indexes.root_last_activity", &mut i.root_last_activity),
            ("indexes.run_roots", &mut i.run_roots),
            ("indexes.intent_roots", &mut i.intent_roots),
            ("indexes.effects", &mut i.effects),
            ("indexes.effect_memberships", &mut i.effect_memberships),
            ("indexes.timers", &mut i.timers),
            ("indexes.due", &mut i.due),
            ("indexes.run_timer", &mut i.run_timer),
            ("indexes.events", &mut i.events),
            ("indexes.event_memberships", &mut i.event_memberships),
            ("indexes.contract_events", &mut i.contract_events),
            ("indexes.waiters", &mut i.waiters),
            ("indexes.run_subscriptions", &mut i.run_subscriptions),
            ("indexes.dependents", &mut i.dependents),
            ("indexes.run_dependencies", &mut i.run_dependencies),
            ("indexes.runs", &mut i.runs),
            ("indexes.receipts", &mut i.receipts),
            ("indexes.all_receipts", &mut i.all_receipts),
            ("retention.event_epochs", &mut r.event_epochs),
            ("retention.event_expires_at_ms", &mut r.event_expires_at_ms),
            (
                "retention.expired_correlations",
                &mut r.expired_correlations,
            ),
            ("retention.event_tombstones", &mut r.event_tombstones),
            ("retention.disposals", &mut r.disposals),
            ("retention.expiry", &mut r.expiry),
            (
                "retention.admission_expires_at_ms",
                &mut r.admission_expires_at_ms,
            ),
            ("retention.replay_order", &mut r.replay_order),
            ("retention.replay_held", &mut r.replay_held),
        ]
    }
}
/// The schema certificate: lazy field paths and their container shapes.
pub fn schema() -> Vec<(&'static str, Shape)> {
    let mut state = State::default();
    state
        .lazy_fields()
        .into_iter()
        .map(|(name, field)| (name, field.shape()))
        .collect()
}
pub fn shape_of(field: &str) -> Option<Shape> {
    schema()
        .into_iter()
        .find(|(name, _)| *name == field)
        .map(|(_, shape)| shape)
}

/// Header bytes plus per-container changes for one commit (or an export).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatePatch {
    pub header: Vec<u8>,
    pub changes: Changes,
}
fn backend(e: impl std::fmt::Display) -> Error {
    Error::Storage(StorageError::Backend(e.to_string()))
}
/// Deserialize a header (lazy fields empty) without binding.
pub fn header_state(header: &[u8]) -> Result<State> {
    serde_json::from_slice(header).map_err(|e| backend(format!("state header: {e}")))
}
/// Deserialize a header and bind every lazy container to `source`.
pub fn bind_state(header: &[u8], source: Arc<dyn RecordSource>) -> Result<State> {
    let mut state = header_state(header)?;
    for (name, field) in state.lazy_fields() {
        field.bind_field(source.clone(), name);
    }
    Ok(state)
}
/// Header plus container changes. For a plain State every container is a
/// complete replacement, so this is also the full export of a State.
pub fn take_patch(state: &mut State) -> Result<StatePatch> {
    let header = header_bytes(&*state).map_err(backend)?;
    let mut changes = Vec::new();
    for (name, field) in state.lazy_fields() {
        field.field_changes(name, &mut changes).map_err(backend)?;
    }
    Ok(StatePatch { header, changes })
}
/// Rebuild a complete State from a header and each container's records.
pub fn assemble_state(
    header: &[u8],
    mut records: impl FnMut(&'static str) -> Result<Vec<(TreeKey, Vec<u8>)>>,
) -> Result<State> {
    let mut value: serde_json::Value =
        serde_json::from_slice(header).map_err(|e| backend(format!("state header: {e}")))?;
    for (field, shape) in schema() {
        let rows = records(field)?;
        let container = match shape {
            Shape::Map(_) | Shape::Nested(..) => {
                let mut map = serde_json::Map::new();
                for (key, bytes) in rows {
                    let text = match key {
                        TreeKey::U64(v) => v.to_string(),
                        TreeKey::I64(v) => v.to_string(),
                        TreeKey::Str(s) => s,
                    };
                    map.insert(text, serde_json::from_slice(&bytes).map_err(backend)?);
                }
                serde_json::Value::Object(map)
            }
            Shape::Set(_) | Shape::Seq => serde_json::Value::Array(
                rows.into_iter()
                    .map(|(_, bytes)| serde_json::from_slice(&bytes).map_err(backend))
                    .collect::<Result<_>>()?,
            ),
        };
        let mut slot = &mut value;
        let mut parts = field.split('.').peekable();
        while let Some(part) = parts.next() {
            let object = slot
                .as_object_mut()
                .ok_or_else(|| backend(format!("state header: {field} parent is not an object")))?;
            if parts.peek().is_none() {
                object.insert(part.to_string(), container);
                break;
            }
            slot = object
                .entry(part.to_string())
                .or_insert_with(|| serde_json::Value::Object(Default::default()));
        }
    }
    serde_json::from_value(value).map_err(|e| backend(format!("state assemble: {e}")))
}

/// The number of records of each container of one namespace revision
/// (what a bound container's `len` reads), and the header's length.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Accounting {
    pub header_len: usize,
    pub fields: BTreeMap<&'static str, usize>,
}
impl Accounting {
    /// Records of `field`.
    pub fn count(&self, field: &str) -> usize {
        self.fields.get(field).copied().unwrap_or(0)
    }
    /// Account one record write: whether the record existed before and
    /// whether it exists after.
    pub fn record(&mut self, field: &'static str, old: bool, new: bool) -> Result<()> {
        shape_of(field).ok_or_else(|| backend(format!("unknown field {field}")))?;
        let count = self.fields.entry(field).or_default();
        *count = (*count + usize::from(new))
            .checked_sub(usize::from(old))
            .ok_or_else(|| backend(format!("{field}: record count underflow")))?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// In-memory record backend (reference/test double for the capability).

type Container = BTreeMap<TreeKey, Vec<u8>>;
#[derive(Debug, Clone, Default)]
struct RecordData {
    header: Vec<u8>,
    containers: BTreeMap<&'static str, Container>,
}
impl RecordData {
    fn container(&self, field: &str) -> Option<&Container> {
        self.containers.get(field)
    }
}
#[derive(Debug, Default)]
struct RecordNamespace {
    owner: Option<String>,
    revision: u64,
    data: Arc<RecordData>,
    accounting: Accounting,
    history: crate::history::HistoryRows,
}
/// Observability for the in-memory record backend.
#[derive(Debug, Default)]
pub struct RecordStats {
    pub full_loads: AtomicUsize,
    pub full_commits: AtomicUsize,
    pub transitions: AtomicUsize,
    pub record_reads: AtomicUsize,
    pub fallbacks: Mutex<Vec<String>>,
}
impl RecordStats {
    pub fn fallbacks(&self) -> Vec<String> {
        self.fallbacks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}
/// Record-addressed in-memory storage implementing `transition_v1`.
#[derive(Clone, Default)]
pub struct RecordStorage {
    inner: Arc<Mutex<BTreeMap<String, RecordNamespace>>>,
    pub stats: Arc<RecordStats>,
    unsupported: bool,
}
impl RecordStorage {
    pub fn new() -> Self {
        Self::default()
    }
    /// Same records, but the capability reports `Unsupported`.
    pub fn without_transition() -> Self {
        Self {
            unsupported: true,
            ..Self::default()
        }
    }
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, BTreeMap<String, RecordNamespace>>> {
        self.inner.lock().map_err(backend)
    }
}
struct TxSource {
    data: Arc<RecordData>,
    stats: Arc<RecordStats>,
    poison: Mutex<Option<String>>,
}
impl TxSource {
    fn container(&self, path: &ContainerPath) -> std::result::Result<Option<&Container>, String> {
        if path.outer.is_some() {
            return Err(format!(
                "{path:?}: nested containers are not in this schema"
            ));
        }
        Ok(self.data.container(path.field))
    }
    fn check(&self) -> Result<()> {
        match self
            .poison
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            Some(e) => Err(backend(format!("transition source failed: {e}"))),
            None => Ok(()),
        }
    }
}
impl RecordSource for TxSource {
    fn get(
        &self,
        path: &ContainerPath,
        key: &TreeKey,
    ) -> std::result::Result<Option<Vec<u8>>, String> {
        self.stats.record_reads.fetch_add(1, Ordering::Relaxed);
        Ok(self.container(path)?.and_then(|c| c.get(key)).cloned())
    }
    fn after(
        &self,
        path: &ContainerPath,
        after: Option<&TreeKey>,
        limit: usize,
    ) -> std::result::Result<Vec<(TreeKey, Vec<u8>)>, String> {
        use std::ops::Bound::{Excluded, Unbounded};
        let Some(c) = self.container(path)? else {
            // An empty page costs one read, as an absent record does.
            self.stats.record_reads.fetch_add(1, Ordering::Relaxed);
            return Ok(Vec::new());
        };
        let rows: Vec<_> = match after {
            Some(a) => c.range((Excluded(a.clone()), Unbounded)),
            None => c.range::<TreeKey, _>(..),
        }
        .take(limit)
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
        self.stats
            .record_reads
            .fetch_add(rows.len().max(1), Ordering::Relaxed);
        Ok(rows)
    }
    fn len(&self, path: &ContainerPath) -> std::result::Result<usize, String> {
        Ok(self.container(path)?.map_or(0, |c| c.len()))
    }
    fn scan(&self, path: &ContainerPath) -> std::result::Result<Vec<(TreeKey, Vec<u8>)>, String> {
        Ok(self
            .container(path)?
            .map(|c| c.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default())
    }
    fn fallback(&self, path: &ContainerPath, at: &'static Location<'static>) {
        self.stats
            .fallbacks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(format!("{} at {}:{}", path.field, at.file(), at.line()));
    }
    fn fail(&self, error: String) {
        self.poison
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert(error);
    }
}
fn field_name(path: &ContainerPath) -> Result<&'static str> {
    if path.outer.is_some() {
        return Err(backend("nested container change outside schema"));
    }
    Ok(path.field)
}
/// Apply a patch to record data and accounting, after validation.
fn stage(data: &RecordData, accounting: &Accounting, patch: &StatePatch) -> Result<Accounting> {
    let mut next = accounting.clone();
    next.header_len = patch.header.len();
    for (path, change) in &patch.changes {
        let field = field_name(path)?;
        let old = data.container(field);
        match change {
            ContainerChange::Entries(rows) => {
                for (key, new) in rows {
                    let before = old.is_some_and(|c| c.contains_key(key));
                    next.record(field, before, new.is_some())?;
                }
            }
            ContainerChange::Replace(rows) => {
                for _ in old.into_iter().flatten() {
                    next.record(field, true, false)?;
                }
                for _ in rows {
                    next.record(field, false, true)?;
                }
            }
        }
    }
    Ok(next)
}
fn apply(data: &mut RecordData, patch: StatePatch) -> Result<()> {
    data.header = patch.header;
    for (path, change) in patch.changes {
        let field = field_name(&path)?;
        let container = data.containers.entry(field).or_default();
        match change {
            ContainerChange::Entries(rows) => {
                for (key, new) in rows {
                    match new {
                        Some(bytes) => container.insert(key, bytes),
                        None => container.remove(&key),
                    };
                }
            }
            ContainerChange::Replace(rows) => *container = rows.into_iter().collect(),
        }
        if container.is_empty() {
            data.containers.remove(field);
        }
    }
    Ok(())
}
/// An accepted event's identity never changes
/// ([`crate::retention::check_event_change`]): each stored event the patch
/// rewrites is compared with what it writes. O(rewritten events).
fn verify_events(data: &RecordData, patch: &StatePatch) -> Result<()> {
    let Some(old) = data.container("events") else {
        return Ok(());
    };
    let decode = |bytes: &[u8]| -> Result<crate::AcceptedEvent> {
        serde_json::from_slice(bytes).map_err(|e| backend(format!("events record: {e}")))
    };
    for (path, change) in &patch.changes {
        if path.field != "events" {
            continue;
        }
        let written: Vec<(&TreeKey, &Vec<u8>)> = match change {
            ContainerChange::Entries(rows) => rows
                .iter()
                .filter_map(|(k, v)| v.as_ref().map(|v| (k, v)))
                .collect(),
            ContainerChange::Replace(rows) => rows.iter().map(|(k, v)| (k, v)).collect(),
        };
        for (key, new) in written {
            if let Some(before) = old.get(key)
                && before != new
            {
                crate::retention::check_event_change(&decode(before)?, &decode(new)?)?;
            }
        }
    }
    Ok(())
}
/// Keyed admission receipts are never rebound. Their removal is checked by
/// [`verify_patch_removals`].
fn verify_keyed_admissions(data: &RecordData, patch: &StatePatch) -> Result<()> {
    let refuse = || {
        Error::Storage(StorageError::Backend(
            "immutable keyed admission receipt removed or changed".into(),
        ))
    };
    let Some(old) = data.container("keyed_admissions") else {
        return Ok(());
    };
    for (path, change) in &patch.changes {
        if path.field != "keyed_admissions" {
            continue;
        }
        match change {
            ContainerChange::Entries(rows) => {
                for (key, new) in rows {
                    if let (Some(before), Some(new)) = (old.get(key), new)
                        && new != before
                    {
                        return Err(refuse());
                    }
                }
            }
            ContainerChange::Replace(rows) => {
                if rows
                    .iter()
                    .any(|(k, v)| old.get(k).is_some_and(|before| before != v))
                {
                    return Err(refuse());
                }
            }
        }
    }
    Ok(())
}
/// A touched record key with its stored value and the value a patch writes.
type Touch<'a> = (&'a TreeKey, Option<&'a Vec<u8>>, Option<&'a Vec<u8>>);
/// Incremental equivalent of [`crate::retention::verify_removals`]: the same
/// rules over the records `patch` deletes from `data` (and the expiry records
/// it writes), checked against the bound `next`. Reads only the touched
/// records, the operations and children of deleted runs, and (for a legacy
/// run disposal) the disposal records.
fn verify_patch_removals(data: &RecordData, patch: &StatePatch, next: &State) -> Result<()> {
    use crate::retention::Window;
    use std::collections::BTreeSet;
    const FENCED: &[&str] = &["runs", "intents", "events", "keyed_admissions"];
    let decode = |field: &str, bytes: &[u8]| -> Result<serde_json::Value> {
        serde_json::from_slice(bytes).map_err(|e| backend(format!("{field} record: {e}")))
    };
    let at = |field: &str, bytes: &[u8]| -> Result<i64> {
        serde_json::from_slice(bytes).map_err(|e| backend(format!("{field} record: {e}")))
    };
    // A licensed admin commit changes no header field its licence does not
    // name, and adds no run, operation, event or keyed receipt. Its caps are
    // computed from the stored header and the stored events.
    let licensed = crate::retention::licence(next)?.is_some();
    let mut caps = State::default();
    if licensed {
        caps = header_state(&data.header)?;
        crate::retention::check_licensed_header(&caps, next)?;
        for (path, change) in &patch.changes {
            let field = field_name(path)?;
            if !FENCED.contains(&field) {
                continue;
            }
            let old = data.container(field);
            let added = match change {
                ContainerChange::Entries(rows) => rows
                    .iter()
                    .any(|(key, new)| new.is_some() && old.and_then(|c| c.get(key)).is_none()),
                ContainerChange::Replace(rows) => rows
                    .iter()
                    .any(|(key, _)| old.and_then(|c| c.get(key)).is_none()),
            };
            if added {
                return Err(backend(crate::retention::REFUSE_LICENCE));
            }
        }
        if let Some(stored) = data.container("events") {
            for (path, change) in &patch.changes {
                if field_name(path)? != "retention.event_expires_at_ms" {
                    continue;
                }
                let keys: Vec<&TreeKey> = match change {
                    ContainerChange::Entries(rows) => rows.iter().map(|(k, _)| k).collect(),
                    ContainerChange::Replace(rows) => rows.iter().map(|(k, _)| k).collect(),
                };
                for key in keys {
                    if let (TreeKey::Str(id), Some(bytes)) = (key, stored.get(key)) {
                        let event: crate::AcceptedEvent = serde_json::from_slice(bytes)
                            .map_err(|e| backend(format!("events record: {e}")))?;
                        caps.events.insert(id.clone(), event);
                    }
                }
            }
        }
    }
    let mut removed: BTreeMap<&str, serde_json::Map<String, serde_json::Value>> = BTreeMap::new();
    let mut removed_receipts: Vec<serde_json::Value> = vec![];
    for (path, change) in &patch.changes {
        let field = field_name(path)?;
        let old = data.container(field);
        // Each touched key: its stored value and the value the patch writes.
        let touched: Vec<Touch<'_>> = match change {
            ContainerChange::Entries(rows) => rows
                .iter()
                .map(|(key, new)| (key, old.and_then(|c| c.get(key)), new.as_ref()))
                .collect(),
            ContainerChange::Replace(rows) => {
                let new: BTreeMap<&TreeKey, &Vec<u8>> = rows.iter().map(|(k, v)| (k, v)).collect();
                let keys: BTreeSet<&TreeKey> = old
                    .into_iter()
                    .flatten()
                    .map(|(k, _)| k)
                    .chain(new.keys().copied())
                    .collect();
                keys.into_iter()
                    .map(|key| (key, old.and_then(|c| c.get(key)), new.get(key).copied()))
                    .collect()
            }
        };
        let window = match field {
            "retention.event_expires_at_ms" => Some(Window::Event),
            "retention.admission_expires_at_ms" => Some(Window::Admission),
            _ => None,
        };
        if let Some(window) = window {
            for (key, before, after) in touched {
                let TreeKey::Str(id) = key else {
                    return Err(backend(format!("{field}: unexpected record key")));
                };
                let before = before.map(|b| at(field, b)).transpose()?;
                let after = after.map(|b| at(field, b)).transpose()?;
                if before != after {
                    crate::retention::check_expiry_record(&caps, next, window, id, before, after)?;
                }
            }
        } else if field == "receipts" {
            // Positional: a receipt is removed when its sequence was at a
            // touched position and the patch writes it nowhere.
            let mut written = BTreeSet::new();
            let mut stored = vec![];
            for (_, before, after) in touched {
                if let Some(after) = after {
                    written.insert(decode(field, after)?["sequence"].as_u64());
                }
                if let Some(before) = before {
                    stored.push(decode(field, before)?);
                }
            }
            removed_receipts.extend(
                stored
                    .into_iter()
                    .filter(|r| !written.contains(&r["sequence"].as_u64())),
            );
        } else if FENCED.contains(&field) {
            for (key, before, after) in touched {
                let (Some(before), None) = (before, after) else {
                    continue;
                };
                let TreeKey::Str(id) = key else {
                    return Err(backend(format!("{field}: unexpected record key")));
                };
                removed
                    .entry(field)
                    .or_default()
                    .insert(id.clone(), decode(field, before)?);
            }
        }
    }
    // The State header (policy, clock, artifacts) plus the deleted records.
    let mut previous = header_state(&data.header)?;
    if !removed.is_empty() || !removed_receipts.is_empty() {
        let expires = |field: &str, key: &str| -> Result<Option<serde_json::Value>> {
            data.container(field)
                .and_then(|c| c.get(&TreeKey::Str(key.to_string())))
                .map(|bytes| decode(field, bytes))
                .transpose()
        };
        let mut value = serde_json::to_value(&previous).map_err(backend)?;
        for (field, records) in &removed {
            let expiry_field = match *field {
                "events" => Some(("event_expires_at_ms", "retention.event_expires_at_ms")),
                "keyed_admissions" => Some((
                    "admission_expires_at_ms",
                    "retention.admission_expires_at_ms",
                )),
                _ => None,
            };
            if let Some((name, path)) = expiry_field {
                for key in records.keys() {
                    if let Some(at) = expires(path, key)? {
                        value["retention"][name][key] = at;
                    }
                }
            }
            value[*field] = serde_json::Value::Object(records.clone());
        }
        value["receipts"] = serde_json::Value::Array(removed_receipts);
        if let Some(runs) = removed.get("runs") {
            // Children of removed runs, and every operation of a removed run.
            let mut all_runs = runs.clone();
            let stored = data.container("runs");
            for run in runs.values() {
                for child in run["children"].as_array().into_iter().flatten() {
                    let Some(child) = child.as_str() else {
                        continue;
                    };
                    if let Some(bytes) = stored.and_then(|c| c.get(&TreeKey::Str(child.into()))) {
                        all_runs.insert(child.into(), decode("runs", bytes)?);
                    }
                }
            }
            value["runs"] = serde_json::Value::Object(all_runs);
            let mut intents = removed.get("intents").cloned().unwrap_or_default();
            for (key, bytes) in data.container("intents").into_iter().flatten() {
                let intent = decode("intents", bytes)?;
                if intent["run"]
                    .as_str()
                    .is_some_and(|run| runs.contains_key(run))
                    && let TreeKey::Str(id) = key
                {
                    intents.insert(id.clone(), intent);
                }
            }
            value["intents"] = serde_json::Value::Object(intents);
        }
        previous =
            serde_json::from_value(value).map_err(|e| backend(format!("removed records: {e}")))?;
    }
    // Evictions: the order keys the patch adds, and the stored correlation
    // keys of the events it removes.
    let stored_order = data.container("retention.replay_order");
    let mut added = vec![];
    for (path, change) in &patch.changes {
        if path.field != "retention.replay_order" {
            continue;
        }
        let written: Vec<&TreeKey> = match change {
            ContainerChange::Entries(rows) => rows
                .iter()
                .filter(|(_, new)| new.is_some())
                .map(|(key, _)| key)
                .collect(),
            ContainerChange::Replace(rows) => rows.iter().map(|(key, _)| key).collect(),
        };
        for key in written {
            if stored_order.is_none_or(|c| !c.contains_key(key))
                && let TreeKey::Str(key) = key
            {
                added.push(key.clone());
            }
        }
    }
    let memberships = |key: &str| -> std::result::Result<Option<Vec<String>>, StorageError> {
        data.container("indexes.event_memberships")
            .and_then(|c| c.get(&TreeKey::Str(key.to_string())))
            .map(|bytes| {
                serde_json::from_slice(bytes)
                    .map_err(|e| StorageError::Backend(format!("event memberships record: {e}")))
            })
            .transpose()
    };
    let evicted = crate::retention::check_evictions(
        &previous,
        next,
        &mut added.into_iter(),
        &crate::retention::Stored {
            ordered: &|key| {
                Ok(stored_order.is_some_and(|c| c.contains_key(&TreeKey::Str(key.to_string()))))
            },
            memberships: &memberships,
            waiters: &|key| {
                data.container("indexes.waiters")
                    .and_then(|c| c.get(&TreeKey::Str(key.to_string())))
                    .map(|bytes| {
                        serde_json::from_slice(bytes)
                            .map_err(|e| StorageError::Backend(format!("waiters record: {e}")))
                    })
                    .transpose()
            },
        },
    )?;
    // `previous` holds only removed receipts.
    crate::retention::check_removals(&previous, next, &|_| false, &|window, key| {
        evicted.contains(&(window, key.to_string()))
    })?;
    Ok(())
}
fn assemble(data: &RecordData) -> Result<State> {
    if data.header.is_empty() {
        return Ok(State::default());
    }
    assemble_state(&data.header, |field| {
        Ok(data
            .container(field)
            .map(|c| c.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default())
    })
}
fn export(state: &State) -> Result<(RecordData, Accounting)> {
    let mut plain = state.clone();
    let patch = take_patch(&mut plain)?;
    let empty = RecordData::default();
    let accounting = stage(&empty, &Accounting::default(), &patch)?;
    let mut data = RecordData::default();
    apply(&mut data, patch)?;
    Ok((data, accounting))
}
impl RecordStorage {
    /// Maintained record counts of `namespace`.
    pub fn accounting(&self, namespace: &str) -> Result<Accounting> {
        Ok(self
            .lock()?
            .get(namespace)
            .map(|n| n.accounting.clone())
            .unwrap_or_default())
    }
}
impl Storage for RecordStorage {
    fn persists_history(&self) -> bool {
        true
    }
    fn history_page(
        &self,
        namespace: &str,
        query: &crate::InspectionQuery,
    ) -> std::result::Result<Vec<crate::HistoryItem>, StorageError> {
        let guard = self
            .inner
            .lock()
            .map_err(|e| StorageError::Backend(e.to_string()))?;
        match guard.get(namespace) {
            Some(ns) => crate::history::page(&ns.history, query),
            None => query.validate().map(|()| vec![]),
        }
    }
    fn delete_history(
        &self,
        namespace: &str,
        owner: &str,
        before_ms: i64,
    ) -> std::result::Result<u64, StorageError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| StorageError::Backend(e.to_string()))?;
        let ns = guard
            .get_mut(namespace)
            .filter(|ns| ns.owner.as_deref() == Some(owner))
            .ok_or(StorageError::Ownership)?;
        Ok(crate::history::delete_before(&mut ns.history, before_ms))
    }
    fn load(&self, namespace: &str) -> std::result::Result<VersionedState, StorageError> {
        self.stats.full_loads.fetch_add(1, Ordering::Relaxed);
        let guard = self
            .inner
            .lock()
            .map_err(|e| StorageError::Backend(e.to_string()))?;
        let Some(ns) = guard.get(namespace) else {
            return Ok(VersionedState::default());
        };
        let state = assemble(&ns.data).map_err(|e| match e {
            Error::Storage(s) => s,
            other => StorageError::Backend(other.to_string()),
        })?;
        Ok(VersionedState {
            revision: ns.revision,
            state,
        })
    }
    fn compare_and_swap(
        &self,
        namespace: &str,
        owner: &str,
        expected_revision: u64,
        state: &State,
    ) -> std::result::Result<u64, StorageError> {
        let to_storage = |e: Error| match e {
            Error::Storage(s) => s,
            other => StorageError::Backend(other.to_string()),
        };
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| StorageError::Backend(e.to_string()))?;
        let ns = guard.entry(namespace.into()).or_default();
        if ns.owner.as_deref() != Some(owner) {
            return Err(StorageError::Ownership);
        }
        if ns.revision != expected_revision {
            return Err(StorageError::Conflict);
        }
        let previous = assemble(&ns.data).map_err(to_storage)?;
        crate::retention::verify_removals(&previous, state)?;
        let revision = expected_revision
            .checked_add(1)
            .ok_or_else(|| StorageError::Backend("revision exhausted".into()))?;
        let items = crate::history_items(&state.settled);
        crate::history::check_new(&ns.history, &items)?;
        crate::history::check_not_live(state, &items)?;
        crate::history::check_unrecorded(
            &ns.history,
            &crate::history::new_live_keys(&previous, state),
        )?;
        // `settled` is not part of the stored header (serde skip).
        let (data, accounting) = export(state).map_err(to_storage)?;
        crate::history::insert(&mut ns.history, items, &state.retention.policy);
        ns.data = Arc::new(data);
        ns.accounting = accounting;
        ns.revision = revision;
        self.stats.full_commits.fetch_add(1, Ordering::Relaxed);
        Ok(revision)
    }
    fn acquire(&self, namespace: &str, owner: &str) -> std::result::Result<(), StorageError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| StorageError::Backend(e.to_string()))?;
        let ns = guard.entry(namespace.into()).or_default();
        if ns.owner.as_deref().is_some_and(|x| x != owner) {
            return Err(StorageError::Ownership);
        }
        ns.owner = Some(owner.into());
        Ok(())
    }
    fn release(&self, namespace: &str, owner: &str) -> std::result::Result<(), StorageError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| StorageError::Backend(e.to_string()))?;
        let ns = guard.entry(namespace.into()).or_default();
        if ns.owner.as_deref() != Some(owner) {
            return Err(StorageError::Ownership);
        }
        ns.owner = None;
        Ok(())
    }
    fn query(
        &self,
        namespace: &str,
        query: &crate::IndexQuery,
    ) -> std::result::Result<crate::IndexPage, StorageError> {
        let to_storage = |e: Error| match e {
            Error::Storage(s) => s,
            other => StorageError::Backend(other.to_string()),
        };
        let data = {
            let guard = self
                .inner
                .lock()
                .map_err(|e| StorageError::Backend(e.to_string()))?;
            guard.get(namespace).map(|ns| ns.data.clone())
        };
        let bounded = match data {
            Some(data) if !self.unsupported && !data.header.is_empty() => {
                let source = Arc::new(TxSource {
                    data: data.clone(),
                    stats: self.stats.clone(),
                    poison: Mutex::new(None),
                });
                let state = bind_state(&data.header, source.clone()).map_err(to_storage)?;
                if state.indexes.initialized {
                    let page = state.indexes.query(&state, query);
                    source.check().map_err(to_storage)?;
                    Some(page)
                } else {
                    None
                }
            }
            _ => None,
        };
        match bounded {
            Some(page) => Ok(page),
            None => {
                let state = self.load(namespace)?.state;
                Ok(state.indexes.query(&state, query))
            }
        }
    }
    fn transition_v1_support(&self, _namespace: &str) -> std::result::Result<bool, StorageError> {
        Ok(!self.unsupported)
    }
    fn transition_v1(
        &self,
        namespace: &str,
        owner: &str,
        request: &mut TransitionRequest<'_>,
    ) -> Result<TransitionOutcome> {
        if self.unsupported {
            return Ok(TransitionOutcome::Unsupported);
        }
        let mut guard = self.lock()?;
        let ns = guard.entry(namespace.into()).or_default();
        if ns.owner.as_deref() != Some(owner) {
            return Err(StorageError::Ownership.into());
        }
        if ns.data.header.is_empty() {
            return Ok(TransitionOutcome::Unsupported);
        }
        let source = Arc::new(TxSource {
            data: ns.data.clone(),
            stats: self.stats.clone(),
            poison: Mutex::new(None),
        });
        let mut state = bind_state(&ns.data.header, source.clone())?;
        if !state.indexes.initialized {
            return Ok(TransitionOutcome::Unsupported);
        }
        self.stats.transitions.fetch_add(1, Ordering::Relaxed);
        let changed = (request.plan)(&mut state)?;
        source.check()?;
        if !changed {
            return Ok(TransitionOutcome::Unchanged);
        }
        // `settled` stays on the State for the removal check below.
        let items = crate::history_items(&state.settled);
        crate::history::check_not_live(&state, &items)?;
        source.check()?;
        let patch = take_patch(&mut state)?;
        source.check()?;
        crate::history::check_unrecorded(&ns.history, &crate::history::patch_live_keys(&patch)?)?;
        let accounting = stage(&ns.data, &ns.accounting, &patch)?;
        verify_keyed_admissions(&ns.data, &patch)?;
        verify_events(&ns.data, &patch)?;
        verify_patch_removals(&ns.data, &patch, &state)?;
        source.check()?;
        crate::history::check_new(&ns.history, &items)?;
        let policy = state.retention.policy.clone();
        drop(state);
        drop(source);
        let revision = ns
            .revision
            .checked_add(1)
            .ok_or_else(|| backend("revision exhausted"))?;
        apply(Arc::make_mut(&mut ns.data), patch)?;
        crate::history::insert(&mut ns.history, items, &policy);
        ns.accounting = accounting;
        ns.revision = revision;
        Ok(TransitionOutcome::Committed { revision })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn delete_history_of_an_unknown_namespace_creates_no_entry() {
        let store = RecordStorage::new();
        assert_eq!(
            store.delete_history("unknown", "w", 10),
            Err(StorageError::Ownership)
        );
        assert!(!store.inner.lock().unwrap().contains_key("unknown"));
    }
}
