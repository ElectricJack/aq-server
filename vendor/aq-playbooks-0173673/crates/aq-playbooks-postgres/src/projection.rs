//! Borrowed projection comparison. Unchanged records never materialize public
//! JSON or clone stored native row strings. Temporary maps remain O(history).
//! Changed-source candidates use canonical public sanitizers before actual
//! changed/deleted rows enter SQL. This is not O(1) persistence.
use aq_playbooks_runner::{Intent, Receipt, Run, State, inspection};
use serde_json::{Value, json};
use std::{borrow::Cow, collections::BTreeMap};
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct NativeKey<'a>(Cow<'a, str>, Cow<'a, str>, Cow<'a, str>);
#[derive(Debug, PartialEq, Eq)]
struct NativeFields<'a> {
    scope: Cow<'a, str>,
    sequence: u64,
    at: Option<i64>,
}
struct BorrowedNativeRow<'a> {
    key: NativeKey<'a>,
    fields: NativeFields<'a>,
}
#[derive(Default)]
struct NativeRows<'a>(BTreeMap<NativeKey<'a>, NativeFields<'a>>);
impl<'a> NativeRows<'a> {
    fn push(&mut self, row: BorrowedNativeRow<'a>) {
        self.0.insert(row.key, row.fields);
    }
}
fn native_json(k: &NativeKey<'_>, f: &NativeFields<'_>) -> Value {
    json!({"kind":k.0,"lookup_key":k.1,"item_key":k.2,"scope":f.scope,"sequence":f.sequence,"at_ms":f.at})
}
pub(crate) fn native_delta(previous: &State, next: &State) -> (Vec<Value>, Vec<Value>) {
    let previous = native_rows(previous);
    let next = native_rows(next);
    let changed = next
        .0
        .iter()
        .filter(|(key, row)| previous.0.get(*key) != Some(*row))
        .map(|(key, row)| native_json(key, row))
        .collect();
    let deleted = previous
        .0
        .keys()
        .filter(|key| !next.0.contains_key(*key))
        .map(|key| json!({"kind":key.0,"lookup_key":key.1,"item_key":key.2}))
        .collect();
    (changed, deleted)
}
fn borrowed_record<'a>(
    kind: impl Into<std::borrow::Cow<'a, str>>,
    lookup: impl Into<std::borrow::Cow<'a, str>>,
    key: impl Into<std::borrow::Cow<'a, str>>,
    scope: impl Into<std::borrow::Cow<'a, str>>,
    sequence: u64,
    at: Option<i64>,
) -> BorrowedNativeRow<'a> {
    BorrowedNativeRow {
        key: NativeKey(kind.into(), lookup.into(), key.into()),
        fields: NativeFields {
            scope: scope.into(),
            sequence,
            at,
        },
    }
}
fn native_rows(state: &State) -> NativeRows<'_> {
    let mut rows = NativeRows::default();
    let idx = &state.indexes;
    if let Some(cursor) = &idx.scope_cursor {
        rows.push(borrowed_record("cursor", "ready", cursor, "", 0, None));
    }
    for (kind, cursor) in &idx.effect_cursor {
        rows.push(borrowed_record("cursor", kind, cursor, "", 0, None));
    }
    for (kind_scope, cursor) in &idx.effect_item_cursor {
        rows.push(borrowed_record(
            "item_cursor",
            kind_scope,
            cursor,
            "",
            0,
            None,
        ));
    }
    for (scope, queue) in &idx.ready {
        if let Some((sequence, id)) = queue.first_key_value() {
            rows.push(borrowed_record("ready", "", id, scope, *sequence, None));
        }
    }
    for (kind, scopes) in &idx.effects {
        for (scope, ids) in scopes {
            let cursor = idx.effect_item_cursor.get(&format!("{kind}:{scope}"));
            let selected = cursor
                .and_then(|cursor| {
                    ids.range((
                        std::ops::Bound::Excluded(cursor.clone()),
                        std::ops::Bound::Unbounded,
                    ))
                    .next()
                })
                .or_else(|| ids.first());
            if let Some(id) = selected {
                rows.push(borrowed_record(
                    format!("effect:{kind}"),
                    "",
                    id,
                    scope,
                    0,
                    None,
                ));
            }
        }
    }
    for id in &idx.completion_ready {
        rows.push(borrowed_record(
            "effect:completion",
            "",
            id,
            state
                .intents
                .get(id)
                .map(|i| i.authority.scope.as_str())
                .unwrap_or(""),
            0,
            None,
        ));
    }
    for (key, timer) in &idx.timers {
        rows.push(borrowed_record(
            "timer",
            "",
            key,
            state
                .runs
                .get(&timer.run)
                .map(|r| r.authority.scope.as_str())
                .unwrap_or(""),
            0,
            Some(timer.at_ms),
        ));
    }
    for (key, events) in &idx.events {
        for (seq, id) in events {
            rows.push(borrowed_record(
                "event",
                key,
                id,
                state
                    .events
                    .get(id)
                    .map(|e| e.envelope.scope.as_str())
                    .unwrap_or(""),
                *seq,
                state.events.get(id).map(|e| e.accepted_at_ms),
            ));
        }
    }
    for (key, ids) in &idx.waiters {
        for id in ids {
            rows.push(borrowed_record(
                "waiter",
                key,
                id,
                state
                    .runs
                    .get(id)
                    .map(|r| r.authority.scope.as_str())
                    .unwrap_or(""),
                0,
                None,
            ));
        }
    }
    for id in &idx.runs {
        rows.push(borrowed_record(
            "run",
            "",
            id,
            state
                .runs
                .get(id)
                .map(|r| r.authority.scope.as_str())
                .unwrap_or(""),
            0,
            None,
        ));
    }
    for (run, receipts) in &idx.receipts {
        for (seq, offset) in receipts {
            rows.push(borrowed_record(
                "receipt",
                run,
                offset.to_string(),
                "",
                *seq,
                state.receipts.get(*offset).map(|r| r.at_ms),
            ));
        }
    }
    for (seq, offset) in &idx.all_receipts {
        rows.push(borrowed_record(
            "all_receipt",
            "",
            offset.to_string(),
            "",
            *seq,
            state.receipts.get(*offset).map(|r| r.at_ms),
        ));
    }
    rows
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct PublicKey<'a>(&'static str, Cow<'a, str>);
enum Source<'a> {
    Run(&'a Run),
    Intent(&'a Intent),
    Receipt(&'a Receipt),
}
struct PublicRow<'a> {
    run: &'a str,
    sequence: u64,
    source: Source<'a>,
}
fn public_rows(state: &State) -> BTreeMap<PublicKey<'_>, PublicRow<'_>> {
    let mut rows = BTreeMap::new();
    for r in state.runs.values() {
        rows.insert(
            PublicKey("runs", Cow::Borrowed(&r.id)),
            PublicRow {
                run: &r.id,
                sequence: 0,
                source: Source::Run(r),
            },
        );
    }
    for i in state.intents.values() {
        rows.insert(
            PublicKey("effects", Cow::Borrowed(&i.id)),
            PublicRow {
                run: &i.run,
                sequence: i.invocation,
                source: Source::Intent(i),
            },
        );
    }
    for r in &state.receipts {
        rows.insert(
            PublicKey("receipts", Cow::Owned(format!("{:020}", r.sequence))),
            PublicRow {
                run: &r.run,
                sequence: r.sequence,
                source: Source::Receipt(r),
            },
        );
    }
    rows
}
fn same_source(a: &Source<'_>, b: &Source<'_>) -> bool {
    match (a, b) {
        (Source::Run(a), Source::Run(b)) => {
            a.id == b.id
                && a.artifact == b.artifact
                && a.entry == b.entry
                && a.step == b.step
                && a.status == b.status
                && a.outcome == b.outcome
                && a.reason.is_some() == b.reason.is_some()
                && a.root == b.root
                && a.parent == b.parent
                && a.origin == b.origin
                && a.children == b.children
                && a.dependencies == b.dependencies
                && a.visits == b.visits
                && a.admitted_at_ms == b.admitted_at_ms
                && match (&a.pending, &b.pending) {
                    (None, None) => true,
                    (Some(a), Some(b)) => {
                        match (serde_json::to_value(a), serde_json::to_value(b)) {
                            (Ok(a), Ok(b)) => a == b,
                            _ => false,
                        }
                    }
                    _ => false,
                }
        }
        (Source::Intent(a), Source::Intent(b)) => {
            a.id == b.id
                && a.run == b.run
                && a.step == b.step
                && a.command == b.command
                && a.status == b.status
                && a.handle == b.handle
                && a.reason.is_some() == b.reason.is_some()
                && a.deadline_at_ms == b.deadline_at_ms
                && a.cancel_requested == b.cancel_requested
                && a.cancel_dispatched == b.cancel_dispatched
        }
        (Source::Receipt(a), Source::Receipt(b)) => {
            a.sequence == b.sequence
                && a.run == b.run
                && a.step == b.step
                && a.at_ms == b.at_ms
                && a.kind == b.kind
                && a.detail == b.detail
        }
        _ => false,
    }
}
fn public_json(k: &PublicKey<'_>, r: &PublicRow<'_>) -> Value {
    let value = match r.source {
        Source::Run(run) => inspection::public_run(run),
        Source::Intent(intent) => inspection::public_intent(intent),
        Source::Receipt(receipt) => inspection::public_receipt(receipt),
    };
    json!({"kind":k.0,"item_key":k.1,"run_id":r.run,"sequence":r.sequence,"public_value":value})
}
pub(crate) fn public_delta(previous: &State, next: &State) -> (Vec<Value>, Vec<Value>) {
    let previous = public_rows(previous);
    let next = public_rows(next);
    let mut changed = vec![];
    for (key, row) in &next {
        if let Some(old) = previous.get(key) {
            if old.run == row.run
                && old.sequence == row.sequence
                && same_source(&old.source, &row.source)
            {
                continue;
            }
            let new = public_json(key, row);
            if public_json(key, old) != new {
                changed.push(new)
            }
        } else {
            changed.push(public_json(key, row))
        }
    }
    let deleted = previous
        .keys()
        .filter(|key| !next.contains_key(*key))
        .map(|key| json!({"kind":key.0,"item_key":key.1}))
        .collect();
    (changed, deleted)
}
