use crate::backend;
use crate::profiling::ObservedTransaction;
#[cfg(test)]
use aq_playbooks_runner::inspection;
use aq_playbooks_runner::{EffectQueueKind, IndexItem, IndexPage, IndexQuery, State, StorageError};
use postgres::Client;
use serde_json::Value;
#[cfg(test)]
use serde_json::json;

#[cfg(test)]
fn record(
    kind: &str,
    lookup: &str,
    key: &str,
    scope: &str,
    sequence: u64,
    at: Option<i64>,
) -> Value {
    json!({"kind":kind,"lookup_key":lookup,"item_key":key,"scope":scope,"sequence":sequence,"at_ms":at})
}
#[cfg(test)]
fn native_rows(state: &State) -> Vec<Value> {
    let mut rows = vec![];
    let idx = &state.indexes;
    if let Some(cursor) = &idx.scope_cursor {
        rows.push(record("cursor", "ready", cursor, "", 0, None));
    }
    for (kind, cursor) in &idx.effect_cursor {
        rows.push(record("cursor", kind, cursor, "", 0, None));
    }
    for (kind_scope, cursor) in &idx.effect_item_cursor {
        rows.push(record("item_cursor", kind_scope, cursor, "", 0, None));
    }
    for (scope, queue) in &idx.ready {
        if let Some((sequence, id)) = queue.first_key_value() {
            rows.push(record("ready", "", id, scope, *sequence, None));
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
                rows.push(record(&format!("effect:{kind}"), "", id, scope, 0, None));
            }
        }
    }
    for id in &idx.completion_ready {
        rows.push(record(
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
        rows.push(record(
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
            rows.push(record(
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
            rows.push(record(
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
        rows.push(record(
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
            rows.push(record(
                "receipt",
                run,
                &offset.to_string(),
                "",
                *seq,
                state.receipts.get(*offset).map(|r| r.at_ms),
            ));
        }
    }
    for (seq, offset) in &idx.all_receipts {
        rows.push(record(
            "all_receipt",
            "",
            &offset.to_string(),
            "",
            *seq,
            state.receipts.get(*offset).map(|r| r.at_ms),
        ));
    }
    rows
}
#[cfg(test)]
fn public_rows(state: &State) -> Vec<Value> {
    inspection::rows(state).into_iter().map(|row|json!({"kind":row.kind.key(),"item_key":row.key,"run_id":row.run,"sequence":row.sequence,"public_value":row.value})).collect()
}
#[cfg(test)]
fn delta(previous: Vec<Value>, next: Vec<Value>, keys: &[&str]) -> (Vec<Value>, Vec<Value>) {
    let keyed = |rows: Vec<Value>| -> std::collections::BTreeMap<Vec<String>, Value> {
        rows.into_iter()
            .map(|row| {
                (
                    keys.iter()
                        .map(|key| row[*key].as_str().expect("internal row key").to_string())
                        .collect(),
                    row,
                )
            })
            .collect()
    };
    let previous = keyed(previous);
    let next = keyed(next);
    let changed = next
        .iter()
        .filter(|(key, row)| previous.get(*key) != Some(*row))
        .map(|(_, row)| row.clone())
        .collect();
    let deleted = previous
        .keys()
        .filter(|key| !next.contains_key(*key))
        .map(|key| {
            let fields: serde_json::Map<_, _> = keys
                .iter()
                .zip(key)
                .map(|(name, value)| ((*name).into(), Value::String(value.clone())))
                .collect();
            Value::Object(fields)
        })
        .collect();
    (changed, deleted)
}
pub(crate) fn sync(
    tx: &mut ObservedTransaction<'_>,
    namespace: &str,
    previous: &State,
    next: &State,
) -> Result<(), StorageError> {
    let (changed, deleted) = crate::projection::native_delta(previous, next);
    if !deleted.is_empty() {
        tx.execute("DELETE FROM aq_playbooks.runtime_index old USING jsonb_to_recordset($2) AS d(kind text,lookup_key text,item_key text) WHERE old.namespace=$1 AND (old.kind,old.lookup_key,old.item_key)=(d.kind,d.lookup_key,d.item_key)",&[&namespace,&Value::Array(deleted)]).map_err(backend)?;
    }
    if !changed.is_empty() {
        tx.execute("INSERT INTO aq_playbooks.runtime_index(namespace,kind,lookup_key,item_key,scope,sequence,at_ms) SELECT $1,kind,lookup_key,item_key,scope,sequence,at_ms FROM jsonb_to_recordset($2) AS d(kind text,lookup_key text,item_key text,scope text,sequence bigint,at_ms bigint) ON CONFLICT(namespace,kind,lookup_key,item_key) DO UPDATE SET scope=EXCLUDED.scope,sequence=EXCLUDED.sequence,at_ms=EXCLUDED.at_ms",&[&namespace,&Value::Array(changed)]).map_err(backend)?;
    }
    let (changed, deleted) = crate::projection::public_delta(previous, next);
    if !deleted.is_empty() {
        tx.execute("DELETE FROM aq_playbooks.inspection_rows old USING jsonb_to_recordset($2) AS d(kind text,item_key text) WHERE old.namespace=$1 AND (old.kind,old.item_key)=(d.kind,d.item_key)",&[&namespace,&Value::Array(deleted)]).map_err(backend)?;
    }
    if !changed.is_empty() {
        tx.execute("INSERT INTO aq_playbooks.inspection_rows(namespace,kind,item_key,run_id,sequence,public_value) SELECT $1,kind,item_key,run_id,sequence,public_value FROM jsonb_to_recordset($2) AS d(kind text,item_key text,run_id text,sequence bigint,public_value jsonb) ON CONFLICT(namespace,kind,item_key) DO UPDATE SET run_id=EXCLUDED.run_id,sequence=EXCLUDED.sequence,public_value=EXCLUDED.public_value",&[&namespace,&Value::Array(changed)]).map_err(backend)?;
    }
    Ok(())
}

pub(crate) fn query(
    client: &mut Client,
    namespace: &str,
    query: &IndexQuery,
) -> Result<IndexPage, StorageError> {
    let requested = match query {
        IndexQuery::Ready { limit, .. }
        | IndexQuery::DueTimers { limit, .. }
        | IndexQuery::Effects { limit, .. }
        | IndexQuery::CorrelatedEvents { limit, .. }
        | IndexQuery::Waiters { limit, .. }
        | IndexQuery::Runs { limit, .. }
        | IndexQuery::Receipts { limit, .. } => *limit,
    };
    let limit = requested.min(1000) as i64;
    if limit == 0 {
        return Ok(IndexPage::default());
    }
    let rows=match query {
        IndexQuery::Ready{scope_after,..}=>{client.query("SELECT item_key,scope,sequence,at_ms FROM (SELECT item_key,scope,sequence,at_ms FROM aq_playbooks.runtime_index WHERE namespace=$1 AND kind='ready') q ORDER BY (scope>COALESCE($2::text,(SELECT item_key FROM aq_playbooks.runtime_index WHERE namespace=$1 AND kind='cursor' AND lookup_key='ready'),'')) DESC,scope LIMIT $3",&[&namespace,scope_after,&limit])},
        IndexQuery::Effects{kind:EffectQueueKind::Completion,..}=>client.query("SELECT item_key,scope,sequence,at_ms FROM aq_playbooks.runtime_index WHERE namespace=$1 AND kind='effect:completion' ORDER BY item_key LIMIT $2",&[&namespace,&limit]),
        IndexQuery::Effects{kind,scope_after,..}=>{let cursor_kind=kind.key();let kind=format!("effect:{cursor_kind}");client.query("SELECT item_key,scope,sequence,at_ms FROM (SELECT item_key,scope,sequence,at_ms FROM aq_playbooks.runtime_index WHERE namespace=$1 AND kind=$2) q ORDER BY (scope>COALESCE($3::text,(SELECT item_key FROM aq_playbooks.runtime_index WHERE namespace=$1 AND kind='cursor' AND lookup_key=$5),'')) DESC,scope LIMIT $4",&[&namespace,&kind,scope_after,&limit,&cursor_kind])},
        IndexQuery::DueTimers{now_ms,..}=>client.query("SELECT item_key,scope,sequence,at_ms FROM aq_playbooks.runtime_index WHERE namespace=$1 AND kind='timer' AND at_ms<=$2 ORDER BY at_ms,item_key LIMIT $3",&[&namespace,now_ms,&limit]),
        IndexQuery::CorrelatedEvents{key,after_sequence,..}=>{let after=i64::try_from(*after_sequence).map_err(|_|backend("index sequence exhausted"))?;client.query("SELECT item_key,scope,sequence,at_ms FROM aq_playbooks.runtime_index WHERE namespace=$1 AND kind='event' AND lookup_key=$2 AND sequence>$3 ORDER BY sequence,item_key LIMIT $4",&[&namespace,key,&after,&limit])},
        IndexQuery::Waiters{key,..}=>client.query("SELECT item_key,scope,sequence,at_ms FROM aq_playbooks.runtime_index WHERE namespace=$1 AND kind='waiter' AND lookup_key=$2 ORDER BY item_key LIMIT $3",&[&namespace,key,&limit]),
        IndexQuery::Runs{after,..}=>client.query("SELECT item_key,scope,sequence,at_ms FROM aq_playbooks.runtime_index WHERE namespace=$1 AND kind='run' AND ($2::text IS NULL OR item_key>$2) ORDER BY item_key LIMIT $3",&[&namespace,after,&limit]),
        IndexQuery::Receipts{run,after_sequence,..}=>{let after=i64::try_from(*after_sequence).map_err(|_|backend("index sequence exhausted"))?;let kind=if run.is_some(){"receipt"}else{"all_receipt"};let lookup=run.as_deref().unwrap_or("");client.query("SELECT item_key,scope,sequence,at_ms FROM aq_playbooks.runtime_index WHERE namespace=$1 AND kind=$2 AND lookup_key=$3 AND sequence>$4 ORDER BY sequence,item_key LIMIT $5",&[&namespace,&kind,&lookup,&after,&limit])},
    }.map_err(backend)?;
    let items: Vec<_> = rows
        .into_iter()
        .map(|row| IndexItem {
            key: row.get(0),
            scope: row.get(1),
            sequence: row.get::<_, i64>(2) as u64,
            at_ms: row.get(3),
        })
        .collect();
    let next = items.last().map(|item| match query {
        IndexQuery::Ready { .. } | IndexQuery::Effects { .. } => item.scope.clone(),
        IndexQuery::Receipts { .. } | IndexQuery::CorrelatedEvents { .. } => {
            item.sequence.to_string()
        }
        _ => item.key.clone(),
    });
    Ok(IndexPage { items, next })
}

#[cfg(test)]
mod projection_tests {
    use super::*;
    use aq_playbooks_runner::{Intent, Receipt, Run, indexes::TimerIndex};
    use std::collections::{BTreeMap, BTreeSet};
    fn fixture() -> State {
        let run:Run=serde_json::from_value(json!({"catalog_revision":0,"id":"run-a","root":"run-a","parent":null,"origin":null,"artifact":"artifact","entry":"main","dependencies":{},"authority":{"actor":"actor","scope":"a","capabilities":[]},"step":"return","status":"completed","input":"private","variables":{},"outcome":"ok","output":null,"reason":null,"admitted_at_ms":1,"visits":1,"depth":0,"children":[],"pending":null,"loops":[],"attempts":{},"pending_return":null,"pending_failure":false,"terminal_sequence":2})).unwrap();
        let intent:Intent=serde_json::from_value(json!({"id":"intent-1","run":"run-a","step":"invoke","invocation":1,"deadline_at_ms":1000,"command":{"name":"command","version":"1","input":{"type":"null"},"outcomes":{"ok":{"type":"null"}},"capabilities":[],"scopes":["a"],"effect":"mutation","recovery":"keyed_idempotent","asynchronous":null,"cancellation":false,"public_result_fields":[],"sensitive_input_fields":[],"sensitive_output_fields":[],"max_payload_bytes":1024,"deadline_ms":1000},"input":null,"input_digest":"private","authority":{"actor":"actor","scope":"a","capabilities":[]},"status":"prepared","cancel_requested":false,"cancel_dispatched":false,"handle":null,"result":null,"reason":null,"late_results":[],"completion_event":null})).unwrap();
        let mut s = State::default();
        s.runs.insert(run.id.clone(), run);
        s.intents.insert(intent.id.clone(), intent.clone());
        let mut second = intent;
        second.id = "intent-2".into();
        s.intents.insert(second.id.clone(), second);
        s.receipts = vec![
            Receipt {
                sequence: 1,
                run: "run-a".into(),
                step: "first".into(),
                at_ms: 1,
                kind: "intent".into(),
                detail: json!({"operation":"intent-1","private":"secret"}),
            },
            Receipt {
                sequence: 10,
                run: "run-a".into(),
                step: "second".into(),
                at_ms: 10,
                kind: "unclassified".into(),
                detail: json!({"private":"secret"}),
            },
            Receipt {
                sequence: 2,
                run: "run-a".into(),
                step: "third".into(),
                at_ms: 2,
                kind: "terminal".into(),
                detail: json!({"status":"completed","outcome":"ok"}),
            },
        ];
        let idx = &mut s.indexes;
        idx.scope_cursor = Some("a".into());
        idx.effect_cursor.insert("dispatch".into(), "b".into());
        idx.effect_item_cursor
            .insert("dispatch:a".into(), "intent-1".into());
        idx.ready
            .insert("a".into(), BTreeMap::from([(1, "run-a".into())]));
        idx.effects.insert(
            "dispatch".into(),
            BTreeMap::from([(
                "a".into(),
                BTreeSet::from(["intent-1".into(), "intent-2".into()]),
            )]),
        );
        idx.completion_ready.insert("intent-2".into());
        idx.timers.insert(
            "timer".into(),
            TimerIndex {
                key: "timer".into(),
                run: "run-a".into(),
                at_ms: 100,
                intent: None,
            },
        );
        idx.events.insert(
            "correlation".into(),
            BTreeMap::from([(2, "event-1".into())]),
        );
        idx.waiters
            .insert("correlation".into(), BTreeSet::from(["run-a".into()]));
        idx.runs.insert("run-a".into());
        idx.receipts
            .insert("run-a".into(), BTreeMap::from([(1, 0), (10, 1), (2, 2)]));
        idx.all_receipts = BTreeMap::from([(1, 0), (10, 1), (2, 2)]);
        s
    }
    fn equivalent(previous: &State, next: &State) {
        assert_eq!(
            crate::projection::native_delta(previous, next),
            delta(
                native_rows(previous),
                native_rows(next),
                &["kind", "lookup_key", "item_key"]
            )
        );
        assert_eq!(
            crate::projection::public_delta(previous, next),
            delta(
                public_rows(previous),
                public_rows(next),
                &["kind", "item_key"]
            )
        );
    }
    #[test]
    fn borrowed_deltas_match_reference_for_empty_unchanged_and_materialization() {
        let s = fixture();
        equivalent(&State::default(), &s);
        equivalent(&s, &s);
        equivalent(&s, &State::default());
    }
    #[test]
    fn borrowed_deltas_match_reference_for_scopes_removals_offsets_and_private_edits() {
        let old = fixture();
        for case in 0..9 {
            let mut next = old.clone();
            match case {
                0 => next.runs.get_mut("run-a").unwrap().authority.scope = "changed".into(),
                1 => {
                    next.runs.clear();
                }
                2 => {
                    next.receipts.remove(0);
                }
                3 => next.receipts.reverse(),
                4 => {
                    next.runs.get_mut("run-a").unwrap().input = json!("private changed");
                    next.receipts[1].detail = json!({"private":"different secret"});
                }
                5 => {
                    next.intents.get_mut("intent-1").unwrap().invocation += 1;
                }
                6 => {
                    next.indexes
                        .effect_item_cursor
                        .insert("dispatch:a".into(), "intent-2".into());
                }
                7 => {
                    next.runs.get_mut("run-a").unwrap().reason = Some("private reason".into());
                }
                8 => {
                    next.indexes.all_receipts.insert(100, 0);
                    next.indexes
                        .receipts
                        .get_mut("run-a")
                        .unwrap()
                        .insert(100, 0);
                }
                _ => unreachable!(),
            }
            equivalent(&old, &next);
        }
    }
    #[test]
    fn borrowed_deltas_preserve_last_duplicate_identity_and_redaction_semantics() {
        let old = fixture();
        let mut next = old.clone();
        let mut duplicate = next.runs["run-a"].clone();
        duplicate.status = aq_playbooks_runner::RunStatus::Failed;
        next.runs.insert("zz-alias".into(), duplicate);
        let mut receipt = next.receipts[0].clone();
        receipt.detail = json!({"operation":"new","secret":"never project"});
        next.receipts.push(receipt);
        next.indexes
            .ready
            .insert("z".into(), BTreeMap::from([(999, "run-a".into())]));
        next.indexes.effects.insert(
            "completion".into(),
            BTreeMap::from([("a".into(), BTreeSet::from(["intent-2".into()]))]),
        );
        equivalent(&old, &next);
        let (changed, _) = crate::projection::public_delta(&old, &next);
        assert!(
            !serde_json::to_string(&changed)
                .unwrap()
                .contains("never project")
        );
    }
}

#[cfg(test)]
mod projection_field_tests {
    use super::*;
    fn candidates(v: &Value) -> Vec<Value> {
        let mut out = vec![Value::Null, json!("changed"), json!(true), json!(1)];
        match v {
            Value::String(s) => out.push(json!(format!("{s}-changed"))),
            Value::Number(n) => {
                if let Some(n) = n.as_u64() {
                    out.push(json!(n.saturating_add(1)));
                }
            }
            Value::Array(a) => {
                let mut a = a.clone();
                a.push(json!("changed"));
                out.push(Value::Array(a));
            }
            Value::Object(o) => {
                let mut o = o.clone();
                o.insert("changed".into(), json!("value"));
                out.push(Value::Object(o));
            }
            _ => {}
        }
        out
    }
    #[test]
    fn source_field_mutations_cannot_change_projection_without_being_detected() {
        // Golden source includes all serialized fields, including private ones.
        // Valid primitive mutations exercise the quick-equivalence gate against
        // the existing public sanitizer instead of reasserting its field list.
        let run = serde_json::json!({"catalog_revision":0,"id":"r","root":"r","parent":null,"origin":null,"artifact":"a","entry":"main","dependencies":{},"authority":{"actor":"a","scope":"s","capabilities":[]},"step":"end","status":"completed","input":null,"variables":{},"outcome":"ok","output":null,"reason":null,"admitted_at_ms":0,"visits":1,"depth":0,"children":[],"pending":null,"loops":[],"attempts":{},"pending_return":null,"pending_failure":false,"terminal_sequence":1});
        let mut old = State::default();
        old.runs
            .insert("r".into(), serde_json::from_value(run.clone()).unwrap());
        let mut checked = 0;
        for (field, value) in run.as_object().unwrap() {
            for replacement in candidates(value) {
                let mut changed = run.clone();
                changed[field] = replacement;
                if let Ok(run) = serde_json::from_value::<aq_playbooks_runner::Run>(changed) {
                    let mut next = old.clone();
                    next.runs.insert("r".into(), run);
                    assert_eq!(
                        crate::projection::public_delta(&old, &next),
                        delta(public_rows(&old), public_rows(&next), &["kind", "item_key"]),
                        "field {field}"
                    );
                    checked += 1;
                }
            }
        }
        assert!(checked > 30);
    }
}
