//! Read-set certificate for one canonical no-consumer/no-waiter admission.
//! This module NEVER implements event transition rules. It invokes Engine unchanged.
use super::tree::{Link, Result, Tree};
use aq_playbooks_model::{EventEnvelope, digest};
use aq_playbooks_runner::{storage::*, *};
use serde_json::{Value, json};
use std::collections::BTreeSet;

// Explicit schema tripwire. Any new State field requires an access review.
const STATE_FIELDS: &str = "artifacts authority_revocations bindings budgets catalog_history catalog_history_at_ms catalog_revision events execution_denied indexes intents receipts recovery_requests retention runs sequence waits";
const RETENTION_FIELDS: &str = "artifact_disposals disposals event_epochs event_expires_at_ms event_tombstones expired_correlations gc_cursor last_gc_at_ms policy producer_epochs";
const INDEX_FIELDS: &str = "activation_triggers all_receipts completion_ready dependents dirty_intents dirty_runs dispatch_turn due effect_cursor effect_item_cursor effect_memberships effects event_memberships events initialized intent_roots ready ready_tokens receipts root_active root_effects root_last_activity root_members run_dependencies run_roots run_subscriptions run_timer runs scope_cursor timers triggers uncertain_completion_contracts uncertain_contract_membership waiters";
fn fields(value: &Value, expected: &str) -> Result<()> {
    let actual: BTreeSet<&str> = value
        .as_object()
        .ok_or("expected object")?
        .keys()
        .map(String::as_str)
        .collect();
    if actual != expected.split_whitespace().collect() {
        return Err("schema changed: read-set certificate requires review".into());
    }
    Ok(())
}
fn assign(value: &mut Value, path: &[&str], new: Value) {
    if path.is_empty() {
        *value = new;
        return;
    }
    if !value.is_object() {
        *value = json!({});
    }
    assign(
        value
            .as_object_mut()
            .unwrap()
            .entry(path[0])
            .or_insert(Value::Null),
        &path[1..],
        new,
    );
}
fn copy(tree: &mut Tree<'_, '_>, root: &Link, view: &mut Value, path: &[&str]) -> Result<()> {
    if let Some(v) = tree.at(root, path)? {
        assign(view, path, v);
    }
    Ok(())
}
/// Limits on the *full* namespace, not the partial planner view.
/// The prototype genesis audits queued destinations==0. Its only publishing
/// operation cannot add destinations; no generic full-CAS writer is exposed.
pub fn quota(tree: &mut Tree<'_, '_>, root: &Link, config: &EngineConfig) -> Result<()> {
    for (field, limit) in [
        ("runs", config.max_runs),
        ("events", config.max_events),
        ("receipts", config.max_receipts),
    ] {
        let count = tree
            .path(root, &[field])?
            .ok_or("missing quota field")?
            .count;
        if count > limit as u64 {
            return Err("full namespace count quota".into());
        }
    }
    if root.bytes > config.max_state_bytes as u64 {
        return Err("full namespace exact byte quota".into());
    }
    Ok(())
}
pub struct Plan {
    pub before: Value,
    pub after: Value,
    pub disposition: EventDisposition,
}
pub fn plan(
    tree: &mut Tree<'_, '_>,
    root: &Link,
    producer: &str,
    event: EventEnvelope,
    epoch: u64,
    now: i64,
) -> Result<Plan> {
    let key = digest(&(producer, epoch, &event.event_id))?;
    let trigger = indexes::trigger_key(&event.contract, &event.scope)?;
    let mut correlations = vec![indexes::correlation_key(
        &event.contract,
        &event.scope,
        &event.correlation,
    )?];
    for (field, value) in &event.correlation {
        correlations.push(indexes::field_key(
            &event.contract,
            &event.scope,
            field,
            value,
        )?);
    }
    if tree.batched {
        let mut paths = vec![
            vec!["artifacts"],
            vec!["catalog_revision"],
            vec!["sequence"],
            vec!["runs"],
            vec!["events"],
            vec!["receipts"],
            vec!["indexes", "initialized"],
            vec!["indexes", "dirty_runs"],
            vec!["indexes", "dirty_intents"],
            vec!["retention", "policy"],
            vec!["retention", "producer_epochs", producer],
            vec!["retention", "event_tombstones", &key],
            vec!["retention", "event_expires_at_ms", &key],
            vec!["retention", "event_epochs", &key],
            vec!["events", &key],
            vec!["indexes", "triggers", &trigger],
            vec!["indexes", "event_memberships", &key],
        ];
        for correlation in &correlations {
            paths.push(vec!["indexes", "waiters", correlation]);
            paths.push(vec!["indexes", "events", correlation]);
        }
        tree.prefetch_paths(root, &paths)?;
        let artifacts = tree.path(root, &["artifacts"])?.ok_or("missing catalog")?;
        tree.prefetch_subtree(&artifacts)?;
    }
    let mut view = serde_json::to_value(State::default())?;
    fields(&view, STATE_FIELDS)?;
    fields(&view["retention"], RETENTION_FIELDS)?;
    fields(&view["indexes"], INDEX_FIELDS)?;
    // Whole catalog needed by find_event's ordered artifact search and protection
    // checks. This is O(A), explicitly not a history-constant catalog lookup.
    for field in ["artifacts", "catalog_revision", "sequence"] {
        copy(tree, root, &mut view, &[field])?;
    }
    for field in ["initialized", "dirty_runs", "dirty_intents"] {
        copy(tree, root, &mut view, &["indexes", field])?;
    }
    if view["indexes"]["initialized"] != json!(true)
        || view["indexes"]["dirty_runs"] != json!([])
        || view["indexes"]["dirty_intents"] != json!([])
    {
        return Err("fallback: initialization or dirty-index synchronization".into());
    }
    for path in [
        vec!["retention", "policy"],
        vec!["retention", "producer_epochs", producer],
        vec!["retention", "event_tombstones", &key],
        vec!["retention", "event_expires_at_ms", &key],
        vec!["retention", "event_epochs", &key],
        vec!["events", &key],
    ] {
        copy(tree, root, &mut view, &path)?;
    }
    if tree
        .at(root, &["indexes", "triggers", &trigger])?
        .is_some_and(|v| v != json!([]))
    {
        return Err("fallback: relevant consumer".into());
    }
    for correlation in correlations {
        // Absence is traversed from the authoritative root, never SQL mirror.
        if tree
            .at(root, &["indexes", "waiters", &correlation])?
            .is_some_and(|v| v != json!([]))
        {
            return Err("fallback: relevant waiter".into());
        }
        // Existing groups need only an existence marker: their old members are
        // preserved by leaf patches. index_event inserts only the fresh sequence.
        if tree
            .path(root, &["indexes", "events", &correlation])?
            .is_some()
        {
            assign(&mut view, &["indexes", "events", &correlation], json!({}));
        }
    }
    copy(
        tree,
        root,
        &mut view,
        &["indexes", "event_memberships", &key],
    )?;
    // Complete read-set established. Other source collections are deliberately
    // omitted, not asserted empty: see PB04 prototype read-set document.
    let state: State = serde_json::from_value(view.clone())?;
    let store = MemoryStorage::default();
    store.acquire("prototype-view", "owner")?;
    store.compare_and_swap("prototype-view", "owner", 0, &state)?;
    let engine = Engine {
        store,
        namespace: "prototype-view".into(),
        owner: "owner".into(),
        config: EngineConfig::default(),
        metrics: metrics::Metrics::default(),
    };
    let disposition = engine.admit_event_in_epoch(producer, event, epoch, now)?;
    Ok(Plan {
        before: view,
        after: serde_json::to_value(engine.snapshot()?)?,
        disposition,
    })
}
/// Only insertion/replacement leaves. No deletes, receipt-offset maintenance,
/// array edits, or unsupported transitions can enter this isolated prototype.
/// Old expected values are checked against freshly authenticated rooted values.
pub fn patch(tree: &mut Tree<'_, '_>, root: &Link, before: &Value, after: &Value) -> Result<Link> {
    fn walk(
        tree: &mut Tree<'_, '_>,
        root: &Link,
        path: &mut Vec<String>,
        before: &Value,
        after: &Value,
    ) -> Result<Link> {
        if before == after {
            return Ok(root.clone());
        }
        if let (Some(a), Some(b)) = (before.as_object(), after.as_object()) {
            if a.keys().any(|k| !b.contains_key(k)) {
                return Err("fallback: deletion".into());
            }
            let mut root = root.clone();
            for (k, v) in b {
                path.push(k.clone());
                root = walk(tree, &root, path, a.get(k).unwrap_or(&Value::Null), v)?;
                path.pop();
            }
            return Ok(root);
        }
        // A newly introduced object must be merged leafwise because the partial
        // planner may know only an authenticated group's existence, not history.
        if before.is_null()
            && let Some(object) = after.as_object()
        {
            let refs: Vec<&str> = path.iter().map(String::as_str).collect();
            if tree.path(root, &refs)?.is_none() {
                return tree.set(root, &refs, after);
            }
            let mut root = root.clone();
            for (k, v) in object {
                path.push(k.clone());
                root = walk(tree, &root, path, &Value::Null, v)?;
                path.pop();
            }
            return Ok(root);
        }
        let path: Vec<&str> = path.iter().map(String::as_str).collect();
        let actual = tree.at(root, &path)?;
        if actual.as_ref().is_some_and(|v| v != before) {
            return Err("patch expected-value mismatch".into());
        }
        if before.is_array() || after.is_array() {
            // New membership array is safe; changing retained arrays is not.
            if actual.is_some() {
                return Err("fallback: existing array edit".into());
            }
        }
        tree.set(root, &path, after)
    }
    walk(tree, root, &mut vec![], before, after)
}
