//! Run history (spec 0003 §2). A settled cohort leaves the State in the commit
//! that settles it and is handed to storage in that same commit, which writes
//! its public inspection items once as history. History holds public
//! projections only; it is inserted once and never updated. It is a rolling
//! log: three rolling sets of `policy.history_keep` each (finished runs,
//! event receipts, admin receipts), trimmed oldest first by sequence by each
//! commit that writes history ([`HISTORY_TRIM_RUNS_PER_COMMIT`] more than it
//! writes, at most); the host may also delete history by finish time.
use crate::inspection::{self, InspectionKind, InspectionQuery, InspectionRow};
use crate::lazy::{ContainerChange, TreeKey};
use crate::transition::StatePatch;
use crate::{Intent, Receipt, Run, State, StorageError};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// One public inspection item of a finished run. `key` and `sequence` are
/// exactly those of [`inspection::rows`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryItem {
    pub kind: InspectionKind,
    pub key: String,
    pub run: String,
    pub root: String,
    pub sequence: u64,
    pub finished_at_ms: i64,
    pub value: serde_json::Value,
}
/// A root and everything that belonged to it, removed from the State by the
/// commit that carries it (`State::settled`).
#[derive(Debug, Clone)]
pub struct SettledCohort {
    pub root: String,
    pub finished_at_ms: i64,
    pub runs: Vec<Run>,
    pub intents: Vec<Intent>,
    pub receipts: Vec<Receipt>,
}
impl SettledCohort {
    pub fn history_items(&self) -> Vec<HistoryItem> {
        // A run row's sequence is its run's terminal receipt (else the
        // cohort's last receipt): the order the history trim keeps, which
        // host time cannot invert.
        let last = self.receipts.iter().map(|r| r.sequence).max().unwrap_or(0);
        let run_row = |run: &Run| InspectionRow {
            sequence: run.terminal_sequence.unwrap_or(last),
            ..inspection::run_row(run)
        };
        let item = |row: InspectionRow| HistoryItem {
            kind: row.kind,
            key: row.key,
            run: row.run,
            root: self.root.clone(),
            sequence: row.sequence,
            finished_at_ms: self.finished_at_ms,
            value: row.value,
        };
        self.runs
            .iter()
            .map(run_row)
            .chain(self.intents.iter().map(inspection::intent_row))
            .chain(self.receipts.iter().map(inspection::receipt_row))
            .map(item)
            .collect()
    }
}
pub fn history_items(settled: &[SettledCohort]) -> Vec<HistoryItem> {
    settled
        .iter()
        .flat_map(SettledCohort::history_items)
        .collect()
}

/// Refusal of a commit that would record an item a second time.
pub const ALREADY_RECORDED: &str = "run history item already recorded";
/// Refusal of a commit that would leave an item key both live and in history.
pub const ALSO_LIVE: &str = "run history item key is also live";

/// An inspection item key: `(kind, key)` as in [`inspection::rows`].
pub type ItemKey = (InspectionKind, String);
fn receipt_key(sequence: u64) -> String {
    format!("{sequence:020}")
}
fn also_live() -> StorageError {
    StorageError::Backend(ALSO_LIVE.into())
}
/// Refuse history `items` whose key is live in `state`, the State the commit
/// stores. Bounded on a bound State: map lookups, and receipts through the
/// runtime index once it is initialized (a scan of `receipts` otherwise).
pub fn check_not_live(state: &State, items: &[HistoryItem]) -> Result<(), StorageError> {
    for item in items {
        let live = match item.kind {
            InspectionKind::Runs => state.runs.contains_key(&item.key),
            InspectionKind::Effects => state.intents.contains_key(&item.key),
            InspectionKind::Receipts if state.indexes.initialized => state
                .indexes
                .all_receipts
                .get(&item.sequence)
                .and_then(|&offset| state.receipts.get(offset))
                .is_some_and(|r| r.sequence == item.sequence),
            InspectionKind::Receipts => state.receipts.iter().any(|r| r.sequence == item.sequence),
        };
        if live {
            return Err(also_live());
        }
    }
    Ok(())
}
/// Item keys live in `next` and not in `previous` (complete States).
pub fn new_live_keys(previous: &State, next: &State) -> Vec<ItemKey> {
    let old_receipts: std::collections::BTreeSet<u64> =
        previous.receipts.iter().map(|r| r.sequence).collect();
    let runs = next
        .runs
        .keys()
        .filter(|id| !previous.runs.contains_key(*id))
        .map(|id| (InspectionKind::Runs, id.clone()));
    let intents = next
        .intents
        .keys()
        .filter(|id| !previous.intents.contains_key(*id))
        .map(|id| (InspectionKind::Effects, id.clone()));
    let receipts = next
        .receipts
        .iter()
        .filter(|r| !old_receipts.contains(&r.sequence))
        .map(|r| (InspectionKind::Receipts, receipt_key(r.sequence)));
    runs.chain(intents).chain(receipts).collect()
}
/// Item keys a transition patch writes as live (bounded by the patch).
pub fn patch_live_keys(patch: &StatePatch) -> Result<Vec<ItemKey>, StorageError> {
    let mut keys = vec![];
    for (path, change) in &patch.changes {
        let kind = match (path.outer.is_none(), path.field) {
            (true, "runs") => InspectionKind::Runs,
            (true, "intents") => InspectionKind::Effects,
            (true, "receipts") => InspectionKind::Receipts,
            _ => continue,
        };
        let written: Vec<(&TreeKey, &[u8])> = match change {
            ContainerChange::Entries(rows) => rows
                .iter()
                .filter_map(|(k, v)| v.as_deref().map(|v| (k, v)))
                .collect(),
            ContainerChange::Replace(rows) => rows.iter().map(|(k, v)| (k, v.as_slice())).collect(),
        };
        for (key, value) in written {
            keys.push(match (kind, key) {
                (InspectionKind::Receipts, _) => {
                    let receipt: Receipt = serde_json::from_slice(value).map_err(|e| {
                        StorageError::Backend(format!("receipt record decode: {e}"))
                    })?;
                    (kind, receipt_key(receipt.sequence))
                }
                (_, TreeKey::Str(id)) => (kind, id.clone()),
                _ => return Err(StorageError::Backend("unexpected record key".into())),
            });
        }
    }
    Ok(keys)
}
/// Refuse live `keys` already recorded as history.
pub(crate) fn check_unrecorded(rows: &HistoryRows, keys: &[ItemKey]) -> Result<(), StorageError> {
    if keys
        .iter()
        .any(|(kind, key)| rows.contains_key(&(kind.key().to_string(), key.clone())))
    {
        return Err(also_live());
    }
    Ok(())
}

/// A commit that writes history trims, of each rolling set (finished runs,
/// event receipts, admin receipts), at most this many more than it writes
/// (so the trim keeps up with any commit, and a lowered `history_keep` is
/// reached within a bounded number of commits).
pub const HISTORY_TRIM_RUNS_PER_COMMIT: usize = 64;
/// How many units of one rolling set holding `count` the trim of a commit
/// that writes `written` of them may remove under `keep`.
pub fn trim_budget(count: u64, written: usize, keep: u64) -> usize {
    let excess = usize::try_from(count.saturating_sub(keep)).unwrap_or(usize::MAX);
    excess.min(written.saturating_add(HISTORY_TRIM_RUNS_PER_COMMIT))
}
/// Which rolling set of history an item counts in. History is three
/// rolling sets of `history_keep` each, trimmed oldest first by sequence:
/// finished runs (a run's `runs` row, with its effects and receipts), the
/// receipts of events (`event:<key>`: an event's receipts leave together),
/// and admin receipts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryUnit {
    /// A finished run's `runs` row.
    Run,
    /// A receipt of an event (`run` is `event:<key>`).
    EventReceipt,
    /// An `admin` receipt.
    AdminReceipt,
    /// An effect or receipt of a run: it leaves with its run.
    OfRun,
}
pub fn history_unit(item: &HistoryItem) -> HistoryUnit {
    match item.kind {
        InspectionKind::Runs => HistoryUnit::Run,
        InspectionKind::Receipts if item.run == "admin" => HistoryUnit::AdminReceipt,
        InspectionKind::Receipts if item.run.starts_with("event:") => HistoryUnit::EventReceipt,
        _ => HistoryUnit::OfRun,
    }
}
/// How many of `items` count in each rolling set: (runs, event receipts,
/// admin receipts).
pub fn units_written(items: &[HistoryItem]) -> (usize, usize, usize) {
    let of = |unit: HistoryUnit| items.iter().filter(|i| history_unit(i) == unit).count();
    (
        of(HistoryUnit::Run),
        of(HistoryUnit::EventReceipt),
        of(HistoryUnit::AdminReceipt),
    )
}

type RowKey = (String, String);
/// History rows of one namespace in the in-memory backends, by
/// `(kind.key(), key)`, with each rolling set in sequence order and each
/// run's (or event's) rows (what the PostgreSQL indexes give).
#[derive(Debug, Default)]
pub(crate) struct HistoryRows {
    pub(crate) rows: BTreeMap<RowKey, HistoryItem>,
    /// Each rolling set, oldest first: `(sequence, row)`.
    runs: BTreeSet<(u64, RowKey)>,
    events: BTreeSet<(u64, RowKey)>,
    admin: BTreeSet<(u64, RowKey)>,
    /// The rows of each run or event (`HistoryItem::run`).
    by_run: BTreeMap<String, BTreeSet<RowKey>>,
}
impl HistoryRows {
    fn contains_key(&self, key: &RowKey) -> bool {
        self.rows.contains_key(key)
    }
    fn set(&mut self, unit: HistoryUnit) -> Option<&mut BTreeSet<(u64, RowKey)>> {
        match unit {
            HistoryUnit::Run => Some(&mut self.runs),
            HistoryUnit::EventReceipt => Some(&mut self.events),
            HistoryUnit::AdminReceipt => Some(&mut self.admin),
            HistoryUnit::OfRun => None,
        }
    }
    fn add(&mut self, item: HistoryItem) {
        let key = row_key(&item);
        if let Some(set) = self.set(history_unit(&item)) {
            set.insert((item.sequence, key.clone()));
        }
        self.by_run
            .entry(item.run.clone())
            .or_default()
            .insert(key.clone());
        self.rows.insert(key, item);
    }
    fn remove(&mut self, key: &RowKey) -> Option<HistoryItem> {
        let item = self.rows.remove(key)?;
        let sequence = item.sequence;
        if let Some(set) = self.set(history_unit(&item)) {
            set.remove(&(sequence, key.clone()));
        }
        if let Some(keys) = self.by_run.get_mut(&item.run) {
            keys.remove(key);
            if keys.is_empty() {
                self.by_run.remove(&item.run);
            }
        }
        Some(item)
    }
    /// Remove every row of run (or event) `owner`, its `runs` row last.
    fn remove_owner(&mut self, owner: &str) -> usize {
        let run_row = (InspectionKind::Runs.key().to_string(), owner.to_string());
        let keys: Vec<RowKey> = self
            .by_run
            .get(owner)
            .into_iter()
            .flatten()
            .filter(|key| **key != run_row)
            .cloned()
            .collect();
        let mut removed = keys.len();
        for key in keys {
            self.remove(&key);
        }
        removed += usize::from(self.remove(&run_row).is_some());
        removed
    }
    /// Delete the oldest of each rolling set beyond `keep`, at most
    /// [`trim_budget`] for a commit that wrote `written` (runs, event
    /// receipts, admin receipts) of them: a run with all its rows (its `runs`
    /// row last), an event's receipts together, an admin receipt alone.
    /// O(rows deleted).
    fn trim(&mut self, keep: u64, written: (usize, usize, usize)) {
        for _ in 0..trim_budget(self.runs.len() as u64, written.0, keep) {
            let Some((_, (_, run))) = self.runs.first().cloned() else {
                break;
            };
            self.remove_owner(&run);
        }
        let mut budget = trim_budget(self.events.len() as u64, written.1, keep);
        while budget > 0 {
            let Some((_, key)) = self.events.first().cloned() else {
                break;
            };
            let owner = self.rows[&key].run.clone();
            budget = budget.saturating_sub(self.remove_owner(&owner));
        }
        for _ in 0..trim_budget(self.admin.len() as u64, written.2, keep) {
            let Some((_, key)) = self.admin.first().cloned() else {
                break;
            };
            self.remove(&key);
        }
    }
}
fn row_key(item: &HistoryItem) -> (String, String) {
    (item.kind.key().to_string(), item.key.clone())
}
/// Refuse a commit whose history repeats an item already recorded (or repeats
/// one within itself). Runs before anything is inserted.
pub(crate) fn check_new(rows: &HistoryRows, items: &[HistoryItem]) -> Result<(), StorageError> {
    let mut seen = std::collections::BTreeSet::new();
    for item in items {
        let key = row_key(item);
        if rows.contains_key(&key) || !seen.insert(key) {
            return Err(StorageError::Backend(ALREADY_RECORDED.into()));
        }
    }
    Ok(())
}
/// Write the history of a commit under `keep` (`policy.history_keep`): its
/// items (none when `keep` is 0), then the trim of each rolling set beyond
/// `keep`, when the commit has history to write.
pub(crate) fn insert(rows: &mut HistoryRows, items: Vec<HistoryItem>, keep: u64) {
    if items.is_empty() {
        return;
    }
    let written = units_written(&items);
    if keep > 0 {
        for item in items {
            rows.add(item);
        }
    }
    rows.trim(keep, written);
}
/// At most `limit + 1` items matching `query`, in key order.
pub(crate) fn page(
    rows: &HistoryRows,
    query: &InspectionQuery,
) -> Result<Vec<HistoryItem>, StorageError> {
    use std::ops::Bound::{Excluded, Included, Unbounded};
    query.validate()?;
    let kind = query.kind.key().to_string();
    let start = match &query.after {
        Some(after) => Excluded((kind.clone(), after.clone())),
        None => Included((kind.clone(), String::new())),
    };
    Ok(rows
        .rows
        .range((start, Unbounded))
        .take_while(|((k, _), _)| *k == kind)
        .map(|(_, item)| item)
        .filter(|item| query.run.as_ref().is_none_or(|run| &item.run == run))
        .take(query.limit + 1)
        .cloned()
        .collect())
}
/// Delete the items of runs finished before `before_ms`; returns how many.
pub(crate) fn delete_before(rows: &mut HistoryRows, before_ms: i64) -> u64 {
    let old: Vec<(String, String)> = rows
        .rows
        .iter()
        .filter(|(_, item)| item.finished_at_ms < before_ms)
        .map(|(key, _)| key.clone())
        .collect();
    for key in &old {
        rows.remove(key);
    }
    old.len() as u64
}
