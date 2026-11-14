//! Work-view projections for AQ's owner Graph reads (`human-graph-1`, G11
//! X8-X13): task-state counts, edge projection, readiness with its blockers
//! and hold reasons, and a change summary for each task history entry.
//!
//! Everything here is a pure function over Graph or AQ authority JSON. The
//! aq-server handlers own authorization, Graph I/O and AQ's own SQL.
use serde_json::{Map, Value, json};
use uuid::Uuid;

/// Task-state buckets for omitted and summary counts; an unset state is `unknown`.
pub const STATES: [&str; 5] = ["draft", "active", "completed", "canceled", "unknown"];
/// Longest history change summary, in characters.
pub const MAX_SUMMARY: usize = 512;

/// The bucket for a task state.
pub fn bucket(state: &Value) -> &'static str {
    state
        .as_str()
        .and_then(|s| STATES[..4].iter().copied().find(|b| *b == s))
        .unwrap_or("unknown")
}
/// Zero counts for every bucket.
pub fn buckets() -> Map<String, Value> {
    STATES.iter().map(|k| ((*k).to_owned(), json!(0))).collect()
}
/// Count one task in its state's bucket.
pub fn count(map: &mut Map<String, Value>, state: &Value) {
    let slot = map.entry(bucket(state)).or_insert(json!(0));
    *slot = json!(slot.as_u64().unwrap_or(0) + 1);
}
/// Counts as decimal strings, the wire form.
pub fn decimal(map: Map<String, Value>) -> Map<String, Value> {
    map.into_iter()
        .map(|(k, v)| (k, json!(v.as_u64().unwrap_or(0).to_string())))
        .collect()
}

/// Live task descendants of `root` in a bounded Graph tree, counted once each
/// by state (decimal strings), with their total and whether no level was
/// truncated. None when a level is malformed or crosses `workspace`.
pub fn descendants(
    tree: &Value,
    workspace: Uuid,
    root: Uuid,
) -> Option<(Map<String, Value>, u64, bool)> {
    let mut counts = buckets();
    let mut complete = true;
    let mut seen = std::collections::BTreeSet::from([root.to_string()]);
    let mut stack = vec![tree];
    while let Some(t) = stack.pop() {
        complete &= t["truncated"] == false;
        for child in t["children"].as_array()? {
            let n = &child["node"];
            if n["workspace_id"] != json!(workspace) {
                return None;
            }
            if seen.insert(n["id"].as_str()?.to_owned())
                && n["kind"] == "task"
                && n["deleted"] == false
            {
                count(&mut counts, &n["task_state"]);
            }
            stack.push(child);
        }
    }
    let total = counts.values().filter_map(Value::as_u64).sum();
    Some((decimal(counts), total, complete))
}

/// The public projection of a Graph edge: identity, endpoints, type and
/// description, with positive revisions as decimal strings. None when a
/// revision is malformed.
pub fn edge(edge: &Value) -> Option<Value> {
    let mut e = Map::new();
    for k in ["id", "source", "target", "edge_type", "description"] {
        e.insert(k.into(), edge[k].clone());
    }
    for k in ["version", "source_revision", "target_revision"] {
        let v = if edge[k].is_null() && k != "version" {
            Value::Null
        } else {
            json!(edge[k].as_u64().filter(|n| *n > 0)?.to_string())
        };
        e.insert(k.into(), v);
    }
    Some(Value::Object(e))
}

/// Live `blocks` edges that target `task` in a depth-1 Graph Context, at most
/// 64. `satisfied` is true only for a completed source and null when the
/// source is not in the Context. None when the Context lacks node or edge lists.
pub fn blockers(context: &Value, task: Uuid) -> Option<Vec<Value>> {
    let nodes = context["nodes"].as_array()?;
    let mut out = Vec::new();
    for e in context["edges"].as_array()? {
        if e["edge_type"] != "blocks" || e["deleted"] != false || e["target"] != json!(task) {
            continue;
        }
        let source = nodes
            .iter()
            .find(|n| n["id"] == e["source"] && n["deleted"] == false);
        let (title, state) = source.map_or((Value::Null, Value::Null), |n| {
            (n["title"].clone(), n["task_state"].clone())
        });
        let satisfied = state
            .as_str()
            .map_or(Value::Null, |s| json!(s == "completed"));
        out.push(json!({"edge_id":e["id"],"id":e["source"],"title":title,"task_state":state,"satisfied":satisfied}));
        if out.len() >= 64 {
            break;
        }
    }
    Some(out)
}

/// The attempt holding a task, from AQ's execution authority state (the hot
/// authority for adopted native attempts, else the attempt's own state).
pub struct Hold {
    pub attempt_id: Uuid,
    /// `offered`, `preparing`, `granted`, `cancelling`, `finished`, `declined`,
    /// `unknown` or `never_granted`.
    pub status: String,
    /// A B3 cancellation is recorded for the attempt.
    pub cancelled: bool,
    /// The stop proof is a Stopped or NotStarted receipt.
    pub stopped: bool,
}
impl Hold {
    pub fn from_authority(attempt_id: Uuid, state: &Value) -> Self {
        Self {
            attempt_id,
            status: state["status"].as_str().unwrap_or("unknown").to_owned(),
            cancelled: !state["cancellation_id"].is_null(),
            stopped: matches!(
                state["stop"]["kind"].as_str(),
                Some("stopped" | "not_started")
            ),
        }
    }
    /// Still executing, or not provably stopped, while it holds the task.
    pub fn running(&self) -> bool {
        !self.stopped
            && !matches!(
                self.status.as_str(),
                "finished" | "declined" | "never_granted"
            )
    }
    /// Why a task whose attempt no longer runs is still held; None while running.
    pub fn reason(&self) -> Option<Value> {
        if self.running() {
            return None;
        }
        let (code, message) = if !self.stopped {
            (
                "attempt_finished_stop_unconfirmed",
                "The attempt has finished but its stop is not proven yet, so it still holds the task.",
            )
        } else if self.cancelled {
            (
                "cancelled_attempt_holds_task",
                "The attempt was cancelled and has stopped, but it still holds the task: a cancelled attempt keeps its retained source for a replacement (B4). The claim is released when the task is retried or the owner abandons the attempt.",
            )
        } else {
            (
                "attempt_stopped_not_released",
                "The attempt has finished and stopped, but it still holds the task until its result is settled, the task is retried or the owner abandons the attempt.",
            )
        };
        Some(
            json!({"code":code,"attempt_id":self.attempt_id,"status":self.status,"message":message}),
        )
    }
}

/// A server-side hold of the task itself (R6): AQ does not offer it, for `code` (a server reason
/// code, `phrase` its fixed description), about `attempt_id` (with its authority `status`).
pub fn task_hold(attempt_id: Uuid, status: &str, code: &str, phrase: &str) -> Value {
    let message = format!("AQ does not offer this task: {phrase} (hold reason: {code}).");
    json!({"code":"task_held","attempt_id":attempt_id,"status":status,"message":message,"hold_reason":code})
}

/// Work-view readiness. The state is the first match of: the task's own
/// `draft`, `completed` or `canceled` state; `running` while the holding
/// attempt executes; `held` when it has stopped but keeps the task claim, or
/// an active task has a server-side hold ([`task_hold`]); `waiting` for an
/// unsatisfied blocker; `unknown` for a truncated Context; otherwise `ready`.
/// `reasons` explains a stopped attempt's hold in any state, then the task's hold.
pub fn readiness(
    task_state: Option<&str>,
    hold: Option<&Hold>,
    held: Option<Value>,
    blockers: Vec<Value>,
    complete: bool,
) -> Value {
    let open = blockers.iter().any(|b| b["satisfied"] != true);
    let held = held.filter(|_| task_state == Some("active"));
    let state = match task_state {
        Some(s @ ("completed" | "canceled" | "draft")) => s,
        _ if hold.is_some_and(Hold::running) => "running",
        _ if hold.is_some() || held.is_some() => "held",
        _ if open => "waiting",
        _ if !complete => "unknown",
        _ => "ready",
    };
    let reasons: Vec<Value> = hold
        .and_then(Hold::reason)
        .into_iter()
        .chain(held)
        .collect();
    json!({"state":state,"blockers":blockers,"complete":complete,"reasons":reasons})
}

/// Revision fields a history diff names, in display order. `priority` is the
/// `aq:priority` metadata key; `metadata` stands for any other metadata key.
const FIELDS: [&str; 16] = [
    "title",
    "body",
    "acceptance_criteria",
    "labels",
    "priority",
    "metadata",
    "task_state",
    "provenance",
    "content_type",
    "retired",
    "retirement_reason",
    "successor_id",
    "valid_from",
    "valid_until",
    "review_at",
    "deleted",
];

/// The task's node snapshot that a revision entry carries, with its revision.
fn snapshot(entry: &Value, task: Uuid) -> Option<(&Value, u64)> {
    let d = &entry["data"];
    if entry["category"] != "revision" || d["id"] != json!(task) {
        return None;
    }
    Some((d, d["content_revision"].as_u64().filter(|n| *n > 0)?))
}

/// The content revision the oldest revision entry on a history page needs as
/// its predecessor, when the page does not hold that snapshot.
pub fn predecessor(items: &[Value], task: Uuid) -> Option<u64> {
    let held: Vec<u64> = items
        .iter()
        .filter_map(|e| snapshot(e, task).map(|s| s.1))
        .collect();
    items
        .iter()
        .filter(|e| e["operation"] != "created")
        .filter_map(|e| snapshot(e, task).map(|s| s.1))
        .filter(|r| *r > 1 && !held.contains(&(r - 1)))
        .min()
        .map(|r| r - 1)
}

/// One change summary per history entry, in order, or null when an entry
/// carries nothing AQ can describe. `prior` is the node snapshot at the
/// revision [`predecessor`] named, when it could be read.
///
/// A change is `{summary, fields, edge, complete}`. `fields` lists changed
/// fields as `{field, from, to}`; values are given only for task state,
/// priority, content type, retired and deleted, and are null otherwise.
/// `edge` is `{edge_type, direction, node_id}` for a dependency or link
/// change. `complete` is false when the previous revision was unavailable.
pub fn changes(items: &[Value], task: Uuid, prior: Option<&Value>) -> Vec<Value> {
    items
        .iter()
        .map(|e| {
            let op = e["operation"].as_str().unwrap_or("");
            match e["category"].as_str() {
                Some("lifecycle") => lifecycle(op, &e["data"]),
                Some("edge") => linked(op, &e["data"], task),
                Some("revision") => revision(items, e, op, task, prior),
                _ => None,
            }
            .unwrap_or(Value::Null)
        })
        .collect()
}

fn summary(text: String) -> String {
    text.chars().take(MAX_SUMMARY).collect()
}
fn field(name: &str, from: Value, to: Value) -> Value {
    json!({"field":name,"from":from,"to":to})
}
fn change(text: String, fields: Vec<Value>, edge: Value, complete: bool) -> Option<Value> {
    Some(json!({"summary":summary(text),"fields":fields,"edge":edge,"complete":complete}))
}
/// A task state (or null) as shown in a change; None for anything else.
fn state(v: &Value) -> Option<Value> {
    match v {
        Value::Null => Some(Value::Null),
        _ => STATES[..4].iter().find(|s| v == **s).map(|s| json!(s)),
    }
}
fn shown(v: &Value) -> String {
    v.as_str().unwrap_or("none").to_owned()
}

fn lifecycle(op: &str, d: &Value) -> Option<Value> {
    match op {
        "created" | "transitioned" => {
            let (from, to) = (state(&d["from"])?, state(&d["to"])?);
            let text = if op == "created" {
                format!("created as {}", shown(&to))
            } else {
                format!("{} → {}", shown(&from), shown(&to))
            };
            change(text, vec![field("task_state", from, to)], Value::Null, true)
        }
        "deleted" if d["deleted"] == true => change(
            "deleted".into(),
            vec![field("deleted", json!("false"), json!("true"))],
            Value::Null,
            true,
        ),
        "restored" => {
            let from = d["from_revision"].as_u64().filter(|n| *n > 0)?;
            let to = state(&d["task_state"])?;
            change(
                format!("restored revision {from}"),
                vec![field("task_state", Value::Null, to)],
                Value::Null,
                true,
            )
        }
        _ => None,
    }
}

fn linked(op: &str, d: &Value, task: Uuid) -> Option<Value> {
    let verb = matches!(op, "added" | "removed").then_some(op)?;
    let kind = d["edge_type"].as_str().filter(|t| {
        !t.is_empty() && t.len() <= 64 && t.bytes().all(|b| b.is_ascii_lowercase() || b == b'-')
    })?;
    let id = |k: &str| d[k].as_str()?.parse::<Uuid>().ok();
    let (source, target) = (id("source")?, id("target")?);
    let (direction, other, word) = if target == task {
        ("incoming", source, "from")
    } else if source == task {
        ("outgoing", target, "to")
    } else {
        return None;
    };
    let short = &other.to_string()[..8];
    change(
        format!("{verb} {kind} {word} {short}"),
        vec![],
        json!({"edge_type":kind,"direction":direction,"node_id":other}),
        true,
    )
}

fn revision(
    items: &[Value],
    e: &Value,
    op: &str,
    task: Uuid,
    prior: Option<&Value>,
) -> Option<Value> {
    let (after, r) = snapshot(e, task)?;
    if op == "created" {
        return change("created".into(), vec![], Value::Null, true);
    }
    let previous = items
        .iter()
        .filter_map(|i| snapshot(i, task))
        .find(|(_, n)| *n + 1 == r)
        .map(|(s, _)| s)
        .or(prior
            .filter(|p| p["id"] == json!(task) && p["content_revision"].as_u64() == Some(r - 1)));
    let label = op.replace('_', " ");
    let Some(before) = previous else {
        return change(
            format!("{label}; earlier revision unavailable"),
            vec![],
            Value::Null,
            false,
        );
    };
    let fields = diff(before, after);
    let text = if fields.is_empty() {
        label
    } else {
        fields
            .iter()
            .map(|f| {
                let name = f["field"].as_str().unwrap_or("").replace('_', " ");
                match (&f["from"], &f["to"]) {
                    (Value::Null, Value::Null) => name,
                    (a, b) => format!("{name} {} → {}", shown(a), shown(b)),
                }
            })
            .collect::<Vec<_>>()
            .join("; ")
    };
    change(text, fields, Value::Null, true)
}

/// Changed fields between two node snapshots of the same task.
fn diff(before: &Value, after: &Value) -> Vec<Value> {
    let priority = |n: &Value| n["metadata"]["aq:priority"].clone();
    let other = |n: &Value| {
        let mut m = n["metadata"].as_object().cloned().unwrap_or_default();
        m.remove("aq:priority");
        m
    };
    let text = |v: &Value| match v {
        Value::Bool(b) => json!(b.to_string()),
        Value::Number(n) if n.is_i64() => json!(n.to_string()),
        Value::String(s)
            if !s.is_empty()
                && s.len() <= 64
                && s.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') =>
        {
            json!(s)
        }
        _ => Value::Null,
    };
    let mut out = Vec::new();
    for name in FIELDS {
        let (a, b) = match name {
            "priority" => (priority(before), priority(after)),
            "metadata" => (Value::Object(other(before)), Value::Object(other(after))),
            _ => (before[name].clone(), after[name].clone()),
        };
        if a == b {
            continue;
        }
        if matches!(
            name,
            "task_state" | "priority" | "content_type" | "retired" | "deleted"
        ) {
            out.push(field(name, text(&a), text(&b)));
        } else {
            out.push(field(name, Value::Null, Value::Null));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(task: Uuid, revision: u64, priority: i64, title: &str) -> Value {
        json!({"id":task,"content_revision":revision,"title":title,"body":"b","labels":[],
            "metadata":{"aq:priority":priority,"aq:project_id":"p"},"task_state":"active",
            "verification":"unverified","retired":false,"deleted":false})
    }
    fn entry(category: &str, operation: &str, data: Value) -> Value {
        json!({"category":category,"operation":operation,"data":data})
    }
    fn authority(status: &str, stop: &str, cancellation: Value) -> Value {
        json!({"status":status,"stop":{"kind":stop},"cancellation_id":cancellation})
    }

    #[test]
    fn a_stopped_holding_attempt_is_held_not_running() {
        let a = Uuid::new_v4();
        let live = Hold::from_authority(a, &authority("granted", "running", Value::Null));
        let r = readiness(Some("active"), Some(&live), None, vec![], true);
        assert_eq!(r["state"], "running");
        assert_eq!(r["reasons"], json!([]));

        // QA P1-01: the MS1 cancel drill's attempt finished with stop proof.
        let cancelled =
            Hold::from_authority(a, &authority("finished", "stopped", json!(Uuid::new_v4())));
        let r = readiness(Some("active"), Some(&cancelled), None, vec![], true);
        assert_eq!(r["state"], "held");
        assert_eq!(r["reasons"][0]["code"], "cancelled_attempt_holds_task");
        assert_eq!(r["reasons"][0]["attempt_id"], json!(a));

        let finished = Hold::from_authority(a, &authority("finished", "stopped", Value::Null));
        let r = readiness(Some("active"), Some(&finished), None, vec![], true);
        assert_eq!(r["reasons"][0]["code"], "attempt_stopped_not_released");

        let unproven = Hold::from_authority(a, &authority("finished", "unknown", Value::Null));
        let r = readiness(Some("active"), Some(&unproven), None, vec![], true);
        assert_eq!(r["state"], "held");
        assert_eq!(r["reasons"][0]["code"], "attempt_finished_stop_unconfirmed");

        // A cancelling attempt that has not stopped still runs.
        let cancelling = Hold::from_authority(
            a,
            &authority("cancelling", "running", json!(Uuid::new_v4())),
        );
        assert_eq!(
            readiness(None, Some(&cancelling), None, vec![], true)["state"],
            "running"
        );
        // The task's own terminal state wins; the hold is still explained.
        let r = readiness(Some("canceled"), Some(&cancelled), None, vec![], true);
        assert_eq!(r["state"], "canceled");
        assert_eq!(r["reasons"][0]["code"], "cancelled_attempt_holds_task");
    }

    #[test]
    fn unheld_readiness_keeps_its_order() {
        let open = vec![json!({"satisfied":false})];
        assert_eq!(
            readiness(Some("active"), None, None, open, true)["state"],
            "waiting"
        );
        let done = vec![json!({"satisfied":true})];
        assert_eq!(
            readiness(Some("active"), None, None, done.clone(), false)["state"],
            "unknown"
        );
        assert_eq!(
            readiness(Some("active"), None, None, done, true)["state"],
            "ready"
        );
        assert_eq!(
            readiness(Some("draft"), None, None, vec![], true)["state"],
            "draft"
        );
    }

    #[test]
    fn a_task_hold_makes_an_active_task_held_with_its_reason() {
        let a = Uuid::new_v4();
        let held = || {
            Some(task_hold(
                a,
                "finished",
                "lineage_retry_budget_exhausted",
                "its retry budget is exhausted",
            ))
        };
        let r = readiness(Some("active"), None, held(), vec![], true);
        assert_eq!(r["state"], "held");
        assert_eq!(r["reasons"][0]["code"], "task_held");
        assert_eq!(
            r["reasons"][0]["hold_reason"],
            "lineage_retry_budget_exhausted"
        );
        assert_eq!(r["reasons"][0]["attempt_id"], json!(a));
        assert!(
            r["reasons"][0]["message"]
                .as_str()
                .unwrap()
                .contains("its retry budget is exhausted")
        );
        // A retired or completed task is not held, and no hold leaves it ready.
        let r = readiness(Some("canceled"), None, held(), vec![], true);
        assert_eq!(
            (r["state"].as_str(), r["reasons"].as_array().unwrap().len()),
            (Some("canceled"), 0)
        );
        assert_eq!(
            readiness(Some("active"), None, None, vec![], true)["state"],
            "ready"
        );
        // A stopped attempt's hold is listed first.
        let stopped = Hold::from_authority(a, &authority("finished", "stopped", Value::Null));
        let r = readiness(Some("active"), Some(&stopped), held(), vec![], true);
        assert_eq!(r["reasons"][0]["code"], "attempt_stopped_not_released");
        assert_eq!(r["reasons"][1]["code"], "task_held");
    }

    #[test]
    fn blockers_read_live_incoming_blocks_edges() {
        let (t, b, gone) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let ctx = json!({"nodes":[{"id":b,"deleted":false,"title":"First","task_state":"completed"}],
            "edges":[{"id":Uuid::new_v4(),"edge_type":"blocks","deleted":false,"source":b,"target":t},
                {"id":Uuid::new_v4(),"edge_type":"blocks","deleted":false,"source":gone,"target":t},
                {"id":Uuid::new_v4(),"edge_type":"blocks","deleted":true,"source":b,"target":t},
                {"id":Uuid::new_v4(),"edge_type":"references","deleted":false,"source":b,"target":t}]});
        let found = blockers(&ctx, t).unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(found[0]["satisfied"], true);
        assert_eq!(found[1]["satisfied"], Value::Null);
        assert!(blockers(&json!({}), t).is_none());
    }

    #[test]
    fn lifecycle_entries_say_from_and_to() {
        let t = Uuid::new_v4();
        let items = [
            entry(
                "lifecycle",
                "created",
                json!({"from":null,"to":"active","version":1}),
            ),
            entry(
                "lifecycle",
                "transitioned",
                json!({"from":"active","to":"completed","version":2}),
            ),
            entry(
                "lifecycle",
                "transitioned",
                json!({"from":"active","to":"<b>x</b>"}),
            ),
            entry("lifecycle", "deleted", json!({"deleted":true,"version":3})),
        ];
        let c = changes(&items, t, None);
        assert_eq!(c[0]["summary"], "created as active");
        assert_eq!(c[1]["summary"], "active → completed");
        assert_eq!(
            c[1]["fields"],
            json!([{"field":"task_state","from":"active","to":"completed"}])
        );
        assert_eq!(c[2], Value::Null, "unknown states are never echoed");
        assert_eq!(c[3]["summary"], "deleted");
    }

    #[test]
    fn edge_entries_name_the_other_task_and_direction() {
        let (t, b) = (Uuid::new_v4(), Uuid::new_v4());
        let e = |op: &str, s: Uuid, d: Uuid| {
            entry(
                "edge",
                op,
                json!({"id":Uuid::new_v4(),"edge_type":"blocks","source":s,"target":d}),
            )
        };
        let c = changes(&[e("added", b, t), e("removed", t, b)], t, None);
        assert_eq!(
            c[0]["edge"],
            json!({"edge_type":"blocks","direction":"incoming","node_id":b})
        );
        assert_eq!(
            c[0]["summary"],
            format!("added blocks from {}", &b.to_string()[..8])
        );
        assert_eq!(c[1]["edge"]["direction"], "outgoing");
        assert!(
            c[1]["summary"]
                .as_str()
                .unwrap()
                .starts_with("removed blocks to ")
        );
    }

    #[test]
    fn revision_entries_diff_against_the_previous_revision() {
        let t = Uuid::new_v4();
        let mut edited = node(t, 3, 5, "Renamed");
        edited["metadata"]["aq:project_id"] = json!("q");
        let mut linked = edited.clone();
        linked["content_revision"] = json!(4);
        let items = [
            entry("revision", "updated", node(t, 2, 0, "Original")),
            entry("revision", "updated", edited),
            entry("revision", "links_changed", linked),
        ];
        assert_eq!(predecessor(&items, t), Some(1));
        let c = changes(&items, t, None);
        assert_eq!(c[0]["complete"], false, "revision 1 was not read");
        assert_eq!(c[0]["summary"], "updated; earlier revision unavailable");
        assert_eq!(
            c[1]["fields"],
            json!([{"field":"title","from":null,"to":null},
                {"field":"priority","from":"0","to":"5"},
                {"field":"metadata","from":null,"to":null}])
        );
        assert_eq!(c[1]["summary"], "title; priority 0 → 5; metadata");
        assert_eq!(c[2]["summary"], "links changed");
        assert_eq!(c[2]["fields"], json!([]));

        let prior = node(t, 1, 0, "Original");
        let c = changes(&items, t, Some(&prior));
        assert_eq!(c[0]["complete"], true);
        assert_eq!(c[0]["fields"], json!([]));
        let created = [entry("revision", "created", node(t, 1, 0, "New"))];
        assert_eq!(predecessor(&created, t), None);
        assert_eq!(changes(&created, t, None)[0]["summary"], "created");
    }

    #[test]
    fn unrecognized_payloads_never_escape() {
        let t = Uuid::new_v4();
        let items = [entry(
            "lifecycle",
            "set_task_state",
            json!({"secret":"must-not-escape"}),
        )];
        assert_eq!(changes(&items, t, None), vec![Value::Null]);
        let mut a = node(t, 1, 0, "A");
        let mut b = node(t, 2, 0, "B");
        a["content_type"] = json!("note");
        b["content_type"] = json!("Secret Text");
        let c = changes(&[entry("revision", "updated", b)], t, Some(&a));
        let fields = c[0]["fields"].as_array().unwrap();
        assert!(fields.contains(&json!({"field":"content_type","from":"note","to":null})));
        assert!(!c[0].to_string().contains("Secret"));
    }

    #[test]
    fn descendants_count_each_live_task_once_and_mark_truncation() {
        let (w, root, a, b) = (
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        let n = |id: Uuid, s: &str| json!({"id":id,"workspace_id":w,"kind":"task","deleted":false,"task_state":s});
        let leaf = |id: Uuid, s: &str| json!({"node":n(id, s),"children":[],"truncated":false});
        let tree = json!({"node":n(root,"active"),"truncated":false,"children":[
            leaf(a, "completed"), {"node":n(b,"active"),"truncated":true,"children":[leaf(a, "completed")]}]});
        let (counts, total, complete) = descendants(&tree, w, root).unwrap();
        assert_eq!(
            (counts["completed"].clone(), total, complete),
            (json!("1"), 2, false)
        );
        assert!(descendants(&tree, Uuid::new_v4(), root).is_none());
    }

    #[test]
    fn counts_and_edges_use_decimal_wire_forms() {
        let mut m = buckets();
        count(&mut m, &json!("completed"));
        count(&mut m, &json!("weird"));
        let m = decimal(m);
        assert_eq!(m["completed"], "1");
        assert_eq!(m["unknown"], "1");
        let e = json!({"id":"e","source":"s","target":"t","edge_type":"blocks","description":null,
            "version":2,"source_revision":null,"target_revision":3});
        let p = edge(&e).unwrap();
        assert_eq!(p["version"], "2");
        assert_eq!(p["source_revision"], Value::Null);
        assert_eq!(p["target_revision"], "3");
        assert!(edge(&json!({"version":0})).is_none());
    }
}

mod execution;
pub use execution::{Execution, TaskActivate, TaskDelete, TaskReopen};

mod downstream;
pub use downstream::blocked_by;
