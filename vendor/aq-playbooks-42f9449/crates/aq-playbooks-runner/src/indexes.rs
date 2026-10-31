//! Durable bounded discovery indexes. These are maintained in the same CAS as
//! authoritative state; adapters may mirror them into indexed native tables.
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
    pub root_members: BTreeMap<String, BTreeSet<String>>,
    pub root_active: BTreeMap<String, BTreeSet<String>>,
    pub root_effects: BTreeMap<String, BTreeSet<String>>,
    pub root_last_activity: BTreeMap<String, i64>,
    pub run_roots: BTreeMap<String, String>,
    pub intent_roots: BTreeMap<String, String>,
    pub effects: BTreeMap<String, BTreeMap<String, BTreeSet<String>>>,
    pub effect_memberships: BTreeMap<String, Vec<(String, String)>>,
    pub timers: BTreeMap<String, TimerIndex>,
    pub due: BTreeMap<i64, BTreeSet<String>>,
    pub run_timer: BTreeMap<String, String>,
    pub events: BTreeMap<String, BTreeMap<u64, String>>,
    pub event_memberships: BTreeMap<String, Vec<String>>,
    pub waiters: BTreeMap<String, BTreeSet<String>>,
    pub run_subscriptions: BTreeMap<String, Vec<String>>,
    pub dependents: BTreeMap<String, BTreeSet<String>>,
    pub run_dependencies: BTreeMap<String, Vec<String>>,
    pub completion_ready: BTreeSet<String>,
    pub uncertain_completion_contracts: BTreeMap<String, BTreeSet<String>>,
    pub uncertain_contract_membership: BTreeMap<String, String>,
    pub dirty_runs: BTreeSet<String>,
    pub dirty_intents: BTreeSet<String>,
    pub runs: BTreeSet<String>,
    pub receipts: BTreeMap<String, BTreeMap<u64, usize>>,
    pub all_receipts: BTreeMap<u64, usize>,
    pub triggers: BTreeMap<String, BTreeSet<String>>,
    pub activation_triggers: BTreeMap<String, Vec<String>>,
    pub initialized: bool,
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
                for (at, ids) in self.due.range(..=*now_ms) {
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
                    Some(&self.all_receipts)
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
            remove_member(&mut s.indexes.waiters, &key, id);
        }
    }
    if let Some(ids) = s.indexes.run_dependencies.remove(id) {
        for dep in ids {
            remove_member(&mut s.indexes.dependents, &dep, id);
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
            if crate::settled(s, &r) {
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
        let root = &s.runs[&r.root];
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
    if let Some(key) = s.indexes.uncertain_contract_membership.remove(id) {
        remove_member(&mut s.indexes.uncertain_completion_contracts, &key, id);
    }
    let token = format!("intent:{id}");
    if let Some(keys) = s.indexes.run_subscriptions.remove(&token) {
        for key in keys {
            remove_member(&mut s.indexes.waiters, &key, &token);
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
        if let Some(root) = s.indexes.intent_roots.remove(id)
            && let Some(ids) = s.indexes.root_effects.get_mut(&root)
        {
            ids.remove(id);
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
        && i.handle.is_none()
        && matches!(i.status, IntentStatus::Dispatched | IntentStatus::Unknown)
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
    Ok(())
}

fn remove_member(map: &mut BTreeMap<String, BTreeSet<String>>, key: &str, member: &str) {
    if let Some(values) = map.get_mut(key) {
        values.remove(member);
        if values.is_empty() {
            map.remove(key);
        }
    }
}
