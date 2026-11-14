//! Durable bounded discovery indexes. These are maintained in the same CAS as
//! authoritative state; adapters may mirror them into indexed native tables.
use crate::lazy::{LazyMap, LazySet};
use crate::{IntentStatus, Pending, Result, RunStatus, State};
use aq_playbooks_model::{ContractRef, digest};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectQueueKind {
    Dispatch,
    Cancel,
    Recovery,
    Completion,
}
impl EffectQueueKind {
    pub fn key(self) -> &'static str {
        match self {
            Self::Dispatch => "dispatch",
            Self::Cancel => "cancel",
            Self::Recovery => "recovery",
            Self::Completion => "completion",
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "query", rename_all = "snake_case")]
pub enum IndexQuery {
    Ready {
        scope_after: Option<String>,
        limit: usize,
    },
    DueTimers {
        now_ms: i64,
        limit: usize,
    },
    Effects {
        kind: EffectQueueKind,
        scope_after: Option<String>,
        limit: usize,
    },
    CorrelatedEvents {
        key: String,
        after_sequence: u64,
        limit: usize,
    },
    Waiters {
        key: String,
        limit: usize,
    },
    Runs {
        after: Option<String>,
        limit: usize,
    },
    Receipts {
        run: Option<String>,
        after_sequence: u64,
        limit: usize,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexItem {
    pub key: String,
    pub scope: String,
    pub sequence: u64,
    pub at_ms: Option<i64>,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IndexPage {
    pub items: Vec<IndexItem>,
    pub next: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimerIndex {
    pub key: String,
    pub run: String,
    pub at_ms: i64,
    pub intent: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RuntimeIndexes {
    pub ready: BTreeMap<String, BTreeMap<u64, String>>,
    pub ready_tokens: BTreeMap<String, (String, u64)>,
    pub scope_cursor: Option<String>,
    pub dispatch_turn: bool,
    pub effect_cursor: BTreeMap<String, String>,
    pub effect_item_cursor: BTreeMap<String, String>,
    pub root_members: LazyMap<String, BTreeSet<String>>,
    pub root_active: LazyMap<String, BTreeSet<String>>,
    pub root_effects: LazyMap<String, BTreeSet<String>>,
    /// Every intent (terminal included) per root, for bounded history checks.
    #[serde(default)]
    pub root_intents: LazyMap<String, BTreeSet<String>>,
    pub root_last_activity: LazyMap<String, i64>,
    pub run_roots: LazyMap<String, String>,
    pub intent_roots: LazyMap<String, String>,
    pub effects: LazyMap<String, BTreeMap<String, BTreeSet<String>>>,
    pub effect_memberships: LazyMap<String, Vec<(String, String)>>,
    pub timers: LazyMap<String, TimerIndex>,
    pub due: LazyMap<i64, BTreeSet<String>>,
    pub run_timer: LazyMap<String, String>,
    pub events: LazyMap<String, BTreeMap<u64, String>>,
    pub event_memberships: LazyMap<String, Vec<String>>,
    /// The events of each contract and scope (`trigger_key`) by acceptance
    /// sequence, so a change of that contract's uncertain completions
    /// re-judges the holds of the events accepted after the operation's
    /// dispatch, and only those.
    #[serde(default)]
    pub contract_events: LazyMap<String, BTreeMap<u64, String>>,
    pub waiters: LazyMap<String, BTreeSet<String>>,
    pub run_subscriptions: LazyMap<String, Vec<String>>,
    pub dependents: LazyMap<String, BTreeSet<String>>,
    pub run_dependencies: LazyMap<String, Vec<String>>,
    pub completion_ready: BTreeSet<String>,
    pub uncertain_completion_contracts: BTreeMap<String, BTreeSet<String>>,
    pub uncertain_contract_membership: BTreeMap<String, String>,
    pub dirty_runs: BTreeSet<String>,
    pub dirty_intents: BTreeSet<String>,
    pub runs: LazySet<String>,
    pub receipts: LazyMap<String, BTreeMap<u64, usize>>,
    pub all_receipts: LazyMap<u64, usize>,
    pub triggers: BTreeMap<String, BTreeSet<String>>,
    pub activation_triggers: BTreeMap<String, Vec<String>>,
    pub initialized: bool,
    /// `root_intents` holds every live operation. False in a State written
    /// before the index existed (pre-D10); `Engine::new` then backfills it
    /// once. Cohort removal takes a cohort's operations from it.
    #[serde(default)]
    pub root_intents_complete: bool,
    /// `contract_events` holds every event. False in a State written before
    /// the index existed; `Engine::new` then fills it once.
    #[serde(default)]
    pub contract_events_complete: bool,
    /// Every dispatched live operation carries its dispatch sequence. False
    /// in a State written before `Intent::dispatch_sequence` existed;
    /// `Engine::new` then fills it once from each operation's own first
    /// `dispatch` receipt.
    #[serde(default)]
    pub dispatch_sequences_complete: bool,
}
pub fn correlation_key(
    contract: &ContractRef,
    scope: &str,
    correlation: &BTreeMap<String, String>,
) -> Result<String> {
    Ok(digest(&(contract, scope, correlation))?)
}
pub fn field_key(contract: &ContractRef, scope: &str, field: &str, value: &str) -> Result<String> {
    Ok(digest(&(contract, scope, field, value))?)
}
pub fn trigger_key(contract: &ContractRef, scope: &str) -> Result<String> {
    Ok(digest(&(contract, scope))?)
}
fn fair_scopes<T>(scopes: &BTreeMap<String, T>, after: Option<&str>) -> Vec<String> {
    let mut v: Vec<_> = scopes
        .keys()
        .filter(|s| after.is_none_or(|a| s.as_str() > a))
        .cloned()
        .collect();
    v.extend(
        scopes
            .keys()
            .filter(|s| after.is_some_and(|a| s.as_str() <= a))
            .cloned(),
    );
    v
}
impl RuntimeIndexes {
    pub fn query(&self, state: &State, query: &IndexQuery) -> IndexPage {
        let mut items = vec![];
        let limit = match query {
            IndexQuery::Ready { limit, .. }
            | IndexQuery::DueTimers { limit, .. }
            | IndexQuery::Effects { limit, .. }
            | IndexQuery::CorrelatedEvents { limit, .. }
            | IndexQuery::Waiters { limit, .. }
            | IndexQuery::Runs { limit, .. }
            | IndexQuery::Receipts { limit, .. } => *limit,
        }
        .min(1000);
        match query {
            IndexQuery::Ready { scope_after, .. } => {
                for scope in fair_scopes(
                    &self.ready,
                    scope_after.as_deref().or(self.scope_cursor.as_deref()),
                ) {
                    if let Some((seq, id)) = self.ready[&scope].first_key_value() {
                        items.push(IndexItem {
                            key: id.clone(),
                            scope: scope.clone(),
                            sequence: *seq,
                            at_ms: None,
                        });
                        if items.len() >= limit {
                            break;
                        }
                    }
                }
            }
            IndexQuery::Effects {
                kind, scope_after, ..
            } => {
                if *kind == EffectQueueKind::Completion {
                    for id in self.completion_ready.iter().take(limit) {
                        items.push(IndexItem {
                            key: id.clone(),
                            scope: state
                                .intents
                                .get(id)
                                .map(|i| i.authority.scope.clone())
                                .unwrap_or_default(),
                            sequence: 0,
                            at_ms: None,
                        });
                    }
                } else if let Some(scopes) = self.effects.get(kind.key()) {
                    for scope in fair_scopes(
                        scopes,
                        scope_after
                            .as_deref()
                            .or_else(|| self.effect_cursor.get(kind.key()).map(String::as_str)),
                    ) {
                        let cursor =
                            self.effect_item_cursor
                                .get(&format!("{}:{}", kind.key(), scope));
                        let selected = scopes[&scope]
                            .iter()
                            .find(|id| cursor.is_none_or(|cursor| *id > cursor))
                            .or_else(|| scopes[&scope].first());
                        if let Some(id) = selected {
                            items.push(IndexItem {
                                key: id.clone(),
                                scope: scope.clone(),
                                sequence: 0,
                                at_ms: None,
                            });
                        }
                        if items.len() >= limit {
                            break;
                        }
                    }
                }
            }
            IndexQuery::DueTimers { now_ms, .. } => {
                let mut after: Option<i64> = None;
                while let Some(at) = self.due.next_key(after.as_ref()) {
                    if at > *now_ms {
                        break;
                    }
                    after = Some(at);
                    let at = &at;
                    let ids = &self.due[at];
                    for id in ids {
                        let t = &self.timers[id];
                        items.push(IndexItem {
                            key: id.clone(),
                            scope: state
                                .runs
                                .get(&t.run)
                                .map(|r| r.authority.scope.clone())
                                .unwrap_or_default(),
                            sequence: 0,
                            at_ms: Some(*at),
                        });
                        if items.len() >= limit {
                            break;
                        }
                    }
                    if items.len() >= limit {
                        break;
                    }
                }
            }
            IndexQuery::CorrelatedEvents {
                key,
                after_sequence,
                ..
            } => {
                if let Some(events) = self.events.get(key) {
                    for (seq, id) in events
                        .range((
                            std::ops::Bound::Excluded(*after_sequence),
                            std::ops::Bound::Unbounded,
                        ))
                        .take(limit)
                    {
                        items.push(IndexItem {
                            key: id.clone(),
                            scope: state
                                .events
                                .get(id)
                                .map(|e| e.envelope.scope.clone())
                                .unwrap_or_default(),
                            sequence: *seq,
                            at_ms: state.events.get(id).map(|e| e.accepted_at_ms),
                        });
                    }
                }
            }
            IndexQuery::Waiters { key, .. } => {
                if let Some(waiters) = self.waiters.get(key) {
                    for id in waiters.iter().take(limit) {
                        items.push(IndexItem {
                            key: id.clone(),
                            scope: state
                                .runs
                                .get(id)
                                .map(|r| r.authority.scope.clone())
                                .unwrap_or_default(),
                            sequence: 0,
                            at_ms: None,
                        });
                    }
                }
            }
            IndexQuery::Runs { after, .. } => {
                for id in self
                    .runs
                    .iter()
                    .filter(|id| after.as_ref().is_none_or(|a| *id > a))
                    .take(limit)
                {
                    items.push(IndexItem {
                        key: id.clone(),
                        scope: state
                            .runs
                            .get(id)
                            .map(|r| r.authority.scope.clone())
                            .unwrap_or_default(),
                        sequence: 0,
                        at_ms: None,
                    });
                }
            }
            IndexQuery::Receipts {
                run,
                after_sequence,
                ..
            } => {
                let receipts = if let Some(run) = run {
                    self.receipts.get(run)
                } else {
                    Some(&*self.all_receipts)
                };
                if let Some(receipts) = receipts {
                    for (seq, offset) in receipts
                        .range((
                            std::ops::Bound::Excluded(*after_sequence),
                            std::ops::Bound::Unbounded,
                        ))
                        .take(limit)
                    {
                        items.push(IndexItem {
                            key: offset.to_string(),
                            scope: String::new(),
                            sequence: *seq,
                            at_ms: state.receipts.get(*offset).map(|r| r.at_ms),
                        });
                    }
                }
            }
        }
        if limit == 0 {
            items.clear()
        }
        let next = items.last().map(|i| match query {
            IndexQuery::Ready { .. } | IndexQuery::Effects { .. } => i.scope.clone(),
            IndexQuery::Receipts { .. } | IndexQuery::CorrelatedEvents { .. } => {
                i.sequence.to_string()
            }
            _ => i.key.clone(),
        });
        IndexPage { items, next }
    }
}
pub(crate) fn mark_run(s: &mut State, id: &str) {
    s.indexes.dirty_runs.insert(id.into());
}
pub(crate) fn mark_intent(s: &mut State, id: &str) {
    s.indexes.dirty_intents.insert(id.into());
}
pub(crate) fn wake(s: &mut State, id: &str) {
    if !s.runs.contains_key(id) {
        return;
    }
    if s.runs[id].status.terminal() {
        return;
    }
    if !s.indexes.ready_tokens.contains_key(id) {
        s.sequence += 1;
        let scope = s.runs[id].authority.scope.clone();
        s.indexes
            .ready
            .entry(scope.clone())
            .or_default()
            .insert(s.sequence, id.into());
        s.indexes
            .ready_tokens
            .insert(id.into(), (scope, s.sequence));
    }
}
pub(crate) fn unqueue(s: &mut State, id: &str) {
    if let Some((scope, seq)) = s.indexes.ready_tokens.remove(id)
        && let Some(q) = s.indexes.ready.get_mut(&scope)
    {
        q.remove(&seq);
        if q.is_empty() {
            s.indexes.ready.remove(&scope);
        }
    }
}
pub(crate) fn wake_dependents(s: &mut State, id: &str) {
    let ids = s.indexes.dependents.get(id).cloned().unwrap_or_default();
    for id in ids {
        wake(s, &id);
    }
}
pub(crate) fn remove_timer(s: &mut State, key: &str) {
    if let Some(t) = s.indexes.timers.remove(key)
        && let Some(ids) = s.indexes.due.get_mut(&t.at_ms)
    {
        ids.remove(key);
        if ids.is_empty() {
            s.indexes.due.remove(&t.at_ms);
        }
    }
}
pub(crate) fn timer(s: &mut State, t: TimerIndex) {
    remove_timer(s, &t.key);
    s.indexes
        .due
        .entry(t.at_ms)
        .or_default()
        .insert(t.key.clone());
    s.indexes.timers.insert(t.key.clone(), t);
}
fn subscribe(s: &mut State, run: &str, key: String) {
    s.replay_touched.wait_keys.insert(key.clone());
    s.indexes
        .waiters
        .entry(key.clone())
        .or_default()
        .insert(run.into());
    s.indexes
        .run_subscriptions
        .entry(run.into())
        .or_default()
        .push(key);
}
fn depend(s: &mut State, run: &str, id: String) {
    s.indexes
        .dependents
        .entry(id.clone())
        .or_default()
        .insert(run.into());
    s.indexes
        .run_dependencies
        .entry(run.into())
        .or_default()
        .push(id);
}
pub(crate) fn index_event(s: &mut State, id: &str) -> Result<()> {
    let e = s.events[id].clone();
    let mut keys = vec![correlation_key(
        &e.envelope.contract,
        &e.envelope.scope,
        &e.envelope.correlation,
    )?];
    for (field, value) in &e.envelope.correlation {
        keys.push(field_key(
            &e.envelope.contract,
            &e.envelope.scope,
            field,
            value,
        )?);
    }
    for key in &keys {
        s.indexes
            .events
            .entry(key.clone())
            .or_default()
            .insert(e.sequence, id.into());
        let targets = s.indexes.waiters.get(key).cloned().unwrap_or_default();
        for target in targets {
            if let Some(intent) = target.strip_prefix("intent:") {
                s.indexes.completion_ready.insert(intent.into());
            } else {
                wake(s, &target);
            }
        }
    }
    s.indexes.event_memberships.insert(id.into(), keys);
    let contract = trigger_key(&e.envelope.contract, &e.envelope.scope)?;
    s.indexes
        .contract_events
        .entry(contract)
        .or_default()
        .insert(e.sequence, id.into());
    Ok(())
}
/// Take event `e` out of `contract_events`.
pub(crate) fn unindex_contract_event(s: &mut State, e: &crate::AcceptedEvent) -> Result<()> {
    let contract = trigger_key(&e.envelope.contract, &e.envelope.scope)?;
    if let Some(events) = s.indexes.contract_events.get_mut(&contract) {
        events.remove(&e.sequence);
        if events.is_empty() {
            s.indexes.contract_events.remove(&contract);
        }
    }
    Ok(())
}
/// A State written before `contract_events` existed: fill it once from the
/// events (one pass over them).
pub(crate) fn backfill_contract_events(s: &mut State) -> Result<()> {
    if !s.indexes.initialized || s.indexes.contract_events_complete {
        return Ok(());
    }
    let events: Vec<(String, u64, String)> = s
        .events
        .iter()
        .map(|(id, e)| {
            Ok((
                trigger_key(&e.envelope.contract, &e.envelope.scope)?,
                e.sequence,
                id.clone(),
            ))
        })
        .collect::<Result<_>>()?;
    for (contract, sequence, id) in events {
        s.indexes
            .contract_events
            .entry(contract)
            .or_default()
            .insert(sequence, id);
    }
    s.indexes.contract_events_complete = true;
    Ok(())
}
pub(crate) fn index_activation(s: &mut State, binding: &str) -> Result<()> {
    if let Some(keys) = s.indexes.activation_triggers.remove(binding) {
        for key in keys {
            remove_member(&mut s.indexes.triggers, &key, binding);
        }
    }
    let Some(a) = s.bindings.get(binding) else {
        return Ok(());
    };
    if !a.controls.event_starts_enabled {
        return Ok(());
    }
    let artifact = &s.artifacts[&a.artifact];
    let scope = a.authority.scope.clone();
    let keys = artifact
        .content
        .playbook
        .triggers
        .iter()
        .map(|t| trigger_key(&t.event, &scope))
        .collect::<Result<Vec<_>>>()?;
    for key in &keys {
        s.indexes
            .triggers
            .entry(key.clone())
            .or_default()
            .insert(binding.into());
    }
    s.indexes.activation_triggers.insert(binding.into(), keys);
    Ok(())
}
fn refresh_run(s: &mut State, id: &str) -> Result<()> {
    let was_queued = s.indexes.ready_tokens.contains_key(id);
    unqueue(s, id);
    if let Some(keys) = s.indexes.run_subscriptions.remove(id) {
        for key in keys {
            remove_lazy_member(&mut s.indexes.waiters, &key, id);
            s.replay_touched.wait_keys.insert(key);
        }
    }
    if let Some(ids) = s.indexes.run_dependencies.remove(id) {
        for dep in ids {
            remove_lazy_member(&mut s.indexes.dependents, &dep, id);
        }
    }
    let timerkey = format!("run:{id}");
    remove_timer(s, &timerkey);
    let Some(r) = s.runs.get(id).cloned() else {
        s.indexes.runs.remove(id);
        if let Some(root) = s.indexes.run_roots.remove(id) {
            if let Some(ids) = s.indexes.root_members.get_mut(&root) {
                ids.remove(id);
                if ids.is_empty() {
                    s.indexes.root_members.remove(&root);
                    s.indexes.root_active.remove(&root);
                    s.indexes.root_effects.remove(&root);
                    s.indexes.root_intents.remove(&root);
                    s.indexes.root_last_activity.remove(&root);
                }
            }
            if let Some(ids) = s.indexes.root_active.get_mut(&root) {
                ids.remove(id);
            }
        }
        return Ok(());
    };
    s.indexes.runs.insert(id.into());
    s.indexes.run_roots.insert(id.into(), r.root.clone());
    s.indexes
        .root_members
        .entry(r.root.clone())
        .or_default()
        .insert(id.into());
    if r.status.terminal() {
        if let Some(ids) = s.indexes.root_active.get_mut(&r.root) {
            ids.remove(id);
        }
    } else {
        s.indexes
            .root_active
            .entry(r.root.clone())
            .or_default()
            .insert(id.into());
    }
    if r.status.terminal() {
        wake_dependents(s, id);
        return Ok(());
    }
    let mut at = None;
    match r.status {
        RunStatus::Admitted | RunStatus::Running => wake(s, id),
        RunStatus::Waiting => match &r.pending {
            Some(Pending::Effect { intent }) => {
                depend(s, id, intent.clone());
                if let Some(i) = s.intents.get(intent)
                    && (matches!(
                        i.status,
                        IntentStatus::Completed
                            | IntentStatus::Rejected
                            | IntentStatus::Cancelled
                            | IntentStatus::Unknown
                    ) || i.status == IntentStatus::Accepted
                        && s.artifacts[&r.artifact].content.playbook.steps[&r.step]
                            .edges
                            .contains_key("accepted"))
                {
                    wake(s, id);
                }
            }
            Some(Pending::Event { wait }) => {
                let w = &s.waits[wait];
                at = Some(w.deadline);
                if let Some(contract) = &w.event {
                    subscribe(s, id, correlation_key(contract, &w.scope, &w.correlation)?);
                }
            }
            Some(Pending::Child { child, .. }) => {
                depend(s, id, child.clone());
                if s.runs.get(child).is_some_and(|r| r.status.terminal()) {
                    wake(s, id);
                }
            }
            Some(Pending::Join {
                members, deadline, ..
            }) => {
                at = Some(*deadline);
                for member in members {
                    depend(s, id, member.clone());
                }
            }
            Some(Pending::Retry { deadline }) => at = Some(*deadline),
            None => {}
        },
        RunStatus::Finishing | RunStatus::Cancelling => {
            if crate::settled(s, &r)? {
                wake(s, id)
            } else {
                for child in &r.children {
                    depend(s, id, child.clone());
                }
                let ids = s
                    .intents
                    .values()
                    .filter(|i| i.run == id)
                    .map(|i| i.id.clone())
                    .collect::<Vec<_>>();
                for intent in ids {
                    depend(s, id, intent);
                }
            }
        }
        RunStatus::Blocked => {}
        _ => {}
    }
    if !matches!(
        r.status,
        RunStatus::Finishing | RunStatus::Cancelling | RunStatus::Blocked
    ) {
        // A live run's root is live: a cohort leaves only as a whole.
        let root = s
            .runs
            .get(&r.root)
            .ok_or_else(|| crate::invariant(format!("root {} of run {id} is missing", r.root)))?;
        let end = crate::deadline(
            root.admitted_at_ms,
            s.artifacts[&root.artifact]
                .content
                .playbook
                .limits
                .max_duration_ms,
        )?;
        let own = crate::deadline(
            r.admitted_at_ms,
            s.artifacts[&r.artifact]
                .content
                .playbook
                .limits
                .max_duration_ms,
        )?;
        let end = end.min(own);
        at = Some(at.map_or(end, |a| a.min(end)));
    }
    if was_queued
        && matches!(
            r.status,
            RunStatus::Waiting | RunStatus::Finishing | RunStatus::Cancelling
        )
    {
        wake(s, id);
    }
    if let Some(at_ms) = at {
        timer(
            s,
            TimerIndex {
                key: timerkey,
                run: id.into(),
                at_ms,
                intent: None,
            },
        );
    }
    Ok(())
}
fn refresh_intent(s: &mut State, id: &str) -> Result<()> {
    let was_uncertain = s.indexes.uncertain_contract_membership.remove(id);
    if let Some(key) = &was_uncertain {
        remove_member(&mut s.indexes.uncertain_completion_contracts, key, id);
    }
    let result = refresh_intent_indexes(s, id);
    // A hold through an uncertain completion begins or ends: the events of
    // that contract and scope are judged again (`retention::rejoin`).
    let is_uncertain = s.indexes.uncertain_contract_membership.get(id).cloned();
    // An operation never dispatched holds nothing either way (a membership
    // it loses here is one an older engine recorded): nothing to judge.
    let after = match s.intents.get(id) {
        Some(i) => i.dispatch_sequence,
        // Gone from the State: every event (0).
        None => Some(0),
    };
    if was_uncertain != is_uncertain
        && let Some(after) = after
    {
        // Only events accepted after the dispatch can be its completion.
        for key in was_uncertain.into_iter().chain(is_uncertain) {
            let entry = s.replay_touched.contract_keys.entry(key).or_insert(after);
            *entry = (*entry).min(after);
        }
    }
    result
}
fn refresh_intent_indexes(s: &mut State, id: &str) -> Result<()> {
    let token = format!("intent:{id}");
    if let Some(keys) = s.indexes.run_subscriptions.remove(&token) {
        for key in keys {
            remove_lazy_member(&mut s.indexes.waiters, &key, &token);
            s.replay_touched.wait_keys.insert(key);
        }
    }
    if let Some(memberships) = s.indexes.effect_memberships.remove(id) {
        for (kind, scope) in memberships {
            if let Some(scopes) = s.indexes.effects.get_mut(&kind)
                && let Some(ids) = scopes.get_mut(&scope)
            {
                ids.remove(id);
                if ids.is_empty() {
                    scopes.remove(&scope);
                    s.indexes
                        .effect_item_cursor
                        .remove(&format!("{kind}:{scope}"));
                }
            }
        }
    }
    remove_timer(s, &format!("effect:{id}"));
    let Some(i) = s.intents.get(id).cloned() else {
        s.indexes.completion_ready.remove(id);
        if let Some(root) = s.indexes.intent_roots.remove(id) {
            if let Some(ids) = s.indexes.root_effects.get_mut(&root) {
                ids.remove(id);
            }
            if let Some(ids) = s.indexes.root_intents.get_mut(&root) {
                ids.remove(id);
                if ids.is_empty() {
                    s.indexes.root_intents.remove(&root);
                }
            }
        }
        return Ok(());
    };
    let root = s
        .runs
        .get(&i.run)
        .map(|r| r.root.clone())
        .or_else(|| s.indexes.run_roots.get(&i.run).cloned());
    if let Some(root) = root {
        s.indexes.intent_roots.insert(id.into(), root.clone());
        if !s
            .indexes
            .root_intents
            .get(&root)
            .is_some_and(|ids| ids.contains(id))
        {
            s.indexes
                .root_intents
                .entry(root.clone())
                .or_default()
                .insert(id.into());
        }
        if matches!(
            i.status,
            IntentStatus::Completed | IntentStatus::Rejected | IntentStatus::Cancelled
        ) {
            if let Some(ids) = s.indexes.root_effects.get_mut(&root) {
                ids.remove(id);
            }
        } else {
            s.indexes
                .root_effects
                .entry(root)
                .or_default()
                .insert(id.into());
        }
    }
    if i.completion_event.is_some() {
        s.indexes.completion_ready.remove(id);
    }
    let mut kinds = vec![];
    if i.status == IntentStatus::Prepared {
        kinds.push(EffectQueueKind::Dispatch)
    }
    if i.cancel_requested
        && !i.cancel_dispatched
        && !matches!(
            i.status,
            IntentStatus::Completed | IntentStatus::Rejected | IntentStatus::Cancelled
        )
    {
        kinds.push(EffectQueueKind::Cancel)
    }
    if matches!(i.status, IntentStatus::Dispatched | IntentStatus::Unknown)
        || i.cancel_requested
            && i.cancel_dispatched
            && !matches!(
                i.status,
                IntentStatus::Completed | IntentStatus::Rejected | IntentStatus::Cancelled
            )
    {
        kinds.push(EffectQueueKind::Recovery)
    }
    for kind in kinds {
        s.indexes
            .effects
            .entry(kind.key().into())
            .or_default()
            .entry(i.authority.scope.clone())
            .or_default()
            .insert(id.into());
        s.indexes
            .effect_memberships
            .entry(id.into())
            .or_default()
            .push((kind.key().into(), i.authority.scope.clone()));
    }
    if matches!(i.status, IntentStatus::Dispatched | IntentStatus::Accepted) {
        timer(
            s,
            TimerIndex {
                key: format!("effect:{id}"),
                run: i.run.clone(),
                at_ms: i.deadline_at_ms,
                intent: Some(id.into()),
            },
        );
    }
    if let Some(asynchronous) = &i.command.asynchronous
        && uncertain(&i)
    {
        let event = &s.artifacts[&s.runs[&i.run].artifact]
            .content
            .registry
            .events[&asynchronous.completion_event];
        let key = trigger_key(
            &ContractRef {
                name: event.name.clone(),
                fingerprint: event.fingerprint()?,
            },
            &i.authority.scope,
        )?;
        s.indexes
            .uncertain_completion_contracts
            .entry(key.clone())
            .or_default()
            .insert(id.into());
        s.indexes
            .uncertain_contract_membership
            .insert(id.into(), key);
    }
    if let (Some(a), Some(handle)) = (&i.command.asynchronous, &i.handle)
        && i.completion_event.is_none()
    {
        let event = &s.artifacts[&s.runs[&i.run].artifact]
            .content
            .registry
            .events[&a.completion_event];
        let key = field_key(
            &ContractRef {
                name: event.name.clone(),
                fingerprint: event.fingerprint()?,
            },
            &i.authority.scope,
            &a.correlation_field,
            handle,
        )?;
        let token = format!("intent:{id}");
        s.replay_touched.wait_keys.insert(key.clone());
        s.indexes
            .waiters
            .entry(key.clone())
            .or_default()
            .insert(token.clone());
        s.indexes
            .run_subscriptions
            .entry(token)
            .or_default()
            .push(key.clone());
        if s.indexes.events.get(&key).is_some_and(|e| !e.is_empty()) {
            s.indexes.completion_ready.insert(id.into());
        }
    }
    wake_dependents(s, id);
    mark_run(s, &i.run);
    Ok(())
}
/// Whether `i` (asynchronous) may still be completed by an event its handle
/// does not name: it was dispatched, has no handle and is not settled. A
/// recovery that prepares it again does not end that: the first issue may
/// still answer. An operation never dispatched cannot be completed.
pub(crate) fn uncertain(i: &crate::Intent) -> bool {
    i.dispatch_sequence.is_some()
        && i.handle.is_none()
        && matches!(
            i.status,
            IntentStatus::Prepared | IntentStatus::Dispatched | IntentStatus::Unknown
        )
}
pub(crate) fn sync(s: &mut State) -> Result<()> {
    let intents = std::mem::take(&mut s.indexes.dirty_intents);
    for id in intents {
        refresh_intent(s, &id)?;
    }
    let runs = std::mem::take(&mut s.indexes.dirty_runs);
    for id in runs {
        refresh_run(s, &id)?;
    }
    Ok(())
}
pub(crate) fn rebuild(s: &mut State) -> Result<()> {
    s.catalog_history
        .entry(s.catalog_revision)
        .or_insert_with(|| s.bindings.clone());
    s.indexes = RuntimeIndexes::default();
    let events = s.events.keys().cloned().collect::<Vec<_>>();
    for id in events {
        index_event(s, &id)?;
    }
    let bindings = s.bindings.keys().cloned().collect::<Vec<_>>();
    for id in bindings {
        index_activation(s, &id)?;
    }
    s.indexes.dirty_runs = s.runs.keys().cloned().collect();
    s.indexes.dirty_intents = s.intents.keys().cloned().collect();
    let receipts = s
        .receipts
        .iter()
        .enumerate()
        .map(|(i, r)| (i, r.run.clone(), r.sequence))
        .collect::<Vec<_>>();
    for (i, run, seq) in receipts {
        if let Some(r) = s.runs.get(&run) {
            let at = s.receipts[i].at_ms;
            s.indexes
                .root_last_activity
                .entry(r.root.clone())
                .and_modify(|old| *old = (*old).max(at))
                .or_insert(at);
        }
        s.indexes.receipts.entry(run).or_default().insert(seq, i);
        s.indexes.all_receipts.insert(seq, i);
    }
    sync(s)?;
    s.indexes.initialized = true;
    s.indexes.root_intents_complete = true;
    s.indexes.contract_events_complete = true;
    Ok(())
}
/// Once per namespace written before `root_intents` existed (pre-D10, whose
/// indexes are otherwise initialized): put every live operation into
/// `root_intents` under its root and set `root_intents_complete`. One pass
/// over live operations; operations of a run without a known root are left
/// out (they cannot belong to a cohort). Returns whether it ran.
pub(crate) fn backfill_root_intents(s: &mut State) -> bool {
    if !s.indexes.initialized || s.indexes.root_intents_complete {
        return false;
    }
    let roots: Vec<(String, String)> = s
        .intents
        .values()
        .filter_map(|i| {
            s.runs
                .get(&i.run)
                .map(|r| r.root.clone())
                .or_else(|| s.indexes.run_roots.get(&i.run).cloned())
                .or_else(|| s.indexes.intent_roots.get(&i.id).cloned())
                .map(|root| (i.id.clone(), root))
        })
        .collect();
    for (id, root) in roots {
        if !s
            .indexes
            .root_intents
            .get(&root)
            .is_some_and(|ids| ids.contains(&id))
        {
            s.indexes.root_intents.entry(root).or_default().insert(id);
        }
    }
    s.indexes.root_intents_complete = true;
    true
}
/// Once per namespace written before `Intent::dispatch_sequence` existed:
/// give every live asynchronous operation without a handle that an older
/// engine dispatched the sequence of its own first `dispatch` receipt (its
/// run is live, so its receipts are in the record), so it holds only the
/// events accepted after it. One without such a receipt was never
/// dispatched and holds nothing. Each is marked, so the next sync records
/// its uncertain membership by the current rule. One pass over live
/// operations, plus each candidate's run receipts. Returns whether it ran.
pub(crate) fn backfill_dispatch_sequences(s: &mut State) -> bool {
    if !s.indexes.initialized || s.indexes.dispatch_sequences_complete {
        return false;
    }
    let candidates: Vec<(String, String)> = s
        .intents
        .values()
        .filter(|i| {
            i.command.asynchronous.is_some() && i.handle.is_none() && i.dispatch_sequence.is_none()
        })
        .map(|i| (i.id.clone(), i.run.clone()))
        .collect();
    for (id, run) in candidates {
        let first = s
            .indexes
            .receipts
            .get(&run)
            .into_iter()
            .flat_map(|offsets| offsets.values().copied())
            .filter_map(|offset| s.receipts.get(offset))
            .filter(|r| {
                r.run == run
                    && r.kind == "dispatch"
                    && r.detail.get("operation").and_then(|v| v.as_str()) == Some(id.as_str())
            })
            .map(|r| r.sequence)
            .min();
        if let Some(intent) = s.intents.get_mut(&id) {
            intent.dispatch_sequence = first;
        }
        mark_intent(s, &id);
    }
    s.indexes.dispatch_sequences_complete = true;
    true
}

fn remove_lazy_member(map: &mut LazyMap<String, BTreeSet<String>>, key: &str, member: &str) {
    if let Some(values) = map.get_mut(key) {
        values.remove(member);
        if values.is_empty() {
            map.remove(key);
        }
    }
}
fn remove_member(map: &mut BTreeMap<String, BTreeSet<String>>, key: &str, member: &str) {
    if let Some(values) = map.get_mut(key) {
        values.remove(member);
        if values.is_empty() {
            map.remove(key);
        }
    }
}

/// Runs, intents and receipts of `root`, in the order of a full-state scan
/// (runs and intents by id, receipts by position), using root-scoped indexes
/// plus this transaction's dirty sets. Returns `None` before initialization.
#[allow(clippy::type_complexity)]
pub(crate) fn root_history(
    s: &State,
    root: &str,
) -> Option<(Vec<usize>, Vec<String>, Vec<String>)> {
    if !s.indexes.initialized {
        return None;
    }
    let in_root = |run: &str| s.runs.get(run).is_some_and(|r| r.root == root);
    let mut runs: BTreeSet<String> = s
        .indexes
        .root_members
        .get(root)
        .cloned()
        .unwrap_or_default();
    runs.extend(s.indexes.dirty_runs.iter().cloned());
    runs.retain(|id| in_root(id));
    let mut intents: BTreeSet<String> = s
        .indexes
        .root_intents
        .get(root)
        .cloned()
        .unwrap_or_default();
    intents.extend(s.indexes.dirty_intents.iter().cloned());
    intents.retain(|id| s.intents.get(id).is_some_and(|i| in_root(&i.run)));
    let mut receipts = BTreeSet::new();
    for run in &runs {
        if let Some(offsets) = s.indexes.receipts.get(run) {
            receipts.extend(
                offsets
                    .values()
                    .copied()
                    .filter(|i| s.receipts.get(*i).is_some_and(|r| &r.run == run)),
            );
        }
    }
    Some((
        receipts.into_iter().collect(),
        intents.into_iter().collect(),
        runs.into_iter().collect(),
    ))
}
/// Non-terminal intents of `root` (indexed plus dirty), filtered by current state.
pub(crate) fn root_open_intents(s: &State, root: &str) -> Option<Vec<String>> {
    if !s.indexes.initialized {
        return None;
    }
    let mut ids: BTreeSet<String> = s
        .indexes
        .root_effects
        .get(root)
        .cloned()
        .unwrap_or_default();
    ids.extend(s.indexes.dirty_intents.iter().cloned());
    Some(
        ids.into_iter()
            .filter(|id| {
                s.intents.get(id).is_some_and(|i| {
                    s.runs.get(&i.run).is_some_and(|x| x.root == root)
                        && !matches!(
                            i.status,
                            IntentStatus::Completed
                                | IntentStatus::Rejected
                                | IntentStatus::Cancelled
                        )
                })
            })
            .collect(),
    )
}
