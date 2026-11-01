//! Explicit replay horizons, disposal records and epoch fences. No unresolved
//! obligation is evicted for quota pressure.
use crate::{Error, IntentStatus, Result, State};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetentionPolicy {
    pub event_replay_ms: u64,
    pub terminal_run_ms: u64,
    pub audit_ms: u64,
    pub max_tombstones: usize,
}
impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            event_replay_ms: 86_400_000,
            terminal_run_ms: 604_800_000,
            audit_ms: 604_800_000,
            max_tombstones: 100_000,
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
    pub event_epochs: BTreeMap<String, u64>,
    #[serde(default)]
    pub event_expires_at_ms: BTreeMap<String, i64>,
    #[serde(default)]
    pub expired_correlations: BTreeMap<String, (String, u64)>,
    pub event_tombstones: BTreeMap<String, EventTombstone>,
    pub artifact_disposals: BTreeMap<String, i64>,
    pub disposals: Vec<Disposal>,
    pub gc_cursor: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GcReport {
    pub runs: usize,
    pub events: usize,
    pub artifacts: usize,
    pub receipts: usize,
    pub preserved: usize,
    pub more: bool,
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
pub(crate) fn gc(s: &mut State, now: i64, limit: usize) -> Result<GcReport> {
    if limit == 0 || limit > 1000 {
        return Err(Error::Refused("GC page limit must be 1..1000".into()));
    }
    if s.retention
        .last_gc_at_ms
        .is_some_and(|previous| now < previous)
    {
        return Err(Error::Refused("GC clock cannot move backwards".into()));
    }
    s.retention.last_gc_at_ms = Some(now);
    let p = s.retention.policy.clone();
    let cutoff = |duration: u64| now.saturating_sub(i64::try_from(duration).unwrap_or(i64::MAX));
    let mut report = GcReport::default();
    let (mut phase, mut cursor, mut inclusive): (u8, String, bool) = s
        .retention
        .gc_cursor
        .as_deref()
        .and_then(|v| serde_json::from_str(v).ok())
        .unwrap_or((0, String::new(), false));
    let mut examined = 0;
    while examined < limit {
        match phase {
            0 => {
                let bound = if inclusive {
                    std::ops::Bound::Included(cursor.clone())
                } else {
                    std::ops::Bound::Excluded(cursor.clone())
                };
                let key = s
                    .indexes
                    .root_members
                    .range((bound, std::ops::Bound::Unbounded))
                    .next()
                    .map(|(key, _)| key.clone());
                let Some(root) = key else {
                    phase = 1;
                    cursor.clear();
                    inclusive = false;
                    continue;
                };
                examined += 1;
                cursor = root.clone();
                inclusive = false;
                let active = s
                    .indexes
                    .root_active
                    .get(&root)
                    .is_some_and(|ids| !ids.is_empty());
                let unsettled = s
                    .indexes
                    .root_effects
                    .get(&root)
                    .is_some_and(|ids| !ids.is_empty());
                let recent = s
                    .indexes
                    .root_last_activity
                    .get(&root)
                    .copied()
                    .unwrap_or(i64::MAX)
                    > cutoff(p.terminal_run_ms.max(p.audit_ms));
                if active || unsettled || recent {
                    report.preserved += 1;
                    continue;
                }
                let Some(id) = s.indexes.root_members[&root]
                    .iter()
                    .find(|id| *id != &root)
                    .cloned()
                    .or_else(|| s.indexes.root_members[&root].first().cloned())
                else {
                    continue;
                };
                let intents = s
                    .intents
                    .values()
                    .filter(|i| i.run == id)
                    .map(|i| i.id.clone())
                    .collect::<Vec<_>>();
                for intent in intents {
                    s.intents.remove(&intent);
                    s.recovery_requests
                        .retain(|_, request| request.intent != intent);
                    crate::indexes::mark_intent(s, &intent);
                }
                s.runs.remove(&id);
                s.waits.retain(|_, w| w.run != id);
                crate::indexes::mark_run(s, &id);
                let before = s.receipts.len();
                s.receipts.retain(|receipt| receipt.run != id);
                report.receipts += before - s.receipts.len();
                report.runs += 1;
                s.retention.disposals.push(Disposal {
                    kind: "run".into(),
                    key: id.clone(),
                    at_ms: now,
                });
                s.indexes.root_members.get_mut(&root).unwrap().remove(&id);
                if s.indexes.root_members[&root].is_empty() {
                    s.indexes.root_members.remove(&root);
                    s.indexes.root_active.remove(&root);
                    s.indexes.root_effects.remove(&root);
                    s.indexes.root_last_activity.remove(&root);
                    s.budgets.remove(&root);
                } else {
                    inclusive = true;
                }
            }
            1 => {
                let key = s
                    .events
                    .range((
                        std::ops::Bound::Excluded(cursor.clone()),
                        std::ops::Bound::Unbounded,
                    ))
                    .next()
                    .map(|(key, _)| key.clone());
                let Some(id) = key else {
                    phase = 2;
                    cursor.clear();
                    inclusive = false;
                    continue;
                };
                examined += 1;
                cursor = id.clone();
                let e = &s.events[&id];
                let expiry = s
                    .retention
                    .event_expires_at_ms
                    .get(&id)
                    .copied()
                    .unwrap_or_else(|| {
                        e.accepted_at_ms
                            .saturating_add(i64::try_from(p.event_replay_ms).unwrap_or(i64::MAX))
                    });
                let protected = s.indexes.event_memberships.get(&id).is_some_and(|keys| {
                    keys.iter().any(|key| {
                        s.indexes.waiters.get(key).is_some_and(|waiters| {
                            waiters.iter().any(|target| {
                                if let Some(intent) = target.strip_prefix("intent:") {
                                    s.intents.get(intent).is_some_and(|i| {
                                        !matches!(
                                            i.status,
                                            IntentStatus::Completed
                                                | IntentStatus::Rejected
                                                | IntentStatus::Cancelled
                                        )
                                    })
                                } else {
                                    s.runs.get(target).is_some_and(|r| !r.status.terminal())
                                }
                            })
                        })
                    })
                });
                let unknown_handle = s
                    .indexes
                    .uncertain_completion_contracts
                    .get(&crate::indexes::trigger_key(
                        &e.envelope.contract,
                        &e.envelope.scope,
                    )?)
                    .is_some_and(|intents| !intents.is_empty());
                if expiry > now
                    || e.destinations.iter().any(|d| d.run.is_none())
                    || protected
                    || unknown_handle
                {
                    report.preserved += 1;
                    continue;
                }
                let epoch = s.retention.event_epochs.get(&id).copied().unwrap_or(0);
                let current = s
                    .retention
                    .producer_epochs
                    .get(&e.envelope.producer)
                    .copied()
                    .unwrap_or(0);
                if epoch == current && s.retention.event_tombstones.len() >= p.max_tombstones {
                    return Err(Error::Refused(
                        "replay tombstone quota reached; retire producer epoch before GC".into(),
                    ));
                }
                let new_correlations = s
                    .indexes
                    .event_memberships
                    .get(&id)
                    .map(|keys| {
                        keys.iter()
                            .filter(|key| !s.retention.expired_correlations.contains_key(*key))
                            .count()
                    })
                    .unwrap_or(0);
                if epoch == current
                    && s.retention
                        .expired_correlations
                        .len()
                        .saturating_add(new_correlations)
                        > p.max_tombstones.saturating_mul(64)
                {
                    return Err(Error::Refused("correlation history fence quota reached; settle retained invocations before epoch compaction".into()));
                }
                let e = s.events.remove(&id).unwrap();
                s.retention.event_epochs.remove(&id);
                s.retention.event_expires_at_ms.remove(&id);
                if epoch == current {
                    s.retention.event_tombstones.insert(
                        id.clone(),
                        EventTombstone {
                            producer: e.envelope.producer.clone(),
                            epoch,
                            digest: e.digest,
                            disposed_at_ms: now,
                        },
                    );
                }
                if let Some(keys) = s.indexes.event_memberships.remove(&id) {
                    for key in keys {
                        if epoch == current {
                            s.retention
                                .expired_correlations
                                .insert(key.clone(), (e.envelope.producer.clone(), epoch));
                        }
                        if let Some(events) = s.indexes.events.get_mut(&key) {
                            events.retain(|_, value| value != &id);
                            if events.is_empty() {
                                s.indexes.events.remove(&key);
                            }
                        }
                    }
                }
                s.retention.disposals.push(Disposal {
                    kind: "event".into(),
                    key: id,
                    at_ms: now,
                });
                report.events += 1;
            }
            2 => {
                let key = s
                    .artifacts
                    .range((
                        std::ops::Bound::Excluded(cursor.clone()),
                        std::ops::Bound::Unbounded,
                    ))
                    .next()
                    .map(|(key, _)| key.clone());
                let Some(id) = key else {
                    phase = 3;
                    break;
                };
                examined += 1;
                cursor = id.clone();
                if artifact_referenced(s, &id) {
                    report.preserved += 1;
                    continue;
                }
                s.artifacts.remove(&id);
                s.retention.artifact_disposals.insert(id.clone(), now);
                s.retention.disposals.push(Disposal {
                    kind: "artifact".into(),
                    key: id,
                    at_ms: now,
                });
                report.artifacts += 1;
            }
            _ => break,
        }
    }
    report.more = phase < 3;
    s.retention.gc_cursor = if report.more {
        Some(
            serde_json::to_string(&(phase, cursor, inclusive))
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
        if *at_ms >= cutoff(p.audit_ms) {
            revisions.insert(*revision);
        }
    }
    s.catalog_history
        .retain(|revision, _| revisions.contains(revision));
    s.catalog_history_at_ms
        .retain(|revision, _| revisions.contains(revision));
    s.retention
        .disposals
        .retain(|d| d.at_ms >= cutoff(p.audit_ms));
    s.retention
        .artifact_disposals
        .retain(|_, at| *at >= cutoff(p.audit_ms));
    s.indexes.receipts.clear();
    s.indexes.all_receipts.clear();
    for (i, r) in s.receipts.iter().enumerate() {
        s.indexes
            .receipts
            .entry(r.run.clone())
            .or_default()
            .insert(r.sequence, i);
        s.indexes.all_receipts.insert(r.sequence, i);
    }
    Ok(report)
}
