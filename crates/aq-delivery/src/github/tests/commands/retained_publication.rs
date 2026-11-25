use super::*;
use crate::retained_publication::Identity;

fn identity() -> Identity {
    Identity {
        project_id: Uuid::from_u128(1),
        repository_id: target().git.repository_id,
        binding_generation: target().git.binding_generation,
        task_id: TASK,
        attempt_id: Uuid::from_u128(2),
        report_id: Uuid::from_u128(3),
        revision: "c".repeat(64),
        head: sha('a'),
    }
}
fn request(id: &Identity) -> Value {
    json!({"project":id.project_id,"task":id.task_id,"result_head":id.head,
        "publication_target":target(),"result_ready":{"project_id":id.project_id,"task_id":id.task_id,
        "attempt_id":id.attempt_id,"result_id":id.report_id}})
}
#[test]
fn exact_identity_and_repository_binding_cannot_be_substituted() {
    let id = identity();
    let original = request(&id);
    assert!(id.matches(&original, &TASK.to_string()));
    assert!(id.matches_target(&original, &target()));
    for field in ["project_id", "task_id", "attempt_id", "result_id"] {
        let mut changed = original.clone();
        changed["result_ready"][field] = json!(Uuid::new_v4());
        assert!(!id.matches(&changed, &TASK.to_string()), "{field}");
    }
    for field in ["project", "task", "result_head"] {
        let mut changed = original.clone();
        changed[field] = json!("other");
        assert!(!id.matches(&changed, &TASK.to_string()), "{field}");
    }
    assert!(!id.matches(&original, &Uuid::new_v4().to_string()));
    let mut changed = target();
    changed.git.repository_id = Uuid::new_v4();
    assert!(!id.matches_target(&original, &changed));
    changed = target();
    changed.git.binding_generation += 1;
    assert!(!id.matches_target(&original, &changed));
    changed = target();
    changed.github.name = "another".into();
    assert!(!id.matches_target(&original, &changed));
}
#[tokio::test]
async fn uncertain_success_reconciles_the_original_generation_without_any_push() {
    let original = Uuid::from_u128(9);
    let git = FakeGit::start(move |r| {
        assert_eq!(r["operation"], "operation.reconcile");
        assert_eq!(
            r["params"],
            json!({"target_operation_id":original,"expected_generation":7})
        );
        json!({"target_operation_id":original,"resolution":{"outcome":"already_present","observed_target":{"algorithm":"sha1","hex":sha('a')},
            "caused_by_this_operation":false,"observed_at":"2026-11-25T15:00:00Z"}})
    });
    let (delivery, mock) = delivery(&git).await;
    let journal = json!({"status":"pending","phase":"indeterminate","generation":7});
    let result = delivery
        .reconcile_publication(&target(), Uuid::new_v4(), original, &sha('a'), &journal)
        .await;
    assert_eq!(result["state"], "published");
    assert_eq!(result["caused_by_this_operation"], false);
    for journal in [
        json!({"status":"complete","phase":"complete","outcome":"rejected"}),
        json!({"state":"unavailable"}),
        json!({"status":"pending","phase":"queued","generation":7}),
    ] {
        assert_eq!(
            delivery
                .reconcile_publication(&target(), Uuid::new_v4(), original, &sha('a'), &journal)
                .await["state"],
            "held"
        );
    }
    assert_eq!(git.operations(), ["operation.reconcile"]);
    assert!(mock.seen().is_empty());
}
#[tokio::test]
async fn moved_remote_and_ambiguous_reconcile_remain_uncertain() {
    let original = Uuid::from_u128(9);
    let git = FakeGit::start(
        move |_| json!({"target_operation_id":original,"outcome":"indeterminate", "observed_target":{"algorithm":"sha1","hex":sha('b')}}),
    );
    let (delivery, _) = delivery(&git).await;
    let result = delivery
        .reconcile_publication(
            &target(),
            Uuid::new_v4(),
            original,
            &sha('a'),
            &json!({"status":"pending","phase":"dispatched","generation":2}),
        )
        .await;
    assert_eq!(
        result,
        json!({"state":"held","reason":"operation_indeterminate"})
    );
    assert_eq!(git.operations(), ["operation.reconcile"]);
}
#[tokio::test]
async fn publisher_inspection_preserves_the_original_request_and_bounds_rejection_evidence() {
    let original = Uuid::from_u128(9);
    let git = FakeGit::start(move |r| {
        if r["operation"] == "capabilities" {
            return json!({"capability_tokens":["publication_rejection_classification_v1"]});
        }
        let mut reply = json!({"version":1,"id":original,"generation":2,"status":"complete","phase":"complete",
        "payload":{"operation":"publish","repository_id":target().git.repository_id,"expected_binding_generation":3,
            "params":{"source_ref":task_branch(TASK),"target":task_branch(TASK),"expected_source":{"algorithm":"sha1","hex":sha('a')},
            "expected_old":{"algorithm":"sha1","hex":"0".repeat(40)}}},
        "result":{"outcome":"rejected","reason":"remote_rejected","stderr":"private publisher output"}});
        reply["payload_hash"] = json!(crate::digest(&reply["payload"]).unwrap());
        reply
    });
    let (delivery, mock) = delivery(&git).await;
    let evidence = delivery
        .inspect_publication(&target(), original, TASK, None, &sha('a'))
        .await;
    assert_eq!(evidence["operation_id"], original.to_string());
    assert_eq!(evidence["reason"], "remote_rejected");
    assert!(!evidence.to_string().contains("private publisher output"));
    let wrong = delivery
        .inspect_publication(&target(), original, TASK, None, &sha('b'))
        .await;
    assert_eq!(wrong["state"], "unavailable");
    assert_eq!(
        git.operations(),
        ["operation.show", "capabilities", "operation.show"]
    );
    assert_eq!(
        git.request(0)["params"]["target_operation_id"],
        original.to_string()
    );
    assert!(mock.seen().is_empty());
}

#[tokio::test]
async fn private_journal_cannot_disclose_a_cross_binding_or_changed_request() {
    let original = Uuid::from_u128(9);
    let corrupt = Arc::new(Mutex::new((String::new(), Value::Null)));
    let mode = corrupt.clone();
    let git = FakeGit::start(move |r| {
        if r["operation"] == "capabilities" {
            return json!({});
        }
        let mut payload = json!({"operation":"publish","repository_id":target().git.repository_id,"expected_binding_generation":3,
            "params":{"source_ref":task_branch(TASK),"target":task_branch(TASK),"expected_source":{"algorithm":"sha1","hex":sha('a')},
            "expected_old":{"algorithm":"sha1","hex":"0".repeat(40)}}});
        let (path, value) = mode.lock().unwrap().clone();
        if let Some(field) = path.strip_prefix("params.") {
            payload["params"][field] = value.clone();
        } else if path != "payload_hash" {
            payload[&path] = value.clone();
        }
        let hash = if path == "payload_hash" {
            value
        } else {
            json!(crate::digest(&payload).unwrap())
        };
        json!({"version":1,"id":original,"generation":1,"status":"pending","phase":"indeterminate","payload":payload,"payload_hash":hash})
    });
    let (delivery, _) = delivery(&git).await;
    for (path, value) in [
        ("repository_id", json!(Uuid::new_v4())),
        ("expected_binding_generation", json!(4)),
        ("operation", json!("ref.delete")),
        ("params.source_ref", json!("refs/heads/aq/other")),
        ("params.target", json!("refs/heads/main")),
        (
            "params.expected_source",
            json!({"algorithm":"sha1","hex":sha('b')}),
        ),
        (
            "params.expected_old",
            json!({"algorithm":"sha1","hex":sha('c')}),
        ),
        ("params.force", json!(true)),
        ("payload_hash", json!("f".repeat(64))),
    ] {
        *corrupt.lock().unwrap() = (path.into(), value);
        let evidence = delivery
            .inspect_publication(&target(), original, TASK, None, &sha('a'))
            .await;
        assert_eq!(evidence["state"], "unavailable", "{path}");
        assert!(evidence.get("payload").is_none());
    }
    assert!(git.operations().iter().all(|op| op == "operation.show"));
}

fn journal_reply(operation: Uuid, status: &str, outcome: Value) -> Value {
    let payload = json!({"operation":"publish","repository_id":target().git.repository_id,"expected_binding_generation":3,
        "params":{"source_ref":task_branch(TASK),"target":task_branch(TASK),"expected_source":{"algorithm":"sha1","hex":sha('a')},
        "expected_old":{"algorithm":"sha1","hex":"0".repeat(40)}}});
    json!({"version":1,"id":operation,"generation":2,"status":status,
        "phase":if status == "pending" {"indeterminate"} else {"complete"},
        "payload_hash":crate::digest(&payload).unwrap(),"payload":payload,"result":outcome})
}
#[tokio::test]
async fn historical_operation_ids_are_inspected_separately_and_unknown_phase_text_is_not_disclosed()
{
    let operations = [
        "dfd8cb53-9b5e-833c-bfe8-f32b36408c66",
        "de4542bf-b430-8e57-a85e-9eb6f957679d",
        "f7f30714-360d-84d3-9506-94aaf6d76f42",
        "72024a5d-7cd8-8af6-a442-9b4d0ac8c0a2",
    ]
    .map(|id| Uuid::parse_str(id).unwrap());
    let git = FakeGit::start(move |r| {
        if r["operation"] == "capabilities" {
            return json!({});
        }
        let id = Uuid::parse_str(r["params"]["target_operation_id"].as_str().unwrap()).unwrap();
        assert!(operations.contains(&id));
        let mut reply = journal_reply(id, "pending", Value::Null);
        reply["phase"] = json!("private_phase_text");
        reply
    });
    let (delivery, _) = delivery(&git).await;
    for original in operations {
        let evidence = delivery
            .inspect_publication(&target(), original, TASK, None, &sha('a'))
            .await;
        assert_eq!(evidence["operation_id"], original.to_string());
        assert_eq!(evidence["outcome"], "indeterminate");
        assert!(evidence["phase"].is_null());
        assert!(!evidence.to_string().contains("private_phase_text"));
        assert_eq!(
            delivery
                .reconcile_publication(&target(), Uuid::new_v4(), original, &sha('a'), &evidence)
                .await["state"],
            "held"
        );
    }
    assert!(
        git.operations()
            .iter()
            .all(|op| matches!(op.as_str(), "operation.show" | "capabilities"))
    );
}
#[tokio::test]
async fn a_separate_tasks_original_journal_cannot_be_read_through_another_retained_result() {
    // Owner-relayed identities in a synthetic socket fixture, not live evidence.
    let task = Uuid::parse_str("a3c72f06-d1a6-428e-bf67-b0f1c734a8c9").unwrap();
    let source = "3bed9a998cd64766f0cdcd0f0da636af21f66da3";
    let originals = [
        "3eed25ec-1f83-850e-b983-b0a718b1e9f5",
        "baf61f21-bce8-8713-a57c-74aed3f3212b",
    ]
    .map(|id| Uuid::parse_str(id).unwrap());
    let git = FakeGit::start(move |r| {
        if r["operation"] == "capabilities" {
            return json!({});
        }
        let id = Uuid::parse_str(r["params"]["target_operation_id"].as_str().unwrap()).unwrap();
        assert!(originals.contains(&id));
        let mut reply = journal_reply(id, "pending", Value::Null);
        reply["payload"]["params"]["source_ref"] = json!(task_branch(task));
        reply["payload"]["params"]["target"] = json!(task_branch(task));
        reply["payload"]["params"]["expected_source"]["hex"] = json!(source);
        reply["payload_hash"] = json!(crate::digest(&reply["payload"]).unwrap());
        reply
    });
    let (delivery, _) = delivery(&git).await;
    for original in originals {
        let evidence = delivery
            .inspect_publication(&target(), original, task, None, source)
            .await;
        assert_eq!(evidence["operation_id"], original.to_string());
        assert_eq!(evidence["expected_source"], source);
        for (other_task, other_source) in [(TASK, source), (task, sha('a').as_str())] {
            let refused = delivery
                .inspect_publication(&target(), original, other_task, None, other_source)
                .await;
            assert_eq!(refused["state"], "unavailable");
            assert!(refused.get("payload").is_none());
        }
    }
    assert!(
        git.operations()
            .iter()
            .all(|op| matches!(op.as_str(), "operation.show" | "capabilities"))
    );
}
#[tokio::test]
async fn rejection_class_requires_runtime_capability_and_a_definitive_original_outcome() {
    let original = Uuid::from_u128(9);
    let state = Arc::new(Mutex::new((json!({}), Value::Null)));
    let reply = state.clone();
    let git = FakeGit::start(move |r| {
        let (capability, journal) = reply.lock().unwrap().clone();
        if r["operation"] == "capabilities" {
            capability
        } else {
            journal
        }
    });
    let (delivery, _) = delivery(&git).await;
    for (supported, status, class, expected) in [
        (
            true,
            "complete",
            Some("hook_declined"),
            Some("hook_declined"),
        ),
        (false, "complete", Some("hook_declined"), None),
        (true, "complete", None, None),
        (true, "pending", Some("hook_declined"), None),
        (
            true,
            "complete",
            Some("private arbitrary text"),
            Some("unknown"),
        ),
    ] {
        let capabilities = json!({"capability_tokens":if supported { vec!["publication_rejection_classification_v1"] } else { vec![] }});
        let outcome = json!({"outcome":"rejected","reason":"remote_rejected","rejection_class":class,"stderr":"never disclose"});
        *state.lock().unwrap() = (capabilities, journal_reply(original, status, outcome));
        let evidence = delivery
            .inspect_publication(&target(), original, TASK, None, &sha('a'))
            .await;
        assert_eq!(evidence["rejection_class"], json!(expected));
        assert_eq!(
            evidence["classification_record"],
            if expected.is_some() {
                "recorded"
            } else {
                "not_recorded"
            }
        );
        assert_eq!(
            evidence["classification_capability"],
            if supported {
                "supported"
            } else {
                "unsupported"
            }
        );
        assert!(!evidence.to_string().contains("never disclose"));
        if status == "pending" {
            assert_eq!(evidence["outcome"], "indeterminate");
            assert!(evidence["reason"].is_null());
        }
    }
    // A token in an unrelated field is not the runtime's advertised capability.
    *state.lock().unwrap() = (
        json!({"features":["publication_rejection_classification_v1"]}),
        journal_reply(
            original,
            "complete",
            json!({"outcome":"rejected","reason":"remote_rejected","rejection_class":"hook_declined"}),
        ),
    );
    let evidence = delivery
        .inspect_publication(&target(), original, TASK, None, &sha('a'))
        .await;
    assert_eq!(evidence["classification_capability"], "unsupported");
    assert!(evidence["rejection_class"].is_null());
    assert!(
        git.operations()
            .iter()
            .all(|op| matches!(op.as_str(), "operation.show" | "capabilities"))
    );
}
#[tokio::test]
async fn original_success_is_recovered_only_with_valid_bound_oids_and_time() {
    let original = Uuid::from_u128(9);
    let state = Arc::new(Mutex::new(Value::Null));
    let reply = state.clone();
    let git = FakeGit::start(move |r| {
        if r["operation"] == "capabilities" {
            json!({})
        } else {
            reply.lock().unwrap().clone()
        }
    });
    let (delivery, _) = delivery(&git).await;
    let applied = json!({"outcome":"applied","target":task_branch(TASK),"old":{"algorithm":"sha1","hex":"0".repeat(40)},
        "new":{"algorithm":"sha1","hex":sha('a')},"acknowledged_at":"2026-11-25T15:00:00Z"});
    let mut wrong = applied.clone();
    wrong["new"]["hex"] = json!(sha('b'));
    let mut missing_time = applied.clone();
    missing_time
        .as_object_mut()
        .unwrap()
        .remove("acknowledged_at");
    for (outcome, published) in [(applied, true), (wrong, false), (missing_time, false)] {
        *state.lock().unwrap() = journal_reply(original, "complete", outcome);
        let journal = delivery
            .inspect_publication(&target(), original, TASK, None, &sha('a'))
            .await;
        assert_eq!(journal["published"], published);
        let recovered = delivery
            .reconcile_publication(&target(), Uuid::new_v4(), original, &sha('a'), &journal)
            .await;
        assert_eq!(
            recovered["state"],
            if published { "published" } else { "held" }
        );
        if published {
            assert_eq!(recovered["caused_by_this_operation"], false);
            assert_eq!(journal["receipt"]["outcome"], "applied");
            assert_eq!(journal["receipt"]["target"], task_branch(TASK));
            assert_eq!(journal["receipt"]["new"]["hex"], sha('a'));
            assert_eq!(journal["receipt"]["old"]["hex"], "0".repeat(40));
            assert!(journal["receipt"]["acknowledged_at"].as_str().is_some());
        } else {
            assert!(journal["receipt"].is_null());
        }
    }
    assert!(
        git.operations()
            .iter()
            .all(|op| matches!(op.as_str(), "operation.show" | "capabilities"))
    );
}
