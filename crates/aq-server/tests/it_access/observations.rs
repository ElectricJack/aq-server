use crate::common;
use aq_accounting::{
    Layout,
    projection::Projection,
    reconcile::{ParserAuthority, Policy},
    wire::*,
};
use aq_server::{
    execution::profiled::{Status, Stop},
    observations::{self, OwnerSource},
};
use aq_session_protocol::ProfiledGrant;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use common::profiled::{Fixture, NOW, artifact, fixture, insert};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;
fn text(s: &str) -> Text {
    Text::new(s).unwrap()
}

#[tokio::test]
async fn monthly_vm_budget_prices_authenticated_facts_and_refuses_placement() {
    use aq_accounting::vm_time::{Fact, Size};
    use aq_selection::NodeCeiling;
    use aq_server::profiled_execution::placement::{self, Ledger};
    let (f, _) = seeded(Some(true)).await;
    let pool = f.db.pool().await;
    let node = f.node.node_id;
    sqlx::query("INSERT INTO delegated_node_boots(node_id,boot_id,registration) VALUES($1,$2,$3)")
        .bind(node)
        .bind(f.node.boot_id)
        .bind(json!({"backend": {
            "kind":"boat", "capabilities":["vm-isolation"], "max_concurrent_sandboxes":4,
            "spend_ceiling":{"currency":"USD","micros":4_500},"templates":[]
        }}))
        .execute(&pool)
        .await
        .unwrap();
    let fact = Fact {
        attempt_id: f.profiled.offer.binding.attempt_id,
        sandbox_id: "boat-vm-1".into(),
        size: Size::Small,
        started_at_ms: NOW - 900_000,
        stopped_at_ms: None,
    };
    let post = |fact: Fact, credential: Option<String>| {
        let state = f.state.clone();
        async move {
            let mut request = Request::builder()
                .method("POST")
                .uri("/v1/worker/observations/vm-time")
                .header("content-type", "application/json");
            if let Some(credential) = credential {
                request = request.header("authorization", format!("Bearer {credential}"));
            }
            aq_server::build_worker_router(state)
                .oneshot(
                    request
                        .body(Body::from(serde_json::to_vec(&fact).unwrap()))
                        .unwrap(),
                )
                .await
                .unwrap()
                .status()
        }
    };
    let book = aq_accounting::pricing::RateCardBook::default();
    let ceiling = |now| {
        let pool = &pool;
        let book = &book;
        async move {
            placement::facts_in(pool, &Ledger { pool, book }, None, node, None, "ci@1", now)
                .await
                .unwrap()
                .unwrap()
                .ceiling
        }
    };
    // Existing attempts without VM evidence cannot silently look free.
    assert_eq!(ceiling(NOW).await, Some(NodeCeiling::SpendUnaccounted));
    // Admission may fence its own new reservation before a VM exists.
    let mut conn = pool.acquire().await.unwrap();
    let own = observations::vm_time::node(
        &mut conn,
        node,
        &aq_admission_sandbox::sandbox::Money {
            currency: "USD".into(),
            micros: 4_500,
        },
        NOW,
        Some(fact.attempt_id),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(own.micros, 0);
    drop(conn);
    assert_eq!(post(fact.clone(), None).await, StatusCode::UNAUTHORIZED);
    let mut other = fact.clone();
    other.attempt_id = Uuid::new_v4();
    assert_eq!(
        post(other, Some(f.credential.clone())).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        post(fact.clone(), Some(f.credential.clone())).await,
        StatusCode::OK
    );
    assert_eq!(
        post(fact.clone(), Some(f.credential.clone())).await,
        StatusCode::OK
    );
    // $0.018/h x 15 minutes = $0.0045. Replays do not double the cost.
    assert_eq!(ceiling(NOW - 200).await, None);
    assert_eq!(ceiling(NOW).await, Some(NodeCeiling::Spend));
    let mut changed = fact.clone();
    changed.size = Size::Xlarge;
    assert_eq!(
        post(changed, Some(f.credential.clone())).await,
        StatusCode::CONFLICT
    );
    let mut stopped = fact.clone();
    stopped.stopped_at_ms = Some(NOW);
    assert_eq!(
        post(stopped.clone(), Some(f.credential.clone())).await,
        StatusCode::OK
    );
    assert_eq!(
        post(stopped, Some(f.credential.clone())).await,
        StatusCode::OK
    );
    assert_eq!(
        post(fact, Some(f.credential.clone())).await,
        StatusCode::CONFLICT
    );
    let mut conn = pool.acquire().await.unwrap();
    let spend = observations::vm_time::node(
        &mut conn,
        node,
        &aq_admission_sandbox::sandbox::Money {
            currency: "USD".into(),
            micros: 4_500,
        },
        NOW + 3_600_000,
        None,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(spend.micros, 4_500); // stopped time costs nothing
    let month = observations::vm_time::month(NOW, 4_500).unwrap();
    assert_eq!(ceiling(month.next_month_start_ms).await, None);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM accounting_vm_time WHERE node_id=$1")
        .bind(node)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}
async fn seeded(transport_active: Option<bool>) -> (Fixture, OwnerSource) {
    let mut f = fixture().await;
    if let Some(active) = transport_active {
        let request = aq_observation_admission::Request {
            version: 1,
            operation_id: Uuid::new_v4(),
            node_id: f.node.node_id,
            operation: aq_observation_admission::Operation::Prepare {
                boot_id: f.node.boot_id,
                store_id: Uuid::new_v4(),
            },
        };
        let prepared = observations::transport::mutate(&f.state, f.node.node_id, &request)
            .await
            .unwrap();
        if active {
            let activation = aq_observation_admission::Request {
                version: 1,
                operation_id: Uuid::new_v4(),
                node_id: f.node.node_id,
                operation: aq_observation_admission::Operation::Activate {
                    marker: prepared.current_owner.marker,
                },
            };
            observations::transport::mutate(&f.state, f.node.node_id, &activation)
                .await
                .unwrap();
        }
    }
    f.profiled.status = Status::Granted;
    f.profiled.execution_deadline_ms = Some(NOW + 60_000);
    f.profiled.grant = Some(ProfiledGrant {
        binding: f.profiled.offer.binding.clone(),
        grant_id: Uuid::new_v4(),
        expires_at_ms: NOW + 5000,
    });
    f.profiled.stop = Stop::Running;
    insert(&f).await;
    let source = OwnerSource {
        binding: aq_admission::transport::lifecycle_binding(&f.profiled.offer).unwrap(),
        source_id: Uuid::new_v4(),
        generation: 1,
        native_session_key: text("native-key"),
        account_claim: text(&f.selection.choice.account_reference),
        policy: Policy {
            reference: artifact("accounting-policy", b"policy"),
            account_scope: Digest::of(b"account"),
            call_namespace: Digest::of(b"native-key"),
            provider: text(&f.selection.choice.provider),
            layout: Layout {
                policy: artifact("layout", b"layout"),
                input_includes_cache_read: false,
                input_includes_cache_write: false,
                output_includes_reasoning: false,
            },
            parsers: vec![ParserAuthority {
                parser: artifact("parser", b"parser"),
                category_claim: artifact("parser", b"parser"),
                priority: 1,
                complete_accounting_evidence: Some(artifact(
                    "synthetic-qualification",
                    b"test-only",
                )),
            }],
        },
        billing: None,
    };
    (f, source)
}
async fn setup() -> (Fixture, OwnerSource) {
    let (f, source) = seeded(Some(true)).await;
    observations::register_source(&f.state, &source)
        .await
        .unwrap();
    (f, source)
}
fn event(source: &OwnerSource, sequence: u64, input: u64) -> ObservationRecord {
    ObservationRecord::seal(
        Uuid::new_v4(),
        Observation {
            binding: source.binding.clone(),
            provenance: Provenance {
                source_id: source.source_id,
                source_generation: source.generation,
                source_kind: SourceKind::NativeTranscript,
                native_session_key: source.native_session_key.clone(),
                cursor: SourceCursor::Record {
                    id: text(&format!("record-{sequence}")),
                },
                parser: source.policy.parsers[0].parser.clone(),
                raw_record: RawRecordEvidence::Available {
                    digest: Digest::of(b"source"),
                    byte_length: 6,
                },
            },
            sequence,
            observed_at_ms: NOW,
            kind: ObservationKind::Usage {
                usage: Box::new(Usage {
                    call: ProviderCall {
                        provider: source.policy.provider.clone(),
                        account_reference: source.account_claim.clone(),
                        model: text("model"),
                        native_call_id: text("native-call"),
                    },
                    basis: CounterBasis::PerCall,
                    counters: Counters {
                        input: Counter::Known { value: input },
                        output: Counter::Known { value: 4 },
                        cache_read: Counter::Known { value: 0 },
                        cache_write: Counter::Known { value: 0 },
                        reasoning: Counter::Known { value: 0 },
                    },
                    category_semantics: CategorySemantics::DeclaredDisjoint {
                        policy: source.policy.parsers[0].category_claim.clone(),
                    },
                    cost: Cost::Unknown {
                        reason: UnknownReason::NotReported,
                    },
                    completeness: Completeness::ReportedComplete,
                    correction_of: None,
                }),
            },
        },
    )
    .unwrap()
}
fn packet(f: &Fixture, events: Vec<ObservationRecord>) -> Packet {
    Packet::seal(
        Uuid::new_v4(),
        Message::Batch(ObservationBatch {
            node_id: f.node.node_id,
            boot_id: f.node.boot_id,
            first_sequence: events.first().unwrap().body.sequence,
            last_sequence: events.last().unwrap().body.sequence,
            events,
        }),
    )
    .unwrap()
}
async fn projection(f: &Fixture, s: &OwnerSource) -> Projection {
    let v: Value =
        sqlx::query_scalar("SELECT projection FROM observation_projections WHERE lineage_id=$1")
            .bind(s.binding.lineage_id)
            .fetch_one(&f.db.pool().await)
            .await
            .unwrap();
    serde_json::from_value(v).unwrap()
}
async fn counts(f: &Fixture) -> (i64, i64) {
    sqlx::query_as("SELECT through_sequence,event_count FROM observation_nodes WHERE node_id=$1")
        .bind(f.node.node_id)
        .fetch_one(&f.db.pool().await)
        .await
        .unwrap()
}
#[tokio::test]
async fn exact_retries_corrections_and_history_replay_never_double_charge() {
    let (f, s) = setup().await;
    let first = event(&s, 1, 10);
    let p = packet(&f, vec![first.clone()]);
    let receipt = observations::ingest(&f.state, f.node.node_id, &p)
        .await
        .unwrap();
    receipt.validate_against(&p).unwrap();
    assert_eq!(
        observations::ingest(&f.state, f.node.node_id, &p)
            .await
            .unwrap(),
        receipt
    );
    assert_eq!(counts(&f).await, (1, 1));
    assert_eq!(projection(&f, &s).await.known_subtotals[0], 10);
    let repeat = packet(&f, vec![event(&s, 2, 10)]);
    observations::ingest(&f.state, f.node.node_id, &repeat)
        .await
        .unwrap();
    assert_eq!(projection(&f, &s).await.canonical_calls, 1);
    let mut correction = event(&s, 3, 7).body;
    if let ObservationKind::Usage { usage } = &mut correction.kind {
        usage.correction_of = Some(EventRef {
            event_id: first.event_id,
            digest: first.body_digest.clone(),
        });
    }
    let corrected = ObservationRecord::seal(Uuid::new_v4(), correction).unwrap();
    observations::ingest(&f.state, f.node.node_id, &packet(&f, vec![corrected]))
        .await
        .unwrap();
    assert_eq!(projection(&f, &s).await.known_subtotals[0], 7);
    assert_eq!(counts(&f).await, (3, 3));
    let old: Value = sqlx::query_scalar("SELECT record FROM observation_events WHERE event_id=$1")
        .bind(first.event_id)
        .fetch_one(&f.db.pool().await)
        .await
        .unwrap();
    assert_eq!(old, json!(first));
    let changed = Packet::seal(p.body.message_id, repeat.body.message).unwrap();
    assert!(matches!(
        observations::ingest(&f.state, f.node.node_id, &changed).await,
        Err(observations::Error::Conflict)
    ));
    drop(f);
}
#[tokio::test]
async fn a_refused_source_is_dropped_and_the_batch_still_advances_the_node_sequence() {
    use aq_observation_admission::{RefusalReason, RefusedSource};
    let (f, s) = setup().await;
    let mut foreign = event(&s, 2, 12).body;
    foreign.provenance.source_id = Uuid::new_v4();
    let unknown = foreign.provenance.source_id;
    let p = packet(
        &f,
        vec![
            event(&s, 1, 10),
            ObservationRecord::seal(Uuid::new_v4(), foreign).unwrap(),
        ],
    );
    let reply = observations::ingest_reply(&f.state, f.node.node_id, &p)
        .await
        .unwrap();
    assert_eq!(reply.receipt.through_sequence, Some(2));
    assert_eq!(
        reply.refused_sources,
        [RefusedSource {
            source_id: unknown,
            generation: 1,
            reason: RefusalReason::UnknownSource
        }]
    );
    // The good source's event landed and the sequence is past the batch.
    assert_eq!(counts(&f).await, (2, 1));
    assert_eq!(projection(&f, &s).await.known_subtotals[0], 10);
    let stored: Vec<i64> = sqlx::query_scalar("SELECT sequence FROM observation_events")
        .fetch_all(&f.db.pool().await)
        .await
        .unwrap();
    assert_eq!(stored, [1]);
    // A retry gets the same reply; an unenrolled source leaves no source row.
    assert_eq!(
        observations::ingest_reply(&f.state, f.node.node_id, &p)
            .await
            .unwrap(),
        reply
    );
    let refusals: i64 = sqlx::query_scalar("SELECT count(*) FROM observation_source_refusals")
        .fetch_one(&f.db.pool().await)
        .await
        .unwrap();
    assert_eq!(refusals, 0);
    // The next batch of the good source continues at 3.
    let next = packet(&f, vec![event(&s, 3, 10)]);
    observations::ingest(&f.state, f.node.node_id, &next)
        .await
        .unwrap();
    assert_eq!(counts(&f).await, (3, 2));
    // What still refuses a whole batch: a skipped sequence, another node.
    let skipped = packet(&f, vec![event(&s, 5, 1)]);
    assert!(matches!(
        observations::ingest(&f.state, f.node.node_id, &skipped).await,
        Err(observations::Error::Conflict)
    ));
    let p = packet(&f, vec![event(&s, 4, 10)]);
    assert!(
        observations::ingest(&f.state, Uuid::new_v4(), &p)
            .await
            .is_err()
    );
    assert_eq!(counts(&f).await, (3, 2));
    drop(f);
}
/// The node store's gap, source-change and fence records carry control
/// parsers the catalog entry does not list. They are admitted by kind; a fact
/// under a control parser refuses its source, durably. The ladder reads a
/// control record, or a refused source, as unknown: a monitoring incident,
/// never a nudge or a stop.
#[tokio::test]
async fn control_records_are_admitted_by_kind_and_read_as_unknown_by_the_ladder() {
    use aq_observation_admission::{CONTROL_PARSERS, FENCED_BY_RESTART, RefusalReason};
    use aq_server::stall::{
        engine::{self, Ports, Target},
        evidence::Explanation,
        policy::ExecutionPolicy,
        ports::DbPorts,
    };
    let (f, s) = setup().await;
    assert_eq!(
        s.policy.parsers.len(),
        1,
        "only the format parser is listed"
    );
    let pool = f.db.pool().await;
    let with = |sequence: u64, parser: Option<&str>, kind: Option<ObservationKind>| {
        let mut body = event(&s, sequence, 10).body;
        if let Some(id) = parser {
            // The digest is the worker's source file: any value is admitted.
            body.provenance.parser = artifact(id, format!("worker build {sequence}").as_bytes());
        }
        if let Some(kind) = kind {
            if let ObservationKind::Gap { reason, .. } = &kind {
                body.provenance.cursor = SourceCursor::Record {
                    id: text(&format!("{FENCED_BY_RESTART}{sequence}")),
                };
                body.provenance.raw_record = RawRecordEvidence::Unavailable {
                    reason: reason.clone(),
                };
            }
            body.kind = kind;
        }
        ObservationRecord::seal(Uuid::new_v4(), body).unwrap()
    };
    let gap = |reason| ObservationKind::Gap {
        reason,
        affected: None,
    };
    let turn = ObservationKind::TurnBoundary { in_turn: true };
    let admitted = packet(
        &f,
        vec![
            with(1, None, Some(turn.clone())),
            with(
                2,
                Some(CONTROL_PARSERS[0]),
                Some(gap(GapReason::SpoolOverflow)),
            ),
            with(3, Some(CONTROL_PARSERS[1]), Some(gap(GapReason::Truncated))),
            with(
                4,
                Some(CONTROL_PARSERS[2]),
                Some(gap(GapReason::ReplayUnavailable)),
            ),
        ],
    );
    let reply = observations::ingest_reply(&f.state, f.node.node_id, &admitted)
        .await
        .unwrap();
    assert!(reply.refused_sources.is_empty());
    assert_eq!(counts(&f).await, (4, 4));

    // Progress, then only control records: not progress, not proof of a stall.
    let attempt = s.binding.attempt_id;
    let target = Target {
        attempt_id: attempt,
        session_id: Uuid::new_v4(),
        task_id: f.profiled.offer.manifest.bindings.task_id,
        project_id: f.profiled.offer.manifest.bindings.project_id,
        workspace_id: f.profiled.offer.manifest.bindings.graph_id,
        generation: 1,
    };
    let policy = ExecutionPolicy::v1();
    let ports = DbPorts {
        state: f.state.clone(),
    };
    let evidence = ports.evidence(&target, &policy, NOW).await.unwrap();
    assert!(evidence.last_progress_ms.is_some());
    assert_eq!(
        evidence.explanation,
        Some(Explanation::ObservationsUnavailable)
    );
    // An hour without progress: one monitoring incident and no recovery.
    for minute in 0..=60 {
        engine::tick(&pool, &ports, &target, &policy, NOW + minute * 60_000)
            .await
            .unwrap();
    }
    let actions = |pool: sqlx::PgPool| async move {
        sqlx::query_scalar::<_, String>(
            "SELECT a.kind FROM stall_actions a JOIN stall_episodes e USING(episode_id) \
             WHERE e.attempt_id=$1 ORDER BY a.created_at_ms,a.ordinal",
        )
        .bind(attempt)
        .fetch_all(&pool)
        .await
        .unwrap()
    };
    assert_eq!(actions(pool.clone()).await, ["monitoring_incident"]);

    // A fact cannot ride a control parser: the source is refused, the batch lands.
    let smuggled = packet(&f, vec![with(5, Some(CONTROL_PARSERS[2]), None)]);
    let reply = observations::ingest_reply(&f.state, f.node.node_id, &smuggled)
        .await
        .unwrap();
    assert_eq!(reply.refused_sources[0].reason, RefusalReason::ControlShape);
    assert_eq!(counts(&f).await, (5, 4));
    let recorded: (String, i64) = sqlx::query_as(
        "SELECT reason,sequence FROM observation_source_refusals WHERE node_id=$1 AND source_id=$2",
    )
    .bind(f.node.node_id)
    .bind(s.source_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(recorded, ("control_shape".to_owned(), 5));
    // The refusal stands: later records of the source are dropped, with the
    // sequence still advancing, and the usage it tried to add never counted.
    let later = packet(&f, vec![with(6, None, Some(turn))]);
    let reply = observations::ingest_reply(&f.state, f.node.node_id, &later)
        .await
        .unwrap();
    assert_eq!(reply.refused_sources[0].reason, RefusalReason::Refused);
    assert_eq!(counts(&f).await, (6, 4));
    assert_eq!(projection(&f, &s).await.known_subtotals[0], 0);
    // A refused source is unknown too, and adds no ladder action.
    let evidence = ports.evidence(&target, &policy, NOW).await.unwrap();
    assert_eq!(
        evidence.explanation,
        Some(Explanation::ObservationsUnavailable)
    );
    for minute in 61..=90 {
        engine::tick(&pool, &ports, &target, &policy, NOW + minute * 60_000)
            .await
            .unwrap();
    }
    assert_eq!(actions(pool).await, ["monitoring_incident"]);
    drop(f);
}
#[tokio::test]
async fn late_known_source_is_observation_only_and_revocation_fences_even_replay() {
    let (mut f, s) = setup().await;
    f.profiled.status = Status::Cancelling;
    f.profiled.cancellation_id = Some(Uuid::new_v4());
    sqlx::query("UPDATE execution_attempts SET state=$2 WHERE attempt_id=$1")
        .bind(s.binding.attempt_id)
        .bind(json!(f.profiled))
        .execute(&f.db.pool().await)
        .await
        .unwrap();
    let p = packet(&f, vec![event(&s, 1, 10)]);
    observations::ingest(&f.state, f.node.node_id, &p)
        .await
        .unwrap();
    let mut fresh = s.clone();
    observations::register_source(&f.state, &s).await.unwrap();
    fresh.source_id = Uuid::new_v4();
    assert!(
        observations::register_source(&f.state, &fresh)
            .await
            .is_err()
    );
    let (held, state): (bool, Value) =
        sqlx::query_as("SELECT holds_task,state FROM execution_attempts WHERE attempt_id=$1")
            .bind(s.binding.attempt_id)
            .fetch_one(&f.db.pool().await)
            .await
            .unwrap();
    assert!(held);
    assert_eq!(state, json!(f.profiled));
    sqlx::query("UPDATE nodes SET revoked_at=now() WHERE id=$1")
        .bind(f.node.node_id)
        .execute(&f.db.pool().await)
        .await
        .unwrap();
    assert!(matches!(
        observations::ingest(&f.state, f.node.node_id, &p).await,
        Err(observations::Error::Refused)
    ));
    assert_eq!(counts(&f).await, (1, 1));
    drop(f);
}
#[tokio::test]
async fn missing_cumulative_baseline_and_account_mismatch_are_audited_as_unknown() {
    let (f, s) = setup().await;
    let mut a = event(&s, 1, 100).body;
    if let ObservationKind::Usage { usage } = &mut a.kind {
        usage.basis = CounterBasis::Cumulative {
            counter_id: text("total"),
            counter_generation: 1,
        };
    }
    let mut b = event(&s, 2, 10).body;
    if let ObservationKind::Usage { usage } = &mut b.kind {
        usage.call.account_reference = text("foreign-account");
    }
    let p = packet(
        &f,
        vec![
            ObservationRecord::seal(Uuid::new_v4(), a).unwrap(),
            ObservationRecord::seal(Uuid::new_v4(), b).unwrap(),
        ],
    );
    observations::ingest(&f.state, f.node.node_id, &p)
        .await
        .unwrap();
    let projection = projection(&f, &s).await;
    assert_eq!(projection.canonical_calls, 0);
    assert_eq!(projection.unresolved_events, 2);
    assert!(matches!(
        projection.counters().unwrap().input,
        Counter::Unknown { .. }
    ));
    assert_eq!(counts(&f).await, (2, 2));
    drop(f);
}
#[tokio::test]
async fn quota_and_database_failure_roll_back_all_accounting_effects() {
    let (f, s) = setup().await;
    let pool = f.db.pool().await;
    sqlx::query("UPDATE observation_nodes SET retained_bytes=134217728 WHERE node_id=$1")
        .bind(f.node.node_id)
        .execute(&pool)
        .await
        .unwrap();
    let p = packet(&f, vec![event(&s, 1, 10)]);
    assert!(matches!(
        observations::ingest(&f.state, f.node.node_id, &p).await,
        Err(observations::Error::Capacity)
    ));
    assert_eq!(counts(&f).await, (0, 0));
    sqlx::query("UPDATE observation_nodes SET retained_bytes=1000 WHERE node_id=$1")
        .bind(f.node.node_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql("CREATE FUNCTION observation_test_failure() RETURNS TRIGGER LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected persistence failure'; END $$; CREATE TRIGGER fail_receipt BEFORE INSERT ON observation_packets FOR EACH ROW EXECUTE FUNCTION observation_test_failure();").execute(&pool).await.unwrap();
    assert!(
        observations::ingest(&f.state, f.node.node_id, &p)
            .await
            .is_err()
    );
    assert_eq!(counts(&f).await, (0, 0));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM observation_facts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    sqlx::query("DROP TRIGGER fail_receipt ON observation_packets")
        .execute(&pool)
        .await
        .unwrap();
    observations::ingest(&f.state, f.node.node_id, &p)
        .await
        .unwrap();
    assert!(
        sqlx::query("DELETE FROM observation_events")
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE observation_sources SET generation=2")
            .execute(&pool)
            .await
            .is_err()
    );
    drop(f);
}

#[tokio::test]
async fn concurrent_exact_delivery_returns_one_receipt_and_one_charge() {
    let (f, s) = setup().await;
    let p = packet(&f, vec![event(&s, 1, 11)]);
    let (a, b) = tokio::join!(
        observations::ingest(&f.state, f.node.node_id, &p),
        observations::ingest(&f.state, f.node.node_id, &p)
    );
    assert_eq!(a.unwrap(), b.unwrap());
    assert_eq!(counts(&f).await, (1, 1));
    assert_eq!(projection(&f, &s).await.known_subtotals[0], 11);
    drop(f);
}
#[tokio::test]
async fn worker_route_requires_node_credentials_and_rejects_wrong_packet_scope() {
    let (f, s) = setup().await;
    let p = packet(&f, vec![event(&s, 1, 10)]);
    let router = aq_server::build_worker_router(f.state.clone());
    let hidden = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/observations/sessions/{}/usage",
                    s.binding.attempt_id
                ))
                .header("authorization", format!("Bearer {}", f.credential))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(hidden.status(), StatusCode::NOT_FOUND);
    let request = |credential: &str, body: Vec<u8>| {
        Request::builder()
            .method("POST")
            .uri("/v1/worker/observations/batch")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {credential}"))
            .body(Body::from(body))
            .unwrap()
    };
    let bytes = serde_json::to_vec(&p).unwrap();
    let r = router
        .clone()
        .oneshot(request("aqa_agent-token", bytes.clone()))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let r = router
        .clone()
        .oneshot(request("aqn_wrong", bytes.clone()))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(counts(&f).await, (0, 0));
    let mut foreign = event(&s, 1, 10).body;
    let foreign_node = Uuid::new_v4();
    if let SessionIdentity::Remote { node_id, .. } = &mut foreign.binding.session {
        *node_id = foreign_node;
    }
    let foreign = ObservationRecord::seal(Uuid::new_v4(), foreign).unwrap();
    let foreign = Packet::seal(
        Uuid::new_v4(),
        Message::Batch(ObservationBatch {
            node_id: foreign_node,
            boot_id: f.node.boot_id,
            first_sequence: 1,
            last_sequence: 1,
            events: vec![foreign],
        }),
    )
    .unwrap();
    let r = router
        .clone()
        .oneshot(request(
            &f.credential,
            serde_json::to_vec(&foreign).unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    assert_eq!(counts(&f).await, (0, 0));
    let r = router
        .clone()
        .oneshot(request(&f.credential, bytes.clone()))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers()["cache-control"], "no-store");
    let got: Receipt =
        serde_json::from_slice(&axum::body::to_bytes(r.into_body(), 2048).await.unwrap()).unwrap();
    got.validate_against(&p).unwrap();
    let r = router
        .clone()
        .oneshot(request(&f.credential, vec![b' '; MAX_BATCH_BYTES + 1]))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::PAYLOAD_TOO_LARGE);
    // Cookie/human identities never authorize a node transport.
    let req = Request::builder()
        .method("POST")
        .uri("/v1/worker/observations/batch")
        .header("content-type", "application/json")
        .header("cookie", "session=human")
        .body(Body::from(bytes))
        .unwrap();
    assert_eq!(
        router.oneshot(req).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(counts(&f).await, (1, 1));
    drop(f);
}

async fn successor(f: &mut Fixture, previous: &OwnerSource) -> OwnerSource {
    use aq_server::execution::profiled::{Effects, Retention};
    use aq_session_protocol::canonical_digest;
    let old = f.profiled.clone();
    let old_attempt = old.offer.binding.attempt_id;
    let mut finished = old.clone();
    finished.status = Status::Finished;
    finished.stop = Stop::Stopped {
        receipt: artifact("synthetic-stop", b"test-only"),
    };
    finished.retention = Retention::Retained {
        receipt: artifact("synthetic-retained-source", b"test-only"),
    };
    finished.effects = Effects::Quiescent {
        receipt: artifact("synthetic-effects", b"test-only"),
    };
    finished.validate().unwrap();
    let pool = f.db.pool().await;
    sqlx::query("UPDATE execution_attempts SET state=$2,holds_task=false WHERE attempt_id=$1")
        .bind(old_attempt)
        .bind(json!(finished))
        .execute(&pool)
        .await
        .unwrap();
    let mut next = old;
    let attempt = Uuid::new_v4();
    let session = Uuid::new_v4();
    let offer = Uuid::new_v4();
    next.offer.binding.attempt_id = attempt;
    next.offer.binding.offer_id = offer;
    if let SessionIdentity::Remote {
        attempt_id,
        session_id,
        ..
    } = &mut next.offer.binding.session
    {
        *attempt_id = attempt;
        *session_id = session;
    }
    next.offer.manifest.bindings.session = next.offer.binding.session.clone();
    next.offer.manifest.bindings.attempt_id = attempt;
    next.offer.binding.launch_manifest_digest = canonical_digest(&next.offer.manifest).unwrap();
    next.grant = Some(ProfiledGrant {
        binding: next.offer.binding.clone(),
        grant_id: Uuid::new_v4(),
        expires_at_ms: NOW + 5000,
    });
    next.validate().unwrap();
    sqlx::query("INSERT INTO execution_attempts(attempt_id,task_id,node_id,offer_id,session_id,kind,state,execution_deadline_ms) VALUES($1,$2,$3,$4,$5,'profiled_v1',$6,$7)").bind(attempt).bind(previous.binding.task_id).bind(f.node.node_id).bind(offer).bind(session).bind(json!(next)).bind(next.execution_deadline_ms.unwrap() as i64).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO profiled_launches(attempt_id,decision_id,source_observation_id,launch_binding,launch_manifest,manifest_digest,execution_spec,execution_spec_digest,graph_task_observation,source_binding,offer_expires_at_ms) SELECT $2,decision_id,source_observation_id,$3,$4,$5,execution_spec,execution_spec_digest,graph_task_observation,source_binding,offer_expires_at_ms FROM profiled_launches WHERE attempt_id=$1").bind(old_attempt).bind(attempt).bind(json!(next.offer.binding)).bind(json!(next.offer.manifest)).bind(next.offer.binding.launch_manifest_digest.as_str()).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO intelligence_attempt_selections(attempt_id,selection_digest,selection) SELECT $2,selection_digest,selection FROM intelligence_attempt_selections WHERE attempt_id=$1").bind(old_attempt).bind(attempt).execute(&pool).await.unwrap();
    let mut source = previous.clone();
    source.binding = aq_admission::transport::lifecycle_binding(&next.offer).unwrap();
    source.source_id = Uuid::new_v4();
    f.profiled = next;
    observations::register_source(&f.state, &source)
        .await
        .unwrap();
    source
}

#[tokio::test]
async fn successor_correction_preserves_original_attempt_ownership() {
    let (mut f, first_source) = setup().await;
    let original = event(&first_source, 1, 10);
    observations::ingest(
        &f.state,
        f.node.node_id,
        &packet(&f, vec![original.clone()]),
    )
    .await
    .unwrap();
    let next_source = successor(&mut f, &first_source).await;
    let mut corrected = event(&next_source, 2, 7).body;
    if let ObservationKind::Usage { usage } = &mut corrected.kind {
        usage.correction_of = Some(EventRef {
            event_id: original.event_id,
            digest: original.body_digest,
        });
    }
    observations::ingest(
        &f.state,
        f.node.node_id,
        &packet(
            &f,
            vec![ObservationRecord::seal(Uuid::new_v4(), corrected).unwrap()],
        ),
    )
    .await
    .unwrap();
    let pool = f.db.pool().await;
    let read = |attempt| {
        let pool = pool.clone();
        async move {
            let v: Value = sqlx::query_scalar(
                "SELECT projection FROM observation_attempt_projections WHERE attempt_id=$1",
            )
            .bind(attempt)
            .fetch_one(&pool)
            .await
            .unwrap();
            serde_json::from_value::<Projection>(v).unwrap()
        }
    };
    let original = read(first_source.binding.attempt_id).await;
    let successor = read(next_source.binding.attempt_id).await;
    assert_eq!(original.canonical_calls, 1);
    assert_eq!(original.known_subtotals[0], 7);
    assert_eq!(successor.canonical_calls, 0);
    let original_view =
        observations::inspection::usage(&f.state, f.actor, first_source.binding.attempt_id)
            .await
            .unwrap();
    let successor_view =
        observations::inspection::usage(&f.state, f.actor, next_source.binding.attempt_id)
            .await
            .unwrap();
    assert_eq!(original_view.known_category_subtotals[0], 7);
    assert_eq!(successor_view.canonical_calls, 0);
    assert_eq!(successor_view.known_category_subtotals, [0; 5]);
    assert_eq!(projection(&f, &first_source).await.known_subtotals[0], 7);
    assert!(
        sqlx::query("UPDATE observation_call_heads SET owner_attempt_id=$1")
            .bind(next_source.binding.attempt_id)
            .execute(&pool)
            .await
            .is_err()
    );
    drop(f);
}

#[tokio::test]
async fn inspection_uses_exact_acl_and_stable_bounded_snapshot_pages() {
    use std::sync::atomic::Ordering;
    let (f, s) = setup().await;
    let pool = f.db.pool().await;
    let distinct = |seq: u64| {
        let mut body = event(&s, seq, seq * 10).body;
        if let ObservationKind::Usage { usage } = &mut body.kind {
            usage.call.native_call_id = text(&format!("native-{seq}"));
        }
        ObservationRecord::seal(Uuid::new_v4(), body).unwrap()
    };
    observations::ingest(
        &f.state,
        f.node.node_id,
        &packet(&f, vec![distinct(1), distinct(2)]),
    )
    .await
    .unwrap();
    let first =
        observations::inspection::events(&f.state, f.actor, s.binding.attempt_id, 0, None, 1)
            .await
            .unwrap();
    first.validate().unwrap();
    assert_eq!(first.through_sequence, 2);
    assert_eq!(first.next_after, Some(1));
    assert!(!first.complete_for_snapshot);
    observations::ingest(&f.state, f.node.node_id, &packet(&f, vec![distinct(3)]))
        .await
        .unwrap();
    let second =
        observations::inspection::events(&f.state, f.actor, s.binding.attempt_id, 1, Some(2), 1)
            .await
            .unwrap();
    assert_eq!(second.events.len(), 1);
    assert_eq!(second.events[0].record.body.sequence, 2);
    assert!(second.complete_for_snapshot);
    let latest =
        observations::inspection::events(&f.state, f.actor, s.binding.attempt_id, 2, None, 16)
            .await
            .unwrap();
    assert_eq!(latest.events.len(), 1);
    assert_eq!(latest.events[0].record.body.sequence, 3);
    optional_events_browser(&f, &s.binding).await;
    assert!(
        observations::inspection::events(&f.state, f.actor, s.binding.attempt_id, 4, None, 16)
            .await
            .is_err()
    );
    assert!(
        observations::inspection::events(&f.state, f.actor, s.binding.attempt_id, 0, None, 17)
            .await
            .is_err()
    );
    let viewer = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email,role,created_at,issuer,subject) VALUES($1,'observer@fixture.test','viewer',now(),'dev','observer@fixture.test')").bind(viewer).execute(&pool).await.unwrap();
    assert!(
        observations::inspection::usage(&f.state, viewer, s.binding.attempt_id)
            .await
            .is_err()
    );
    let key = aq_server::session_access::session_key(&s.binding.session).unwrap();
    sqlx::query("INSERT INTO session_inspection_acl(session_key,session_identity,principal_id,revision,can_inspect,expires_at_ms) VALUES($1,$2,$3,1,true,$4)").bind(&key).bind(json!(s.binding.session)).bind(viewer).bind((NOW+10)as i64).execute(&pool).await.unwrap();
    let view = observations::inspection::usage(&f.state, viewer, s.binding.attempt_id)
        .await
        .unwrap();
    view.validate().unwrap();
    assert_eq!(view.canonical_calls, 3);
    assert_eq!(view.known_category_subtotals[0], 60);
    assert!(matches!(
        view.usage.completeness,
        aq_lifecycle_api::AccountingCompleteness::Incomplete { .. }
    ));
    let human = aq_server::build_admin_router(f.state.clone());
    let hidden = human
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/worker/observations/batch")
                .header("x-aq-dev-email", "observer@fixture.test")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(hidden.status(), StatusCode::NOT_FOUND);
    let request = |tail: &str| {
        Request::builder()
            .uri(format!(
                "/api/observations/sessions/{}/{tail}",
                s.binding.attempt_id
            ))
            .header("x-aq-dev-email", "observer@fixture.test")
            .body(Body::empty())
            .unwrap()
    };
    let response = human.clone().oneshot(request("usage")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let bytes = axum::body::to_bytes(response.into_body(), 16384)
        .await
        .unwrap();
    aq_observation_api::decode_usage(&bytes).unwrap();
    let response = human
        .clone()
        .oneshot(request("events?limit=1"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 262144)
        .await
        .unwrap();
    aq_observation_api::decode_events(&bytes).unwrap();
    f.time.store((NOW + 11) as i64, Ordering::SeqCst);
    assert!(
        observations::inspection::usage(&f.state, viewer, s.binding.attempt_id)
            .await
            .is_err()
    );
    assert_eq!(
        human
            .clone()
            .oneshot(request("usage"))
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert!(
        observations::inspection::usage(&f.state, f.actor, s.binding.attempt_id)
            .await
            .is_ok()
    );
    sqlx::query("UPDATE session_inspection_acl SET revision=revision+1,can_inspect=false WHERE session_key=$1 AND principal_id=$2").bind(key).bind(viewer).execute(&pool).await.unwrap();
    f.time.store(NOW as i64, Ordering::SeqCst);
    assert_eq!(
        human.oneshot(request("usage")).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    drop(f);
}

// Opt-in paired browser check uses the real registered source and admitted
// records above. Synthetic fixtures qualify inspection, never model execution.
async fn optional_events_browser(f: &Fixture, binding: &Binding) {
    let Some(script) = std::env::var_os("AQ_TEST_EVENTS_BROWSER_SCRIPT") else {
        assert!(std::env::var_os("AQ_TEST_EVENTS_DASHBOARD_DIR").is_none());
        return;
    };
    let bundle = std::env::var_os("AQ_TEST_EVENTS_DASHBOARD_DIR")
        .expect("an explicit events browser probe requires a compiled bundle");
    let assets =
        aq_server::dashboard_assets::DashboardAssets::load(std::path::Path::new(&bundle)).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let mut state = f.state.clone();
    let mut config = (*state.config).clone();
    config.server.public_origin = origin.clone();
    state.config = std::sync::Arc::new(config);
    let router = aq_server::build_admin_router_with_dashboard_and_policy_owners(
        state,
        None,
        None,
        std::sync::Arc::new(assets),
    );
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let SessionIdentity::Remote { session_id, .. } = binding.session else {
        panic!("remote events fixture")
    };
    let mut probe = tokio::process::Command::new("node")
        .arg(script)
        .env("AQ_LIVE_URL", origin)
        .env("AQ_LIVE_SESSION_ID", session_id.to_string())
        .env("AQ_LIVE_EVENTS_EXPECT", "nonempty")
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(30), probe.wait()).await;
    server.abort();
    assert!(
        result
            .expect("bounded real admitted-events browser probe")
            .unwrap()
            .success()
    );
}

#[tokio::test]
async fn source_enrollment_and_batch_ingress_require_activated_store() {
    for initial in [None, Some(false)] {
        let (f, source) = seeded(initial).await;
        assert!(
            observations::register_source(&f.state, &source)
                .await
                .is_err()
        );
        assert!(
            observations::ingest(
                &f.state,
                f.node.node_id,
                &packet(&f, vec![event(&source, 1, 10)])
            )
            .await
            .is_err()
        );
        if initial.is_none() {
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM observation_nodes WHERE node_id=$1)",
            )
            .bind(f.node.node_id)
            .fetch_one(&f.db.pool().await)
            .await
            .unwrap();
            assert!(
                !exists,
                "refused registration must roll back ledger creation"
            );
        } else {
            assert_eq!(counts(&f).await, (0, 0));
        }
        if initial.is_some() {
            let owner = observations::transport::status(&f.state, f.node.node_id)
                .await
                .unwrap()
                .owner
                .unwrap();
            let activation = aq_observation_admission::Request {
                version: 1,
                operation_id: Uuid::new_v4(),
                node_id: f.node.node_id,
                operation: aq_observation_admission::Operation::Activate {
                    marker: owner.marker,
                },
            };
            observations::transport::mutate(&f.state, f.node.node_id, &activation)
                .await
                .unwrap();
            observations::register_source(&f.state, &source)
                .await
                .unwrap();
            observations::ingest(
                &f.state,
                f.node.node_id,
                &packet(&f, vec![event(&source, 1, 10)]),
            )
            .await
            .unwrap();
            assert_eq!(counts(&f).await, (1, 1));
        }
    }
}
#[tokio::test]
async fn admitted_http_context_cannot_write_or_replay_after_credential_rotation() {
    let (f, source) = setup().await;
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "authorization",
        format!("Bearer {}", f.credential).parse().unwrap(),
    );
    let context = aq_server::worker_api::authenticate_context(&f.state, &headers)
        .await
        .unwrap();
    let packet = packet(&f, vec![event(&source, 1, 10)]);
    observations::ingest_authenticated(&f.state, &context, &packet)
        .await
        .unwrap();
    let replacement = aq_server::tokens::new_node_credential();
    sqlx::query("UPDATE nodes SET credential_hash=$2 WHERE id=$1")
        .bind(f.node.node_id)
        .bind(aq_server::tokens::verifier(&replacement))
        .execute(&f.db.pool().await)
        .await
        .unwrap();
    assert!(
        observations::ingest_authenticated(&f.state, &context, &packet)
            .await
            .is_err(),
        "even receipt replay rechecks the exact current credential"
    );
    assert!(
        observations::transport::status_authenticated(&f.state, &context)
            .await
            .is_err()
    );
    assert_eq!(counts(&f).await, (1, 1));
    headers.insert(
        "authorization",
        format!("Bearer {replacement}").parse().unwrap(),
    );
    let current = aq_server::worker_api::authenticate_context(&f.state, &headers)
        .await
        .unwrap();
    observations::ingest_authenticated(&f.state, &current, &packet)
        .await
        .unwrap();
    assert_eq!(counts(&f).await, (1, 1));
}

// H2: production usage accounting. The catalog, credential kind and rate cards
// are owner configuration; the worker only supplies batches through the route.
mod accounting {
    use super::*;
    use aq_accounting::{
        Category,
        pricing::{RateCard, RateCardRevision, Rule},
    };
    use aq_server::intelligence::config::{
        AccountingCatalog, CredentialConfig, IntelligenceConfig, SourcePolicy,
    };
    use aq_server::observations::{budget, catalog};
    use std::sync::Arc;

    fn layout() -> Layout {
        Layout {
            policy: artifact("layout", b"layout"),
            input_includes_cache_read: false,
            input_includes_cache_write: false,
            output_includes_reasoning: false,
        }
    }
    fn card(f: &Fixture, revision: u64, input_rate: u64) -> RateCardRevision {
        let reference = |r: u64| artifact(&format!("rate-card-r{r}"), b"rates");
        RateCardRevision {
            revision,
            corrects: (revision > 1).then(|| reference(revision - 1)),
            card: RateCard {
                reference: reference(revision),
                allocation_policy: artifact("allocation", b"per-call"),
                currency: text("USD"),
                provider: text(&f.selection.choice.provider),
                model: text("model"),
                layout: layout(),
                rules: [
                    (Category::Input, input_rate),
                    (Category::Output, 15_000_000),
                    (Category::CacheRead, 0),
                    (Category::CacheWrite, 0),
                    (Category::Reasoning, 0),
                ]
                .into_iter()
                .map(|(category, rate)| Rule {
                    category,
                    subtract: vec![],
                    microunits_per_million_tokens: rate,
                })
                .collect(),
            },
        }
    }
    fn install(f: &mut Fixture, kind: aq_selection::CredentialKind, cards: Vec<RateCardRevision>) {
        let choice = &f.selection.choice;
        let credential = aq_selection::AuthorizedCredential {
            allowed_backends: aq_selection::default_allowed_backends(),
            profile: choice.credential_profile.clone(),
            provider: choice.provider.clone(),
            account_reference: choice.account_reference.clone(),
            endpoint_reference: choice.endpoint_reference.clone(),
            revision: 1,
            enabled: true,
            provisionable: false,
            material_owner: None,
        };
        let accounting = AccountingCatalog {
            sources: vec![SourcePolicy {
                provider: choice.provider.clone(),
                harness: choice.harness.clone(),
                reference: artifact("aq.accounting.source", b"catalog"),
                layout: layout(),
                parsers: vec![ParserAuthority {
                    parser: artifact("parser", b"parser"),
                    category_claim: artifact("parser", b"parser"),
                    priority: 1,
                    complete_accounting_evidence: Some(artifact(
                        "synthetic-qualification",
                        b"test-only",
                    )),
                }],
            }],
            rate_cards: cards,
        };
        let mut config = (*f.state.config).clone();
        config.intelligence = Some(Arc::new(
            IntelligenceConfig {
                revision: 1,
                credentials: vec![CredentialConfig {
                    kind,
                    allowed_backends: aq_selection::default_allowed_backends(),
                    credential,
                    allowed_node_ids: vec![],
                    allow_enrolled_nodes: false,
                    secret_file: None,
                    delivery_mode: None,
                }],
                accounting,
                ..Default::default()
            }
            .load()
            .unwrap(),
        ));
        f.state.config = Arc::new(config);
    }
    fn call(source: &OwnerSource, sequence: u64, native: &str, input: u64) -> ObservationRecord {
        let mut body = event(source, sequence, input).body;
        if let ObservationKind::Usage { usage } = &mut body.kind {
            usage.call.native_call_id = text(native);
        }
        ObservationRecord::seal(Uuid::new_v4(), body).unwrap()
    }
    async fn send(f: &Fixture, p: &Packet) -> StatusCode {
        aq_server::build_worker_router(f.state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/worker/observations/batch")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {}", f.credential))
                    .body(Body::from(serde_json::to_vec(p).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
    }
    async fn costs(
        f: &Fixture,
        attempt: Uuid,
    ) -> (Vec<Cost>, aq_lifecycle_api::AccountingCompleteness) {
        let view = observations::inspection::usage(&f.state, f.actor, attempt)
            .await
            .unwrap();
        view.validate().unwrap();
        (view.usage.costs, view.usage.completeness)
    }
    fn estimate(f: &Fixture, revision: u64, amount: u64) -> Cost {
        let c = card(f, revision, 0).card;
        Cost::NonInvoiceEstimate {
            amount_microunits: amount,
            currency: text("USD"),
            rate_card: c.reference,
            basis: EstimateBasis::TokenRates,
            allocation_policy: c.allocation_policy,
        }
    }
    async fn policy_events(f: &Fixture, lineage: Uuid) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM accounting_policy_events WHERE lineage_id=$1")
            .bind(lineage)
            .fetch_one(&f.db.pool().await)
            .await
            .unwrap()
    }

    /// The worker's half of the launch contract, computed without the server's
    /// derivation: the delivered module gives back the catalog entry, and the
    /// source ID is SHA-256 of the sorted-key JSON
    /// `["aq.observation-source/v1", attempt, 1, entry]`, first 16 bytes,
    /// version 8, RFC 4122 variant; the session key is `aq.attempt/<attempt>/1`.
    fn worker_identity(modules: &[ArtifactRef], attempt: Uuid) -> (Uuid, Text) {
        let entry = aq_admission::observation_source::entry(modules)
            .unwrap()
            .expect("the launch delivers the catalog entry");
        let mut value = json!(["aq.observation-source/v1", attempt, 1, entry]);
        value.sort_all_objects();
        let digest = Digest::of(&serde_json::to_vec(&value).unwrap());
        let mut id = [0u8; 16];
        for (i, b) in id.iter_mut().enumerate() {
            *b = u8::from_str_radix(&digest.as_str()[i * 2..i * 2 + 2], 16).unwrap();
        }
        id[6] = (id[6] & 0x0f) | 0x80;
        id[8] = (id[8] & 0x3f) | 0x80;
        (
            Uuid::from_bytes(id),
            text(&format!("aq.attempt/{attempt}/1")),
        )
    }
    fn observed(source: &OwnerSource, sequence: u64, kind: ObservationKind) -> ObservationRecord {
        let mut body = event(source, sequence, 0).body;
        body.kind = kind;
        ObservationRecord::seal(Uuid::new_v4(), body).unwrap()
    }

    #[tokio::test]
    async fn delivered_entry_identity_is_enrolled_and_its_events_are_stall_progress() {
        use aq_server::stall::{
            engine::{Ports, Target},
            evidence::Explanation,
            policy::ExecutionPolicy,
            ports::DbPorts,
        };
        let (mut f, unregistered) = seeded(Some(true)).await;
        let attempt = f.profiled.offer.binding.attempt_id;
        let target = Target {
            attempt_id: attempt,
            session_id: Uuid::new_v4(),
            task_id: f.profiled.offer.manifest.bindings.task_id,
            project_id: f.profiled.offer.manifest.bindings.project_id,
            workspace_id: f.profiled.offer.manifest.bindings.graph_id,
            generation: 1,
        };
        let policy = ExecutionPolicy::v1();
        let evidence = |f: &Fixture| {
            let ports = DbPorts {
                state: f.state.clone(),
            };
            let (target, policy) = (target.clone(), policy.clone());
            async move { ports.evidence(&target, &policy, NOW).await.unwrap() }
        };
        // No catalog source: nothing is delivered. The unenrolled source is
        // refused, its record dropped (the node sequence moves past it), and
        // the ladder has no evidence.
        assert!(f.state.config.intelligence.is_none());
        let turn = ObservationKind::TurnBoundary { in_turn: true };
        let refused = packet(&f, vec![observed(&unregistered, 1, turn.clone())]);
        assert_eq!(send(&f, &refused).await, StatusCode::OK);
        assert_eq!(counts(&f).await, (1, 0));
        let before = evidence(&f).await;
        assert_eq!(before.last_progress_ms, None);
        assert_eq!(
            before.explanation,
            Some(Explanation::ObservationsUnavailable)
        );
        // With a catalog source the launch delivers its entry, and the identity
        // the worker derives from it is the one the batch route enrolls.
        install(
            &mut f,
            aq_selection::CredentialKind::SubscriptionLogin,
            vec![],
        );
        let registry = f.state.config.intelligence.clone().unwrap();
        let choice = &f.selection.choice;
        let entry = (registry.config.accounting)
            .source(&choice.provider, &choice.harness)
            .unwrap()
            .reference
            .clone();
        let (module, _) = aq_admission::observation_source::module(&entry).unwrap();
        let (source_id, session_key) = worker_identity(&[module], attempt);
        let mut worker = catalog::source_for_attempt(&f.state, attempt)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                worker.source_id,
                &worker.native_session_key,
                worker.generation
            ),
            (source_id, &session_key, 1)
        );
        worker.source_id = source_id;
        worker.native_session_key = session_key;
        let tool = ObservationKind::ToolStarted {
            call_id: text("call-1"),
            tool_name: text("Bash"),
        };
        let accepted = packet(
            &f,
            vec![observed(&worker, 2, turn), observed(&worker, 3, tool)],
        );
        assert_eq!(send(&f, &accepted).await, StatusCode::OK);
        assert_eq!(counts(&f).await, (3, 2));
        let after = evidence(&f).await;
        assert!(after.last_progress_ms.is_some() && after.observed_through_ms.is_some());
        assert_ne!(
            after.explanation,
            Some(Explanation::ObservationsUnavailable)
        );
        drop(f);
    }

    #[tokio::test]
    async fn attempt_usage_feeds_ledger_and_budget() {
        let (mut f, _) = seeded(Some(true)).await;
        let cards = vec![card(&f, 1, 3_000_000)];
        install(&mut f, aq_selection::CredentialKind::ApiKey, cards);
        let attempt = f.profiled.offer.binding.attempt_id;
        let s = catalog::source_for_attempt(&f.state, attempt)
            .await
            .unwrap()
            .expect("catalog covers the selected provider and harness");
        assert_eq!(s.billing, Some(EstimateBasis::TokenRates));
        assert_eq!(
            s.account_claim.as_str(),
            f.selection.choice.account_reference
        );
        // The production batch route registers the trusted source before ingest.
        let first = packet(&f, vec![call(&s, 1, "call-1", 1000)]);
        assert_eq!(send(&f, &first).await, StatusCode::OK);
        assert_eq!(counts(&f).await, (1, 1));
        // 1000 input x 3 + 4 output x 15 microunits per token, rounded up once.
        let (got, completeness) = costs(&f, attempt).await;
        assert_eq!(got, vec![estimate(&f, 1, 3060)]);
        assert!(matches!(
            completeness,
            aq_lifecycle_api::AccountingCompleteness::CompleteForPolicy { .. }
        ));
        // An exact replay neither appends nor recounts.
        assert_eq!(send(&f, &first).await, StatusCode::OK);
        assert_eq!(counts(&f).await, (1, 1));
        // A rate-card correction is a new revision; it re-prices, never adds.
        let cards = vec![card(&f, 1, 3_000_000), card(&f, 2, 2_000_000)];
        install(&mut f, aq_selection::CredentialKind::ApiKey, cards);
        assert_eq!(costs(&f, attempt).await.0, vec![estimate(&f, 2, 2060)]);
        let soft = budget::SoftCeiling {
            limit_microunits: 2500,
            currency: "USD".into(),
        };
        let lineage = s.binding.lineage_id;
        let before = budget::lineage(&f.state, lineage, Some(&soft))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(before.spent_microunits, Some(2060));
        assert_eq!(before.currency.as_deref(), Some("USD"));
        assert_eq!(
            before.accounting,
            aq_selection::AccountingCompleteness::Complete
        );
        assert_eq!(policy_events(&f, lineage).await, 0);
        // A retry shares the lineage: history it re-imports is not recounted
        // and its new calls add to, never reset, the lineage spend.
        let previous = s.clone();
        successor(&mut f, &previous).await;
        let next = f.profiled.offer.binding.attempt_id;
        let s2 = catalog::source_for_attempt(&f.state, next)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(s2.source_id, s.source_id);
        let retry = packet(
            &f,
            vec![call(&s2, 2, "call-1", 1000), call(&s2, 3, "call-2", 500)],
        );
        assert_eq!(send(&f, &retry).await, StatusCode::OK);
        assert_eq!(costs(&f, attempt).await.0, vec![estimate(&f, 2, 2060)]);
        assert_eq!(costs(&f, next).await.0, vec![estimate(&f, 2, 1060)]);
        let after = budget::lineage(&f.state, lineage, Some(&soft))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.spent_microunits, Some(3120));
        assert!(after.ledger_revision > before.ledger_revision);
        // The soft ceiling is crossed once and emits exactly one policy event.
        assert!(after.soft_ceiling_reached);
        assert_eq!(policy_events(&f, lineage).await, 1);
        budget::lineage(&f.state, lineage, Some(&soft))
            .await
            .unwrap();
        assert_eq!(policy_events(&f, lineage).await, 1);
        // No hard budget provider exists, so a requested hard ceiling is still
        // refused by selection (hard_cost_enforcement_unqualified, 0004 §2.2).
        assert!(after.snapshot().hard_budget_provider.is_none());
        drop(f);
    }

    // C4 follow-up: the node's accounted spend feeds its B1 spend ceiling.
    #[tokio::test]
    async fn node_spend_ceiling_uses_accounted_node_spend() {
        use aq_selection::NodeCeiling;
        use aq_server::profiled_execution::placement::{self, Ledger};
        let (mut f, _) = seeded(Some(true)).await;
        let cards = vec![card(&f, 1, 3_000_000)];
        install(&mut f, aq_selection::CredentialKind::ApiKey, cards);
        let pool = f.db.pool().await;
        let book = catalog::rate_book(&f.state);
        let node = f.node.node_id;
        let advertise = |currency: &str, micros: u64| {
            json!({"backend": {"kind": "cloudflare-containers", "capabilities": ["vm-isolation"],
                "max_concurrent_sandboxes": 4,
                "spend_ceiling": {"currency": currency, "micros": micros}, "templates": []}})
        };
        sqlx::query(
            "INSERT INTO delegated_node_boots(node_id,boot_id,registration) VALUES($1,$2,$3)",
        )
        .bind(node)
        .bind(Uuid::new_v4())
        .bind(advertise("USD", 3_060))
        .execute(&pool)
        .await
        .unwrap();
        let ceiling = || async {
            let ledger = Ledger {
                pool: &pool,
                book: &book,
            };
            placement::facts_in(&pool, &ledger, None, node, None, "ci@1", NOW)
                .await
                .unwrap()
                .expect("a B1-registered node has placement facts")
                .ceiling
        };
        let readvertise = |currency: &'static str, micros: u64| {
            let pool = pool.clone();
            async move {
                sqlx::query("UPDATE delegated_node_boots SET registration=$2 WHERE node_id=$1")
                    .bind(node)
                    .bind(advertise(currency, micros))
                    .execute(&pool)
                    .await
                    .unwrap();
            }
        };
        // No reported calls yet: zero spent, so the node is under its ceiling.
        assert_eq!(ceiling().await, None);
        let attempt = f.profiled.offer.binding.attempt_id;
        let s = catalog::source_for_attempt(&f.state, attempt)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            send(&f, &packet(&f, vec![call(&s, 1, "call-1", 1000)])).await,
            StatusCode::OK
        );
        let summary = budget::node(&pool, &book, node).await.unwrap();
        assert_eq!(summary.spent_microunits, Some(3060));
        assert!(summary.complete);
        // 3060 spent: at the 3060 ceiling the node refuses with a typed reason.
        assert_eq!(ceiling().await, Some(NodeCeiling::Spend));
        assert_eq!(NodeCeiling::Spend.reason(), "node_spend_ceiling_reached");
        // Under a higher ceiling it receives offers.
        readvertise("USD", 3_061).await;
        assert_eq!(ceiling().await, None);
        // A ceiling in another currency is not comparable: refused, not zero.
        readvertise("EUR", 1_000_000).await;
        assert_eq!(ceiling().await, Some(NodeCeiling::SpendUnaccounted));
        // Another node's spend is not this node's.
        assert_eq!(
            budget::node(&pool, &book, Uuid::new_v4())
                .await
                .unwrap()
                .costs,
            vec![]
        );
        drop(f);
    }

    #[tokio::test]
    async fn flat_subscription_usage_is_labelled_not_costed() {
        let (mut f, _) = seeded(Some(true)).await;
        let cards = vec![card(&f, 1, 3_000_000)];
        install(
            &mut f,
            aq_selection::CredentialKind::SubscriptionLogin,
            cards,
        );
        let attempt = f.profiled.offer.binding.attempt_id;
        let s = catalog::source_for_attempt(&f.state, attempt)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(s.billing, Some(EstimateBasis::SubscriptionAllocation));
        let p = packet(&f, vec![call(&s, 1, "call-1", 1000)]);
        assert_eq!(send(&f, &p).await, StatusCode::OK);
        // Token usage is recorded in full...
        let view = observations::inspection::usage(&f.state, f.actor, attempt)
            .await
            .unwrap();
        assert_eq!(view.canonical_calls, 1);
        assert_eq!(view.known_category_subtotals[0], 1000);
        // ...but no rate card is pinned and nothing is charged at token rates.
        let fact: Value =
            sqlx::query_scalar("SELECT fact FROM observation_call_heads WHERE owner_attempt_id=$1")
                .bind(attempt)
                .fetch_one(&f.db.pool().await)
                .await
                .unwrap();
        assert_eq!(fact["pricing_reference"], Value::Null);
        assert_eq!(
            view.usage.costs,
            vec![Cost::Unknown {
                reason: UnknownReason::NotApplicable
            }]
        );
        assert!(!view.usage.costs.iter().any(|c| matches!(
            c,
            Cost::NonInvoiceEstimate {
                basis: EstimateBasis::TokenRates,
                ..
            }
        )));
        let aq_lifecycle_api::AccountingCompleteness::Incomplete { reason } =
            &view.usage.completeness
        else {
            panic!("subscription cost is labelled, not complete");
        };
        assert!(reason.as_str().contains("subscription"));
        let lineage = budget::lineage(&f.state, s.binding.lineage_id, None)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(
            lineage.accounting,
            aq_selection::AccountingCompleteness::Complete
        );
        drop(f);
    }

    // H2 follow-up: re-enrolling after a credential change bumps the source
    // generation and revokes its predecessor. Late usage from the revoked
    // generation is kept as evidence but never charged, and the lineage budget
    // reads heads by their lineage column.
    #[tokio::test]
    async fn credential_change_bumps_source_generation_and_revokes_previous() {
        use aq_selection::CredentialKind;
        let (mut f, _) = seeded(Some(true)).await;
        let cards = vec![card(&f, 1, 3_000_000)];
        install(&mut f, CredentialKind::SubscriptionLogin, cards.clone());
        let attempt = f.profiled.offer.binding.attempt_id;
        let pool = f.db.pool().await;
        let derive = |f: &Fixture| {
            let state = f.state.clone();
            async move {
                catalog::source_for_attempt(&state, attempt)
                    .await
                    .unwrap()
                    .unwrap()
            }
        };
        let s1 = derive(&f).await;
        assert_eq!(s1.generation, 1);
        assert_eq!(
            send(&f, &packet(&f, vec![call(&s1, 1, "call-1", 1000)])).await,
            StatusCode::OK
        );
        // An unchanged derivation re-enrolls idempotently at the same generation.
        assert!(
            catalog::register_attempt(&f.state, attempt, None)
                .await
                .unwrap()
        );
        assert_eq!(derive(&f).await.source_id, s1.source_id);

        install(&mut f, CredentialKind::ApiKey, cards);
        let s2 = derive(&f).await;
        assert_eq!(s2.generation, 2);
        assert_ne!(s2.source_id, s1.source_id);
        assert_eq!(
            s2.native_session_key.as_str(),
            format!("aq.attempt/{attempt}/2")
        );
        assert_eq!(s2.billing, Some(EstimateBasis::TokenRates));
        assert!(
            catalog::register_attempt(&f.state, attempt, None)
                .await
                .unwrap()
        );
        let revocation: (Uuid, i64) = sqlx::query_as(
            "SELECT superseded_by_source_id,superseded_by_generation \
             FROM observation_source_revocations WHERE source_id=$1 AND generation=1",
        )
        .bind(s1.source_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(revocation, (s2.source_id, 2));
        // The enrolled generation is now current; repeated enrollment is stable.
        assert!(
            catalog::register_attempt(&f.state, attempt, None)
                .await
                .unwrap()
        );
        assert_eq!(derive(&f).await.generation, 2);
        let sources: i64 =
            sqlx::query_scalar("SELECT count(*) FROM observation_sources WHERE attempt_id=$1")
                .bind(attempt)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(sources, 2);
        // A revoked source is never re-admitted, and the history is immutable.
        assert!(matches!(
            observations::register_source(&f.state, &s1).await,
            Err(observations::Error::Refused)
        ));
        assert!(
            sqlx::query("DELETE FROM observation_source_revocations")
                .execute(&pool)
                .await
                .is_err()
        );

        // Late usage from generation 1 keeps the sequence moving but is unresolved.
        assert_eq!(
            send(&f, &packet(&f, vec![call(&s1, 2, "call-2", 500)])).await,
            StatusCode::OK
        );
        assert_eq!(
            send(&f, &packet(&f, vec![call(&s2, 3, "call-3", 100)])).await,
            StatusCode::OK
        );
        assert_eq!(counts(&f).await, (3, 3));
        let decisions: Vec<String> = sqlx::query_scalar(
            "SELECT decision->>'kind' FROM observation_events WHERE attempt_id=$1 ORDER BY sequence",
        )
        .bind(attempt)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(decisions, ["append", "unresolved", "append"]);
        // Only charged calls have heads in the lineage; the token-rate call is
        // priced, the subscription call is labelled, the revoked one is absent.
        let lineage = s2.binding.lineage_id;
        let heads: i64 =
            sqlx::query_scalar("SELECT count(*) FROM observation_call_heads WHERE lineage_id=$1")
                .bind(lineage)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(heads, 2);
        let budget = budget::lineage(&f.state, lineage, None)
            .await
            .unwrap()
            .unwrap();
        // 100 input x 3 + 4 output x 15 microunits per token.
        assert_eq!(budget.spent_microunits, Some(360));
        drop(f);
    }
}

#[tokio::test]
async fn retention_closes_late_packet_window_without_resetting_sequence() {
    let (f, s) = setup().await;
    let first = packet(&f, vec![event(&s, 1, 10)]);
    observations::ingest(&f.state, f.node.node_id, &first)
        .await
        .unwrap();
    let charge: i64 = sqlx::query_scalar(
        "SELECT charged_bytes FROM observation_packet_retention WHERE node_id=$1 AND message_id=$2",
    )
    .bind(f.node.node_id)
    .bind(first.body.message_id)
    .fetch_one(&f.db.pool().await)
    .await
    .unwrap();
    assert!(charge > 0);
    sqlx::query("INSERT INTO retention_closed_attempts(attempt_id,closed_at_ms) VALUES($1,$2)")
        .bind(s.binding.attempt_id)
        .bind(NOW as i64)
        .execute(&f.db.pool().await)
        .await
        .unwrap();
    // A retry of the recorded packet gets its receipt and writes nothing. A
    // late packet's source is refused and its record dropped; the node
    // sequence still moves on, so the node's other sources are not held back.
    observations::ingest(&f.state, f.node.node_id, &first)
        .await
        .unwrap();
    let late = packet(&f, vec![event(&s, 2, 20)]);
    let reply = observations::ingest_reply(&f.state, f.node.node_id, &late)
        .await
        .unwrap();
    assert_eq!(
        reply.refused_sources[0].reason,
        aq_observation_admission::RefusalReason::Binding
    );
    assert_eq!(counts(&f).await, (2, 1));
    assert_eq!(projection(&f, &s).await.known_subtotals[0], 10);
}
