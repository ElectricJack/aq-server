//! F4 automatic notification policy against isolated PostgreSQL. Fake transport
//! only: these tests never dispatch HTTP.
use aq_lifecycle_api::Scope;
use aq_notifications::{RetryPolicy, Route};
use aq_server::{
    notifications::{
        config::Loaded,
        persistence::{RouteRequest, Service},
        policy::{Event, Kind, Policy, Transition},
    },
    test_support::TestDb,
};
use chrono::{TimeZone, Utc};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicI64, Ordering},
};
use uuid::Uuid;

struct Env {
    _db: TestDb,
    policy: Policy,
    scope: Scope,
    time: Arc<AtomicI64>,
}
impl Env {
    async fn new(routed: bool) -> Self {
        let db = TestDb::create().await;
        let store = db.store().await;
        let pool = db.pool().await;
        let owner = store
            .bootstrap_owner(
                "policy-owner@localhost",
                "dev",
                "policy-owner@localhost",
                Utc.timestamp_millis_opt(1000).unwrap(),
            )
            .await
            .unwrap()
            .unwrap()
            .id;
        let scope = Scope {
            project_id: Uuid::new_v4(),
            workspace_id: Uuid::new_v4(),
        };
        sqlx::query("INSERT INTO project_execution_bindings(project_id,revision,binding,validated_at_ms,binding_observation) VALUES($1,1,$2,1,'{}')")
            .bind(scope.project_id).bind(json!({"enabled":true,"graph_workspace_id":scope.workspace_id})).execute(&pool).await.unwrap();
        // Window-aligned start so a burst stays inside one flood window.
        let time = Arc::new(AtomicI64::new(600_000));
        let t = time.clone();
        let service = Service {
            pool,
            configuration: routed.then(|| {
                Arc::new(Loaded::fixture(
                    1,
                    "discord-test",
                    1,
                    "http://127.0.0.1:1/api/v10/webhooks/42/fixture_token_1234567890",
                ))
            }),
            public_origin: "https://aq.example".into(),
            clock: Arc::new(move || Utc.timestamp_millis_opt(t.load(Ordering::SeqCst)).unwrap()),
        };
        service.install(Uuid::new_v4()).await.unwrap();
        if routed {
            service
                .set_route(
                    owner,
                    RouteRequest {
                        operation_id: Uuid::new_v4(),
                        expected_revision: 0,
                        route: Route {
                            route_id: Uuid::new_v4(),
                            revision: 1,
                            scope: scope.clone(),
                            recipient_user_id: owner,
                            endpoint_key: "discord-test".into(),
                            endpoint_revision: 1,
                            enabled: true,
                            retry: RetryPolicy {
                                max_attempts: 3,
                                max_elapsed_ms: 30000,
                                initial_delay_ms: 250,
                                max_delay_ms: 2000,
                                allow_duplicate_risk: false,
                            },
                        },
                    },
                )
                .await
                .unwrap();
        }
        Self {
            _db: db,
            policy: Policy::new(service),
            scope,
            time,
        }
    }
    fn event(&self, kind: Kind, subject: &str, transition: Transition) -> Event {
        Event {
            source_event_id: Uuid::new_v4(),
            kind,
            subject: subject.into(),
            scope: self.scope.clone(),
            transition,
            title: format!("{subject} needs attention"),
        }
    }
    async fn messages(&self) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT definition->>'summary' FROM notification_messages ORDER BY sequence",
        )
        .fetch_all(&self.policy.service().pool)
        .await
        .unwrap()
    }
    async fn operations(&self) -> i64 {
        sqlx::query_scalar("SELECT operation_count FROM notification_budget WHERE singleton")
            .fetch_one(&self.policy.service().pool)
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn credential_invalid_notifies_once_per_episode() {
    let e = Env::new(true).await;
    let opened = e.event(Kind::CredentialInvalid, "profile-a", Transition::Opened);
    let first = e.policy.observe(opened.clone()).await.unwrap();
    assert_eq!(first["outcome"], "notified");
    assert_eq!(first["external_state"], "routed");
    assert_eq!(first["generation"], 1);
    assert_eq!(e.messages().await.len(), 1);
    let operations = e.operations().await;

    // A replayed source event creates no second intent and no new operation.
    assert_eq!(e.policy.observe(opened.clone()).await.unwrap(), first);
    assert_eq!(e.messages().await.len(), 1);
    assert_eq!(e.operations().await, operations);

    // The same episode reported again by a new source event stays silent.
    let again = e
        .policy
        .observe(e.event(Kind::CredentialInvalid, "profile-a", Transition::Opened))
        .await
        .unwrap();
    assert_eq!(again["outcome"], "already_open");
    assert_eq!(e.messages().await.len(), 1);

    // An independent subject is its own episode.
    let other = e
        .policy
        .observe(e.event(Kind::CredentialInvalid, "profile-b", Transition::Opened))
        .await
        .unwrap();
    assert_eq!(other["outcome"], "notified");
    assert_eq!(e.messages().await.len(), 2);

    // Resolve, then reopen: exactly one more notification, with a new identity.
    let resolved = e
        .policy
        .observe(e.event(Kind::CredentialInvalid, "profile-a", Transition::Resolved))
        .await
        .unwrap();
    assert_eq!(resolved["outcome"], "resolved");
    let quiet = e
        .policy
        .observe(e.event(Kind::CredentialInvalid, "profile-a", Transition::Resolved))
        .await
        .unwrap();
    assert_eq!(quiet["outcome"], "not_open");
    e.time.fetch_add(120_000, Ordering::SeqCst);
    let reopened = e
        .policy
        .observe(e.event(Kind::CredentialInvalid, "profile-a", Transition::Opened))
        .await
        .unwrap();
    assert_eq!(reopened["outcome"], "notified");
    assert_eq!(reopened["generation"], 2);
    assert_ne!(reopened["message_id"], first["message_id"]);
    let messages = e.messages().await;
    assert_eq!(messages.len(), 3);
    assert!(
        messages[0].contains("Credential invalid"),
        "{}",
        messages[0]
    );
    assert!(messages[0].contains("profile-a"));
    assert!(messages.iter().all(|m| m.len() <= 600));
}

#[tokio::test]
async fn flood_of_failures_collapses_to_bounded_notifications() {
    let e = Env::new(true).await;
    for n in 0..50 {
        let outcome = e
            .policy
            .observe(e.event(
                Kind::TaskFailedOrBlocked,
                &format!("task-{n}"),
                Transition::Opened,
            ))
            .await
            .unwrap();
        let expected = if n < 3 { "notified" } else { "collapsed" };
        assert_eq!(outcome["outcome"], expected, "event {n}");
    }
    assert_eq!(e.messages().await.len(), 3);
    // The window is still open: nothing to summarize yet.
    assert_eq!(e.policy.flush().await.unwrap(), 0);
    assert_eq!(e.messages().await.len(), 3);
    e.time.fetch_add(60_000, Ordering::SeqCst);
    assert_eq!(e.policy.flush().await.unwrap(), 1);
    assert_eq!(e.policy.flush().await.unwrap(), 0);
    let messages = e.messages().await;
    assert_eq!(messages.len(), 4, "{messages:?}");
    assert!(messages[3].contains("47 more"), "{}", messages[3]);
    // Collapsed episodes are still open: a repeat does not notify.
    let repeat = e
        .policy
        .observe(e.event(Kind::TaskFailedOrBlocked, "task-49", Transition::Opened))
        .await
        .unwrap();
    assert_eq!(repeat["outcome"], "already_open");
    assert_eq!(e.messages().await.len(), 4);
}

#[tokio::test]
async fn no_external_route_is_reported_not_claimed() {
    let e = Env::new(false).await;
    let outcome = e
        .policy
        .observe(e.event(Kind::QuestionAsked, "question-1", Transition::Opened))
        .await
        .unwrap();
    assert_eq!(outcome["outcome"], "notified");
    assert_eq!(outcome["external_state"], "no_external_route");
    let deliveries: i64 = sqlx::query_scalar("SELECT count(*) FROM notification_deliveries")
        .fetch_one(&e.policy.service().pool)
        .await
        .unwrap();
    assert_eq!(deliveries, 0);
}

#[test]
fn every_kind_has_a_stable_wire_name_and_label() {
    let names: Vec<_> = Kind::ALL.iter().map(|k| k.as_str()).collect();
    assert_eq!(
        names,
        [
            "task.failed_or_blocked",
            "decision.needed",
            "question.asked",
            "credential.invalid",
            "milestone.gate_ready",
            "worker.offline",
            "storage.pressure",
            "sandbox.preserved",
            "session.stalled"
        ]
    );
    assert_eq!(
        Kind::from_failure_reason("credential_invalid"),
        Some(Kind::CredentialInvalid)
    );
    assert_eq!(Kind::from_failure_reason("timeout"), None);
}

#[tokio::test]
async fn producer_contracts_map_credential_and_question_without_private_text() {
    let e = Env::new(false).await;
    let source = Uuid::new_v4();
    assert!(
        e.policy
            .observe_failure_reason(source, e.scope.clone(), "subscription-profile", "timeout")
            .await
            .unwrap()
            .is_none()
    );
    let first = e
        .policy
        .observe_failure_reason(
            source,
            e.scope.clone(),
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
                source,
                e.scope.clone(),
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
                Uuid::new_v4(),
                e.scope.clone(),
                "subscription-profile",
                "credential_invalid"
            )
            .await
            .unwrap()
            .unwrap()["outcome"],
        "already_open"
    );
    let decision = Uuid::new_v4();
    e.policy
        .observe_question(source, e.scope.clone(), decision, true)
        .await
        .unwrap();
    assert_eq!(
        e.policy
            .observe_question(source, e.scope.clone(), decision, true)
            .await
            .unwrap()["outcome"],
        "notified"
    ); // receipt replay
    assert_eq!(
        e.policy
            .observe_question(Uuid::new_v4(), e.scope.clone(), decision, false)
            .await
            .unwrap()["outcome"],
        "resolved"
    );
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
    use aq_server::node_storage::{CENTRAL, StorageReport, record};
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
    let report = |subject: String, id: Option<Uuid>, used: u64| async move {
        let disk = StorageReport {
            disk_total_bytes: 10_000,
            disk_used_bytes: used,
            disk_free_bytes: 10_000 - used,
            ..Default::default()
        };
        let mut conn = pool.acquire().await.unwrap();
        record(&mut conn, &subject, id, &disk, 1).await.unwrap();
    };
    let at = |used: u64| report(node.to_string(), Some(node), used);
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
    report(CENTRAL.into(), None, 8_100).await;
    assert_eq!(reconcile().await.unwrap(), 1);
    let messages = e.messages().await;
    assert_eq!(messages.len(), 2);
    assert!(messages[1].contains("(central)"), "{}", messages[1]);
}

#[tokio::test]
async fn preserve_event_produces_one_sandbox_preserved_notification() {
    use aq_server::notifications::Error;
    let e = Env::new(false).await;
    let node = Uuid::new_v4();
    let attempt = Uuid::new_v4();
    let source = Uuid::new_v4();
    let held = 5 * (1u64 << 30);
    let expiry = 1_700_000_000_000;
    let preserve = |source| {
        e.policy
            .observe_preservation(source, e.scope.clone(), node, attempt, held, expiry)
    };
    preserve(source).await.unwrap();
    preserve(source).await.unwrap();
    preserve(Uuid::new_v4()).await.unwrap();
    let messages = e.messages().await;
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert!(messages[0].contains("Sandbox preserved"));
    assert!(messages[0].contains(&node.to_string()));
    assert!(messages[0].contains("5.0 GiB"));
    assert!(messages[0].contains("2023-11-14 22:13 UTC"));
    assert!(messages[0].contains(&format!("({attempt})")));
    // The node-facing port only accepts attempts that ran on that node.
    let pool = &e.policy.service().pool;
    assert_eq!(
        e.policy
            .observe_preserved(source, node, Uuid::new_v4(), held, expiry)
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
            .observe_preserved(source, Uuid::new_v4(), legacy, held, expiry)
            .await,
        Err(Error::NotFound)
    );
    assert_eq!(
        e.policy
            .observe_preserved(Uuid::new_v4(), node, legacy, held, expiry)
            .await,
        Ok(None),
        "an attempt outside an enabled project is not reported"
    );
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
    assert_eq!(events[0].subject, gate.to_string());
    assert_eq!(events[0].kind, Kind::MilestoneGateReady);
    assert_eq!(
        json!(events),
        json!(milestone_events(source, scope, &nodes).unwrap())
    );
}

#[tokio::test]
async fn committed_intents_retry_after_producer_already_sees_open_episode() {
    let e = Env::new(false).await;
    let pool = &e.policy.service().pool;
    sqlx::query(
        "UPDATE project_execution_bindings SET binding=jsonb_set(binding,'{enabled}','false')",
    )
    .execute(pool)
    .await
    .unwrap();
    assert!(
        e.policy
            .observe(e.event(Kind::QuestionAsked, "pending", Transition::Opened))
            .await
            .is_err()
    );
    assert!(e.messages().await.is_empty());
    sqlx::query(
        "UPDATE project_execution_bindings SET binding=jsonb_set(binding,'{enabled}','true')",
    )
    .execute(pool)
    .await
    .unwrap();
    assert_eq!(e.policy.retry_pending().await.unwrap(), 1);
    assert_eq!(e.policy.retry_pending().await.unwrap(), 0);
    assert_eq!(e.messages().await.len(), 1);
}

#[tokio::test]
async fn gc_preserves_open_pending_and_unflushed_state_and_uses_new_identity() {
    use aq_server::notifications::policy::RETENTION_MS;
    let e = Env::new(false).await;
    let old = e
        .policy
        .observe(e.event(Kind::QuestionAsked, "resolved", Transition::Opened))
        .await
        .unwrap();
    e.policy
        .observe(e.event(Kind::QuestionAsked, "resolved", Transition::Resolved))
        .await
        .unwrap();
    for name in ["open-a", "open-b", "collapsed"] {
        e.policy
            .observe(e.event(Kind::QuestionAsked, name, Transition::Opened))
            .await
            .unwrap();
    }
    let pool = &e.policy.service().pool;
    sqlx::query(
        "UPDATE project_execution_bindings SET binding=jsonb_set(binding,'{enabled}','false')",
    )
    .execute(pool)
    .await
    .unwrap();
    e.time.store(720_000, Ordering::SeqCst);
    assert!(
        e.policy
            .observe(e.event(Kind::QuestionAsked, "pending", Transition::Opened))
            .await
            .is_err()
    );
    e.policy
        .observe(e.event(Kind::QuestionAsked, "pending", Transition::Resolved))
        .await
        .unwrap();
    e.time.fetch_add(RETENTION_MS as i64 + 1, Ordering::SeqCst);
    e.policy.garbage_collect().await.unwrap();
    let subjects: Vec<String> =
        sqlx::query_scalar("SELECT subject FROM notification_episodes ORDER BY subject")
            .fetch_all(pool)
            .await
            .unwrap();
    assert_eq!(subjects, ["collapsed", "open-a", "open-b", "pending"]);
    let pending: i64 =
        sqlx::query_scalar("SELECT count(*) FROM notification_policy_events WHERE NOT submitted")
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(pending, 1);
    let windows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM notification_flood_windows WHERE collapsed>0 AND NOT submitted",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(windows, 1);
    sqlx::query(
        "UPDATE project_execution_bindings SET binding=jsonb_set(binding,'{enabled}','true')",
    )
    .execute(pool)
    .await
    .unwrap();
    assert_eq!(e.policy.retry_pending().await.unwrap(), 1);
    assert_eq!(e.policy.flush().await.unwrap(), 1);
    e.policy.garbage_collect().await.unwrap();
    let new = e
        .policy
        .observe(e.event(Kind::QuestionAsked, "resolved", Transition::Opened))
        .await
        .unwrap();
    assert_ne!(
        old["message_id"], new["message_id"],
        "GC must not reuse generation-1 message identity"
    );
}

#[tokio::test]
async fn gc_reclaims_capacity_in_all_three_policy_tables() {
    use aq_server::notifications::policy::RETENTION_MS;
    let e = Env::new(false).await;
    let pool = &e.policy.service().pool;
    e.time.store((2 * RETENTION_MS) as i64, Ordering::SeqCst);
    sqlx::query("INSERT INTO notification_episodes(episode_key,kind,subject,project_id,workspace_id,generation,open,opened_at_ms,resolved_at_ms,outcome,identity) SELECT 'old:'||n,'question.asked',n::text,$1,$2,1,FALSE,1,1,'collapsed',md5(n::text)::uuid FROM generate_series(1,4096) n")
        .bind(e.scope.project_id).bind(e.scope.workspace_id).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO notification_policy_events(source_event_id,request_digest,response,created_at_ms,submitted) SELECT md5(n::text)::uuid,md5(n::text)||md5(n::text),'{}',1,TRUE FROM generate_series(1,8192) n").execute(pool).await.unwrap();
    sqlx::query("INSERT INTO notification_flood_windows(project_id,workspace_id,window_start_ms) SELECT $1,$2,n*60000 FROM generate_series(1,1024) n")
        .bind(e.scope.project_id).bind(e.scope.workspace_id).execute(pool).await.unwrap();
    assert!(
        e.policy
            .observe(e.event(Kind::QuestionAsked, "new", Transition::Opened))
            .await
            .is_err()
    );
    assert_eq!(e.policy.garbage_collect().await.unwrap(), 1536);
    assert_eq!(
        e.policy
            .observe(e.event(Kind::QuestionAsked, "new", Transition::Opened))
            .await
            .unwrap()["outcome"],
        "notified"
    );
}
