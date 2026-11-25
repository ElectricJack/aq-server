use super::*;
#[tokio::test]
async fn producer_contracts_map_credential_and_question_without_private_text() {
    let e = Env::new(false).await;
    let source = Uuid::new_v4();
    assert!(
        e.policy
            .observe_failure_reason(e.origin(source), "subscription-profile", "timeout")
            .await
            .unwrap()
            .is_none()
    );
    let first = e
        .policy
        .observe_failure_reason(
            e.origin(source),
            "subscription-profile",
            "credential_invalid",
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first["outcome"], "notified");
    assert_eq!(
        e.policy
            .observe_failure_reason(
                e.origin(source),
                "subscription-profile",
                "credential_invalid"
            )
            .await
            .unwrap()
            .unwrap(),
        first
    );
    assert_eq!(
        e.policy
            .observe_failure_reason(
                e.origin(Uuid::new_v4()),
                "subscription-profile",
                "credential_invalid"
            )
            .await
            .unwrap()
            .unwrap()["outcome"],
        "already_open"
    );
    question_changes(&e, source).await;
    assert_eq!(e.messages().await.len(), 2);
    assert!(
        e.messages()
            .await
            .iter()
            .all(|s| s.contains("Open AQ to review"))
    );
}

#[tokio::test]
async fn worker_presence_loss_regain_reopens_once_and_respects_project_membership() {
    let e = Env::new(false).await;
    let node = Uuid::new_v4();
    let pool = &e.policy.service().pool;
    sqlx::query("INSERT INTO nodes(id,name,credential_hash,worker_version,protocol_version,enrolled_at,last_seen_at) VALUES($1,'fixture-worker',$2,'1',1,to_timestamp(500),to_timestamp(600))")
        .bind(node).bind(node.to_string()).execute(pool).await.unwrap();
    assert_eq!(e.policy.reconcile_workers(60, json!({})).await.unwrap(), 0);
    sqlx::query("UPDATE project_execution_bindings SET binding=binding || $2 WHERE project_id=$1")
        .bind(e.scope.project_id)
        .bind(json!({"allowed_worker_groups":["builders"]}))
        .execute(pool)
        .await
        .unwrap();
    let groups = json!({"builders":[node]});
    e.time.store(660_000, Ordering::SeqCst);
    assert_eq!(
        e.policy
            .reconcile_workers(60, groups.clone())
            .await
            .unwrap(),
        0,
        "boundary is still online"
    );
    e.time.store(660_001, Ordering::SeqCst);
    assert_eq!(
        e.policy
            .reconcile_workers(60, groups.clone())
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        e.policy
            .reconcile_workers(60, groups.clone())
            .await
            .unwrap(),
        0
    );
    assert_eq!(e.messages().await.len(), 1);
    sqlx::query("UPDATE nodes SET last_seen_at=to_timestamp(661) WHERE id=$1")
        .bind(node)
        .execute(pool)
        .await
        .unwrap();
    assert_eq!(
        e.policy
            .reconcile_workers(60, groups.clone())
            .await
            .unwrap(),
        1
    );
    e.time.store(722_000, Ordering::SeqCst);
    assert_eq!(
        e.policy
            .reconcile_workers(60, groups.clone())
            .await
            .unwrap(),
        1
    );
    assert_eq!(e.messages().await.len(), 2);
    sqlx::query("UPDATE nodes SET revoked_at=to_timestamp(722) WHERE id=$1")
        .bind(node)
        .execute(pool)
        .await
        .unwrap();
    assert_eq!(e.policy.reconcile_workers(60, groups).await.unwrap(), 1);
}

#[tokio::test]
async fn storage_pressure_opens_at_80_once_and_resolves_below_75() {
    use aq_server::node_storage::{StorageReport, record};
    let e = Env::new(false).await;
    let node = Uuid::new_v4();
    let pool = &e.policy.service().pool;
    sqlx::query("INSERT INTO nodes(id,name,credential_hash,worker_version,protocol_version,enrolled_at) VALUES($1,'fixture-worker',$2,'1',1,to_timestamp(500))")
        .bind(node).bind(node.to_string()).execute(pool).await.unwrap();
    sqlx::query("UPDATE project_execution_bindings SET binding=binding || $2 WHERE project_id=$1")
        .bind(e.scope.project_id)
        .bind(json!({"allowed_node_ids":[node]}))
        .execute(pool)
        .await
        .unwrap();
    let report = |id: Option<Uuid>, used: u64| async move {
        let disk = StorageReport {
            disk_total_bytes: 10_000,
            disk_used_bytes: used,
            disk_free_bytes: 10_000 - used,
            ..Default::default()
        };
        let mut conn = pool.acquire().await.unwrap();
        record(&mut conn, id, &disk, 1).await.unwrap();
    };
    let at = |used: u64| report(Some(node), used);
    let reconcile = || e.policy.reconcile_storage(json!({}));
    at(7_999).await;
    assert_eq!(reconcile().await.unwrap(), 0);
    at(8_000).await;
    assert_eq!(reconcile().await.unwrap(), 1, "crossing 80% opens");
    at(9_000).await;
    assert_eq!(reconcile().await.unwrap(), 0, "one episode while open");
    at(7_500).await;
    assert_eq!(reconcile().await.unwrap(), 0, "hysteresis keeps it open");
    at(7_499).await;
    assert_eq!(reconcile().await.unwrap(), 1, "below 75% resolves");
    assert_eq!(reconcile().await.unwrap(), 0);
    let messages = e.messages().await;
    assert_eq!(messages.len(), 1);
    assert!(messages[0].contains("Storage pressure"));
    assert!(messages[0].contains(&node.to_string()));
    let open: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM notification_episodes WHERE kind='storage.pressure' AND open",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(open, 0);
    // The central host reports in the same shape with subject `central`.
    report(None, 8_100).await;
    assert_eq!(reconcile().await.unwrap(), 1);
    let messages = e.messages().await;
    assert_eq!(messages.len(), 2);
    assert!(messages[1].contains("(central)"), "{}", messages[1]);
}

#[tokio::test]
async fn preserve_event_produces_one_sandbox_preserved_notification() {
    let e = Env::new(false).await;
    let node = Uuid::new_v4();
    let attempt = Uuid::new_v4();
    let source = Uuid::new_v4();
    let held = 5 * (1u64 << 30);
    let expiry = 1_700_000_000_000;
    let preserve = |source| {
        e.policy.observe_preservation(
            e.origin(source),
            aq_server::notifications::sources::Preservation {
                node,
                attempt,
                bytes: held,
                expires_at_ms: expiry,
            },
        )
    };
    preserve(source).await.unwrap();
    preserve(source).await.unwrap();
    preserve(Uuid::new_v4()).await.unwrap();
    let messages = e.messages().await;
    assert_eq!(messages.len(), 1, "{messages:?}");
    for expected in [
        "Sandbox preserved",
        &node.to_string(),
        "5.0 GiB",
        "2023-11-14 22:13 UTC",
        &format!("({attempt})"),
    ] {
        assert!(
            messages[0].contains(expected),
            "{} lacks {expected}",
            messages[0]
        );
    }
    // The node-facing port only accepts attempts that ran on that node.
    assert_preservation_scope(&e, node, source, |node, attempt| {
        aq_server::notifications::sources::Preservation {
            node,
            attempt,
            bytes: held,
            expires_at_ms: expiry,
        }
    })
    .await;
}

#[test]
fn graph_ready_projection_only_notifies_labelled_milestone_gates() {
    use aq_server::notifications::{policy::MILESTONE_GATE_LABEL, sources::milestone_events};
    let source = Uuid::new_v4();
    let scope = Scope {
        project_id: Uuid::new_v4(),
        workspace_id: Uuid::new_v4(),
    };
    let gate = Uuid::new_v4();
    let nodes = vec![
        json!({"id":Uuid::new_v4(),"labels":[]}),
        json!({"id":gate,"labels":[MILESTONE_GATE_LABEL]}),
    ];
    let events = milestone_events(source, scope.clone(), &nodes).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(
        (&events[0].subject, events[0].kind),
        (&gate.to_string(), Kind::MilestoneGateReady)
    );
    assert_eq!(
        json!(events),
        json!(milestone_events(source, scope, &nodes).unwrap())
    );
}

async fn question_changes(e: &Env, source: Uuid) {
    let decision = Uuid::new_v4();
    e.policy
        .observe_question(e.origin(source), decision, true)
        .await
        .unwrap();
    assert_eq!(
        e.policy
            .observe_question(e.origin(source), decision, true)
            .await
            .unwrap()["outcome"],
        "notified"
    ); // receipt replay
    assert_eq!(
        e.policy
            .observe_question(e.origin(Uuid::new_v4()), decision, false)
            .await
            .unwrap()["outcome"],
        "resolved"
    );
}

async fn assert_preservation_scope(
    e: &Env,
    node: Uuid,
    source: Uuid,
    preserved: impl Fn(Uuid, Uuid) -> aq_server::notifications::sources::Preservation,
) {
    use aq_server::notifications::Error;
    let pool = &e.policy.service().pool;
    assert_eq!(
        e.policy
            .observe_preserved(source, preserved(node, Uuid::new_v4()))
            .await,
        Err(Error::NotFound)
    );
    sqlx::query("INSERT INTO nodes(id,name,credential_hash,worker_version,protocol_version,enrolled_at) VALUES($1,'fixture-worker',$2,'1',1,to_timestamp(500))")
        .bind(node).bind(node.to_string()).execute(pool).await.unwrap();
    let legacy = Uuid::new_v4();
    sqlx::query("INSERT INTO execution_attempts(attempt_id,task_id,node_id,offer_id,session_id,state) VALUES($1,$2,$3,$4,$5,'{}')")
        .bind(legacy).bind(Uuid::new_v4()).bind(node).bind(Uuid::new_v4()).bind(Uuid::new_v4())
        .execute(pool).await.unwrap();
    assert_eq!(
        e.policy
            .observe_preserved(source, preserved(Uuid::new_v4(), legacy))
            .await,
        Err(Error::NotFound)
    );
    assert_eq!(
        e.policy
            .observe_preserved(Uuid::new_v4(), preserved(node, legacy))
            .await,
        Ok(None),
        "an attempt outside an enabled project is not reported"
    );
}
