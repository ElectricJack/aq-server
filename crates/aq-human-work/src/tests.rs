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

fn t(id: Uuid, state: &str) -> Value {
    json!({"id":id,"kind":"task","title":format!("T {id}"),"task_state":state,"retired":false,"deleted":false})
}
fn pc(source: Uuid, target: Uuid) -> Value {
    json!({"id":Uuid::new_v4(),"edge_type":"parent-child","source":source,"target":target,"deleted":false})
}
fn ctx(nodes: Vec<Value>, edges: Vec<Value>, truncated: bool) -> Value {
    json!({"nodes":nodes,"edges":edges,"truncated":truncated})
}
fn codes(h: &Hierarchy) -> Vec<(String, u64)> {
    h.blocking
        .iter()
        .map(|b| {
            (
                b["code"].as_str().unwrap().into(),
                b["level"].as_u64().unwrap(),
            )
        })
        .collect()
}

#[test]
fn a_draft_or_retired_ancestor_within_three_levels_blocks() {
    let (task, parent, grand) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let edges = vec![pc(parent, task), pc(grand, parent)];
    let run = |p: Value, g: Value| {
        let c = ctx(vec![p, g], edges.clone(), false);
        hierarchy(&c, Some(&c), task, 2).unwrap()
    };
    let h = run(t(parent, "active"), t(grand, "active"));
    assert!(h.blocking.is_empty() && h.complete);
    let h = run(t(parent, "draft"), t(grand, "active"));
    assert_eq!(codes(&h), [("ancestor_draft".into(), 1)]);
    assert_eq!(h.blocking[0]["id"], json!(parent));
    assert_eq!(h.blocking[0]["edge_id"], edges[0]["id"]);
    let h = run(t(parent, "active"), t(grand, "draft"));
    assert_eq!(codes(&h), [("ancestor_draft".into(), 2)]);
    let mut retired = t(grand, "active");
    retired["retired"] = json!(true);
    let h = run(t(parent, "draft"), retired);
    assert_eq!(
        codes(&h),
        [("ancestor_draft".into(), 1), ("ancestor_retired".into(), 2)]
    );
    let h = run(t(parent, "completed"), t(grand, "canceled"));
    assert!(h.blocking.is_empty());
}

#[test]
fn live_children_make_a_container_but_deleted_or_retired_ones_do_not() {
    let (task, a, b, c) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    let mut gone = t(b, "active");
    gone["deleted"] = json!(true);
    let mut old = t(c, "active");
    old["retired"] = json!(true);
    let edges = vec![pc(task, a), pc(task, b), pc(task, c)];
    let ctx1 = ctx(vec![t(a, "completed"), gone, old], edges, false);
    let h = hierarchy(&ctx1, None, task, 2).unwrap();
    assert_eq!(codes(&h), [("has_live_children".into(), 1)]);
    assert_eq!(h.blocking[0]["relationship"], "child");
    assert_eq!(h.blocking[0]["task_state"], "completed");
    let leaf = ctx(vec![t(b, "active")], vec![], false);
    let h = hierarchy(&leaf, None, task, 2).unwrap();
    assert!(h.blocking.is_empty() && h.complete);
}

#[test]
fn missing_or_truncated_ancestry_is_incomplete_not_ready() {
    let (task, parent) = (Uuid::new_v4(), Uuid::new_v4());
    let edges = vec![pc(parent, task)];
    // A parent edge needs the ancestry read.
    let c = ctx(vec![t(parent, "active")], edges.clone(), false);
    assert!(needs_ancestry(&c, task));
    assert!(!hierarchy(&c, None, task, 2).unwrap().complete);
    assert!(hierarchy(&c, Some(&c), task, 2).unwrap().complete);
    // The parent node is not in either Context.
    let c = ctx(vec![], edges, false);
    assert!(!hierarchy(&c, Some(&c), task, 2).unwrap().complete);
    // A truncated Context.
    let c = ctx(vec![], vec![], true);
    assert!(needs_ancestry(&c, task));
    assert!(!hierarchy(&c, Some(&c), task, 2).unwrap().complete);
    let c = ctx(vec![], vec![], false);
    assert!(!needs_ancestry(&c, task));
    assert!(hierarchy(&c, None, task, 2).unwrap().complete);
    assert!(hierarchy(&json!({}), None, task, 2).is_none());
}

#[test]
fn hierarchy_makes_an_active_task_wait_and_only_then() {
    let h = vec![json!({"code":"ancestor_draft"})];
    let r = readiness(
        Some("active"),
        None,
        vec![],
        TaskDependencies {
            blockers: vec![],
            hierarchy: h.clone(),
            complete: true,
        },
    );
    assert_eq!(r["state"], "waiting");
    assert_eq!(r["hierarchy"], json!(h));
    // Truncated and nothing known: unknown. A relationship is waiting.
    let r = readiness(
        Some("active"),
        None,
        vec![],
        TaskDependencies {
            blockers: vec![],
            hierarchy: vec![],
            complete: false,
        },
    );
    assert_eq!(r["state"], "unknown");
    assert!(r.get("hierarchy").is_none());
    let r = readiness(
        Some("active"),
        None,
        vec![],
        TaskDependencies {
            blockers: vec![],
            hierarchy: h.clone(),
            complete: false,
        },
    );
    assert_eq!(r["state"], "waiting");
    // Terminal and draft tasks keep their own state and show no hierarchy.
    for s in ["draft", "completed", "canceled"] {
        let r = readiness(
            Some(s),
            None,
            vec![],
            TaskDependencies {
                blockers: vec![],
                hierarchy: h.clone(),
                complete: true,
            },
        );
        assert_eq!(r["state"], s);
        assert!(r.get("hierarchy").is_none());
    }
    // A running attempt outranks it.
    let live = Hold::from_authority(
        Uuid::new_v4(),
        &authority("granted", "running", Value::Null),
    );
    let r = readiness(
        Some("active"),
        Some(&live),
        vec![],
        TaskDependencies {
            blockers: vec![],
            hierarchy: h,
            complete: true,
        },
    );
    assert_eq!(r["state"], "running");
}

#[test]
fn a_stopped_holding_attempt_is_held_not_running() {
    let a = Uuid::new_v4();
    let live = Hold::from_authority(a, &authority("granted", "running", Value::Null));
    let r = readiness(
        Some("active"),
        Some(&live),
        None,
        TaskDependencies::new(vec![], true),
    );
    assert_eq!(r["state"], "running");
    assert_eq!(r["reasons"], json!([]));

    // QA P1-01: the MS1 cancel drill's attempt finished with stop proof.
    let cancelled =
        Hold::from_authority(a, &authority("finished", "stopped", json!(Uuid::new_v4())));
    let r = readiness(
        Some("active"),
        Some(&cancelled),
        None,
        TaskDependencies::new(vec![], true),
    );
    assert_eq!(r["state"], "held");
    assert_eq!(r["reasons"][0]["code"], "cancelled_attempt_holds_task");
    assert_eq!(r["reasons"][0]["attempt_id"], json!(a));

    let finished = Hold::from_authority(a, &authority("finished", "stopped", Value::Null));
    let r = readiness(
        Some("active"),
        Some(&finished),
        None,
        TaskDependencies::new(vec![], true),
    );
    assert_eq!(r["reasons"][0]["code"], "attempt_stopped_not_released");

    let unproven = Hold::from_authority(a, &authority("finished", "unknown", Value::Null));
    let r = readiness(
        Some("active"),
        Some(&unproven),
        None,
        TaskDependencies::new(vec![], true),
    );
    assert_eq!(r["state"], "held");
    assert_eq!(r["reasons"][0]["code"], "attempt_finished_stop_unconfirmed");

    // A cancelling attempt that has not stopped still runs.
    let cancelling = Hold::from_authority(
        a,
        &authority("cancelling", "running", json!(Uuid::new_v4())),
    );
    assert_eq!(
        readiness(
            None,
            Some(&cancelling),
            None,
            TaskDependencies::new(vec![], true)
        )["state"],
        "running"
    );
    // The task's own terminal state wins; the hold is still explained.
    let r = readiness(
        Some("canceled"),
        Some(&cancelled),
        None,
        TaskDependencies::new(vec![], true),
    );
    assert_eq!(r["state"], "canceled");
    assert_eq!(r["reasons"][0]["code"], "cancelled_attempt_holds_task");
}

#[test]
fn unheld_readiness_keeps_its_order() {
    let open = vec![json!({"satisfied":false})];
    assert_eq!(
        readiness(
            Some("active"),
            None,
            None,
            TaskDependencies::new(open, true)
        )["state"],
        "waiting"
    );
    let done = vec![json!({"satisfied":true})];
    assert_eq!(
        readiness(
            Some("active"),
            None,
            None,
            TaskDependencies::new(done.clone(), false)
        )["state"],
        "unknown"
    );
    assert_eq!(
        readiness(
            Some("active"),
            None,
            None,
            TaskDependencies::new(done, true)
        )["state"],
        "ready"
    );
    assert_eq!(
        readiness(
            Some("draft"),
            None,
            None,
            TaskDependencies::new(vec![], true)
        )["state"],
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
    let r = readiness(
        Some("active"),
        None,
        held(),
        TaskDependencies::new(vec![], true),
    );
    let reason = &r["reasons"][0];
    assert_eq!(
        [
            &r["state"],
            &reason["code"],
            &reason["hold_reason"],
            &reason["attempt_id"]
        ],
        [
            &json!("held"),
            &json!("task_held"),
            &json!("lineage_retry_budget_exhausted"),
            &json!(a)
        ]
    );
    assert!(
        r["reasons"][0]["message"]
            .as_str()
            .unwrap()
            .contains("its retry budget is exhausted")
    );
    // A retired or completed task is not held, and no hold leaves it ready.
    let r = readiness(
        Some("canceled"),
        None,
        held(),
        TaskDependencies::new(vec![], true),
    );
    assert_eq!(
        (r["state"].as_str(), r["reasons"].as_array().unwrap().len()),
        (Some("canceled"), 0)
    );
    assert_eq!(
        readiness(
            Some("active"),
            None,
            None,
            TaskDependencies::new(vec![], true)
        )["state"],
        "ready"
    );
    // A stopped attempt's hold is listed first.
    let stopped = Hold::from_authority(a, &authority("finished", "stopped", Value::Null));
    let r = readiness(
        Some("active"),
        Some(&stopped),
        held(),
        TaskDependencies::new(vec![], true),
    );
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
    let satisfied: Vec<_> = found.iter().map(|e| &e["satisfied"]).collect();
    assert_eq!(satisfied, [&json!(true), &Value::Null]);
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
    let summaries: Vec<_> = c.iter().map(|change| change["summary"].as_str()).collect();
    assert_eq!(
        summaries,
        [
            Some("created as active"),
            Some("active → completed"),
            None,
            Some("deleted")
        ]
    );
    assert_eq!(
        c[1]["fields"],
        json!([{"field":"task_state","from":"active","to":"completed"}])
    );
    assert_eq!(c[2], Value::Null, "unknown states are never echoed");
}

#[test]
fn parent_reads_the_live_parent_edge_and_defers_node_validation() {
    let (top, child) = (Uuid::from_u128(1), Uuid::from_u128(2));
    let edge =
        json!({"id":"e","edge_type":"parent-child","deleted":false,"source":top,"target":child});
    let nodes =
        vec![json!({"id":top,"kind":"task","title":"t","task_state":"draft","deleted":false})];
    let pass = |n: &Value, _: &str| Some(n.clone());
    let found = parent(&ctx(nodes.clone(), vec![edge.clone()], false), child, pass);
    assert_eq!(found.unwrap()["task_state"], "draft");
    assert_eq!(
        parent(
            &ctx(nodes.clone(), vec![edge.clone()], false),
            child,
            |_, _| None
        ),
        None
    );
    assert_eq!(
        parent(&ctx(nodes, vec![edge], false), top, pass),
        Some(Value::Null)
    );
    let gone =
        json!({"id":"e","edge_type":"parent-child","deleted":false,"source":top,"target":child});
    let unlisted = parent(&ctx(vec![], vec![gone], false), child, pass).unwrap();
    assert_eq!(
        (unlisted["id"].clone(), unlisted["kind"].clone()),
        (json!(top), Value::Null)
    );
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
    for (change, direction, action) in [
        (&c[0], "incoming", "added blocks from"),
        (&c[1], "outgoing", "removed blocks to"),
    ] {
        assert_eq!(
            change["edge"],
            json!({"edge_type":"blocks","direction":direction,"node_id":b})
        );
        assert_eq!(
            change["summary"],
            format!("{action} {}", &b.to_string()[..8])
        );
    }
}

#[test]
fn a_role_change_is_named_in_the_history_with_its_values() {
    let t = Uuid::new_v4();
    let mut named = node(t, 2, 0, "Task");
    named["metadata"]["aq:role"] = json!("reviewer");
    let mut cleared = node(t, 3, 0, "Task");
    cleared["content_revision"] = json!(3);
    let items = [
        entry("revision", "created", node(t, 1, 0, "Task")),
        entry("revision", "updated", named),
        entry("revision", "updated", cleared),
    ];
    let c = changes(&items, t, None);
    assert_eq!(
        c[1]["fields"],
        json!([{"field":"role","from":null,"to":"reviewer"}])
    );
    assert_eq!(c[1]["summary"], "role none → reviewer");
    assert_eq!(c[2]["summary"], "role reviewer → none");
}

#[test]
fn a_role_hold_makes_an_active_task_held_and_is_listed_after_its_other_hold() {
    let role = json!({"code":"role_disabled","role":"reviewer","message":"m"});
    let r = readiness(
        Some("active"),
        None,
        vec![role.clone()],
        TaskDependencies::new(vec![], true),
    );
    assert_eq!(
        (r["state"].as_str(), &r["reasons"]),
        (Some("held"), &json!([role]))
    );
    let a = Uuid::new_v4();
    let recorded = task_hold(a, "finished", "lineage_retry_budget_exhausted", "x");
    let both = vec![recorded.clone(), role.clone()];
    let r = readiness(
        Some("active"),
        None,
        both.clone(),
        TaskDependencies::new(vec![], true),
    );
    assert_eq!(r["reasons"], json!([recorded, role]));
    // Only an active task is held; a draft or closed task keeps its state.
    for state in ["draft", "completed", "canceled"] {
        let r = readiness(
            Some(state),
            None,
            both.clone(),
            TaskDependencies::new(vec![], true),
        );
        assert_eq!(
            (r["state"].as_str(), &r["reasons"]),
            (Some(state), &json!([]))
        );
    }
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
    assert_eq!(
        c[0],
        json!({"complete":false,"fields":[],"edge":null,"summary":"updated; earlier revision unavailable"}),
        "revision 1 was not read"
    );
    assert_eq!(
        c[1]["fields"],
        json!([{"field":"title","from":null,"to":null},
            {"field":"priority","from":"0","to":"5"},
            {"field":"metadata","from":null,"to":null}])
    );
    assert_eq!(c[1]["summary"], "title; priority 0 → 5; metadata");
    assert_eq!(
        c[2],
        json!({"complete":true,"fields":[],"edge":null,"summary":"links changed"})
    );

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
    assert_eq!(
        [&p["version"], &p["source_revision"], &p["target_revision"]],
        [&json!("2"), &Value::Null, &json!("3")]
    );
    assert!(edge(&json!({"version":0})).is_none());
}

#[test]
fn an_owner_retry_hold_takes_precedence_over_ready_replacement_evidence() {
    let proof = json!({"code":"replacement_facts_ready","wake_condition":"worker_poll"});
    let budget = json!({"code":"task_held","wake_condition":"owner_retry_authorized"});
    let blocked = readiness(
        Some("active"),
        None,
        vec![budget.clone(), proof.clone()],
        TaskDependencies::new(vec![], true),
    );
    assert_eq!(blocked["state"], "held");
    assert_eq!(blocked["reasons"], json!([budget]));
    let waiting = readiness(
        Some("active"),
        None,
        vec![proof.clone()],
        TaskDependencies::new(vec![], true),
    );
    assert_eq!(waiting["reasons"], json!([proof]));
}

#[test]
fn closed_launch_shows_the_owner_and_the_event_that_can_release_it() {
    let attempt = Uuid::new_v4();
    let hold = Hold::from_authority(attempt, &authority("cancelling", "unknown", Value::Null));
    let absent = crate::holds::recovery_reason(attempt, Some("unknown"), None, false);
    assert_eq!(
        readiness(
            Some("active"),
            Some(&hold),
            absent,
            TaskDependencies::new(vec![], true)
        )["state"],
        "running"
    );

    let reason =
        crate::holds::recovery_reason(attempt, Some("unknown"), Some(false), false).unwrap();
    let result = readiness(
        Some("active"),
        Some(&hold),
        Some(reason),
        TaskDependencies::new(vec![], true),
    );
    assert_eq!(result["state"], "held");
    assert_eq!(result["reasons"][0]["attempt_id"], json!(attempt));
    assert_eq!(result["reasons"][0]["blocker_owner"], "aq_operator");
    assert_eq!(
        result["reasons"][0]["wake_condition"],
        "original_launch_evidence_reconciled"
    );

    let stopped = crate::holds::recovery_reason(attempt, Some("stopped"), None, false).unwrap();
    assert_eq!(stopped["blocker_owner"], "aq_worker");
    assert!(crate::holds::recovery_reason(attempt, Some("stopped"), None, true).is_none());
}

#[cfg(feature = "delivery_playbook")]
#[test]
fn exhausted_delivery_names_the_retry_owner_and_original_result() {
    let attempt = Uuid::new_v4();
    let result = Uuid::new_v4();
    let stopped = Hold::from_authority(attempt, &authority("finished", "stopped", Value::Null));
    let reason = delivery_hold(&stopped, result, "returns_exhausted", None);
    assert_eq!(reason["attempt_id"], json!(attempt));
    assert_eq!(reason["result_id"], json!(result));
    assert_eq!(reason["blocker_owner"], "project_owner");
    assert_eq!(reason["wake_condition"], "owner_retry_authorized");
}
