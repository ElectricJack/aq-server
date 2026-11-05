//! Actual mailbox/reservation/session persistence with synthetic lifecycle evidence.
//! No host processes, provider calls, or sleeps.
use aq_central_authority::{owner::*, *};
use aq_server::{
    AppState,
    agent_access::supervisor_conversation::{
        self as conversation, dispatch, reply,
        reservations::{
            Cancel, Configure, LocalPolicy, PolicyKind, Preview, PreviewRequest, PreviewStatus,
            ReservationOwner, ReservationState, Reserve,
        },
    },
    config::Config,
    identity::verifier_from_config,
    store::User,
    test_support::{TestDb, with_store_profile},
};
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use chrono::{TimeZone, Utc};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;

#[derive(Clone)]
struct Evidence;
#[async_trait::async_trait]
impl EvidenceVerifier for Evidence {
    async fn authorize_plan(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        plan: &CentralPlan,
    ) -> OwnerResult<()> {
        let digest: String = sqlx::query_scalar(
            "SELECT input_digest FROM central_supervisor_dispatches WHERE dispatch_id=$1",
        )
        .bind(plan.dispatch_id)
        .fetch_one(&mut **tx)
        .await?;
        let digest = serde_json::from_value(json!(digest)).unwrap();
        dispatch::authorize_dispatch(tx, plan, &digest)
            .await
            .map_err(|_| OwnerError::Conflict)
    }
    async fn verify_event(
        &self,
        _: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        _: &Run,
        _: &Event,
    ) -> OwnerResult<()> {
        Ok(())
    }
}
fn artifact() -> ArtifactRef {
    ArtifactRef {
        id: Text::new("synthetic-evidence").unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(b"synthetic"),
    }
}
fn clock() -> Clock {
    Clock {
        machine_boot_id: Uuid::from_u128(9),
        boot_ms: 1000,
    }
}
fn observation(plan: &CentralPlan) -> HostObservationRef {
    HostObservationRef {
        binding: plan.binding().unwrap(),
        launch_id: Uuid::from_u128(plan.generation as u128 + 100),
        guardian: artifact(),
        observation: artifact(),
    }
}
struct Fixture {
    db: TestDb,
    state: AppState,
    user: User,
    pool: PgPool,
    app: Router,
    actor: Uuid,
    conversation: Uuid,
    /// Milliseconds added to the fixed request clock (agile-vault-27 expiry).
    skew: Arc<std::sync::atomic::AtomicI64>,
}
impl Fixture {
    async fn new(version: u32) -> Self {
        let db = TestDb::create().await;
        let store = with_store_profile(version, db.store()).await;
        let now = Utc.timestamp_millis_opt(1_000_000).unwrap();
        let skew = Arc::new(std::sync::atomic::AtomicI64::new(0));
        let clock_skew = skew.clone();
        let user = store
            .bootstrap_owner("owner@localhost", "dev", "owner@localhost", now)
            .await
            .unwrap()
            .unwrap();
        let config: Config = toml::from_str("format_version=1\nenvironment='development'\n[server]\nadmin_bind='127.0.0.1:0'\nworker_bind='127.0.0.1:0'\npublic_origin='http://127.0.0.1:8080'\n[auth]\nmode='dev'\ndev_email='owner@localhost'\n[bootstrap]\nowner_email='owner@localhost'\nenabled=true\n").unwrap();
        let state = AppState {
            store,
            identity: verifier_from_config(&config.auth, reqwest::Client::new()),
            config: Arc::new(config),
            clock: Arc::new(move || {
                now + chrono::Duration::milliseconds(
                    clock_skew.load(std::sync::atomic::Ordering::SeqCst),
                )
            }),
            agent_access: None,
            source_retention: None,
        };
        let app = conversation::routes(&state)
            .merge(conversation::supervisor_routes(&state))
            .with_state(state.clone());
        let pool = db.pool().await;
        let conversation =
            sqlx::query_scalar("SELECT conversation_id FROM global_supervisor_conversation")
                .fetch_one(&pool)
                .await
                .unwrap();
        let f = Self {
            db,
            state,
            user,
            pool,
            app,
            actor: Uuid::new_v4(),
            conversation,
            skew,
        };
        f.owner()
            .configure_actor(
                &f.user,
                &serde_json::to_vec(&Configure {
                    operation_id: Uuid::new_v4(),
                    actor_id: f.actor,
                    expected_actor_revision: 0,
                    policy: LocalPolicy {
                        schema_version: 1,
                        kind: PolicyKind::GlobalSupervisorReservation,
                        actor_id: f.actor,
                        conversation_id: f.conversation,
                        enabled: true,
                    },
                })
                .unwrap(),
            )
            .await
            .unwrap();
        f
    }
    fn owner(&self) -> ReservationOwner {
        ReservationOwner::new(&self.state)
    }
    fn sessions(&self) -> SessionOwner<Evidence> {
        SessionOwner::new(self.pool.clone(), Evidence)
    }
    async fn post(&self, path: &str, token: Option<&str>, value: Value) -> (StatusCode, Value) {
        let mut request = Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/json");
        request = if let Some(token) = token {
            request.header("authorization", format!("Bearer {token}"))
        } else {
            request
                .header("x-aq-dev-email", "owner@localhost")
                .header("x-aq-request", "1")
        };
        let response = self
            .app
            .clone()
            .oneshot(request.body(Body::from(value.to_string())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }
    async fn human(&self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .header("x-aq-dev-email", "owner@localhost")
            .header("x-aq-request", "1");
        let body = body.map_or_else(Body::empty, |v| Body::from(v.to_string()));
        let response = self
            .app
            .clone()
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }
    fn marker_path(&self) -> String {
        format!("/api/human/conversations/{}/read-marker", self.conversation)
    }
    fn history_path(&self) -> String {
        format!("/api/human/conversations/{}/messages", self.conversation)
    }
    async fn send(&self) -> Uuid {
        let id = Uuid::new_v4();
        let (status, result) = self.post(&format!("/api/human/conversations/{}/messages", self.conversation), None, json!({"operation_id":Uuid::new_v4(),"message_id":id,"expected_conversation_revision":"1","body":"next owner message","context":[],"attachments":[]})).await;
        assert_eq!(status, StatusCode::OK, "{result}");
        id
    }
    async fn preview(&self) -> Preview {
        let revision = self
            .owner()
            .inspect_actor(&self.user, self.actor)
            .await
            .unwrap()
            .revision;
        self.owner()
            .preview_batch(
                &self.user,
                PreviewRequest {
                    actor_id: self.actor,
                    expected_actor_revision: revision,
                    scan_after: 0,
                    through_cursor: None,
                },
            )
            .await
            .unwrap()
    }
    async fn reserve(&self) -> conversation::reservations::Receipt {
        let p = self.preview().await;
        assert_eq!(p.status, PreviewStatus::Ready);
        self.owner()
            .reserve_batch(
                &self.user,
                &serde_json::to_vec(&Reserve {
                    operation_id: Uuid::new_v4(),
                    reservation_id: Uuid::new_v4(),
                    actor_id: self.actor,
                    expected_actor_revision: p.actor_revision,
                    expected_policy_revision: p.policy_revision,
                    scan_after: p.scan_after,
                    through_cursor: p.through_cursor,
                    selection_digest: p.selection_digest,
                })
                .unwrap(),
            )
            .await
            .unwrap()
    }
    async fn plan(&self, reservation: Uuid, generation: u64) -> CentralPlan {
        let examples: Value = serde_json::from_str(include_str!(
            "../../aq-central-authority/artifacts/examples.json"
        ))
        .unwrap();
        let Document::Plan(mut p) =
            decode(&serde_json::to_vec(&examples["plan"]).unwrap()).unwrap()
        else {
            panic!()
        };
        let CentralSubject::CentralConversation {
            actor_id,
            conversation_id,
            ..
        } = &mut p.subject;
        *actor_id = self.actor;
        *conversation_id = self.conversation;
        p.generation = generation;
        let SessionIdentity::Central {
            session_id,
            generation: g,
            ..
        } = &mut p.session
        else {
            panic!()
        };
        *session_id = Uuid::new_v4();
        *g = generation;
        let turn = dispatch::prepare_turn(
            &self.pool,
            self.actor,
            reservation,
            generation,
            *session_id,
            1_000_000,
        )
        .await
        .unwrap();
        p.batch = turn.batch;
        p.dispatch_id = turn.dispatch_id;
        p.host_operation_id = turn.host_operation_id;
        p
    }
    async fn start(&self, p: &CentralPlan) -> Snapshot {
        self.sessions().prepare(p.clone(), clock()).await.unwrap();
        let receipt = self
            .sessions()
            .start(
                &p.binding().unwrap(),
                HostLaunchRef {
                    binding: p.binding().unwrap(),
                    launch_id: observation(p).launch_id,
                    intent: artifact(),
                },
                100_000,
                clock(),
            )
            .await
            .unwrap();
        assert_eq!(receipt.decision, Decision::Applied);
        receipt.snapshot
    }
    async fn report(&self, p: &CentralPlan, event: Event) -> Snapshot {
        let receipt = self
            .sessions()
            .report(&p.binding().unwrap(), event, clock())
            .await
            .unwrap();
        assert!(
            !matches!(receipt.decision, Decision::Refused { .. }),
            "{:?}",
            receipt.decision
        );
        receipt.snapshot
    }
    async fn assert_consumed(&self, reservation: Uuid) {
        assert!(
            self.owner()
                .inspect_actor(&self.user, self.actor)
                .await
                .unwrap()
                .current_reservation
                .is_none()
        );
        assert_eq!(
            self.owner()
                .history(&self.user, self.actor, None, 16)
                .await
                .unwrap()
                .items
                .iter()
                .find(|r| r.id == reservation)
                .unwrap()
                .state,
            ReservationState::Consumed
        );
    }
    async fn answer(&self, p: &CentralPlan) -> (String, Value) {
        let token = reply::issue_credential(&self.pool, self.actor, 60_000, 999_000)
            .await
            .unwrap();
        let request = json!({"operation_id":Uuid::new_v4(),"message_id":Uuid::new_v4(),"dispatch_id":p.dispatch_id,"body":"supervisor answer"});
        let (status, receipt) = self
            .post(
                "/v1/supervisor/conversation/replies",
                Some(&token),
                request.clone(),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{receipt}");
        (token, request)
    }
}

#[tokio::test]
async fn reply_consumes_reservation_and_allows_successor_turn() {
    let f = Fixture::new(aq_server::STATE_FORMAT_VERSION).await;
    let first = f.send().await;
    let receipt = f.reserve().await;
    let reservation = receipt.reservation.as_ref().unwrap();
    let p = f.plan(reservation.id, 1).await;
    let s = f.start(&p).await;
    f.report(
        &p,
        Event::Handoff {
            evidence: HandoffEvidence {
                association: s.run.association().unwrap().clone(),
                disposition: HandoffDisposition::HandedOff,
                receipt: artifact(),
            },
        },
    )
    .await;
    let (token, request) = f.answer(&p).await;
    f.assert_consumed(reservation.id).await;
    let lookup = f
        .owner()
        .lookup_operation(&f.user, f.actor, receipt.operation_id)
        .await
        .unwrap();
    assert_eq!(
        lookup.receipt, receipt,
        "the original reserve receipt remains unchanged"
    );
    assert_eq!(
        lookup.current_reservation.unwrap().state,
        ReservationState::Consumed
    );
    let second = f.send().await;
    let next_receipt = f.reserve().await;
    let next = next_receipt.reservation.unwrap();
    assert_eq!(next.batch.messages.len(), 1);
    assert_eq!(next.batch.messages[0].message_id, second);
    assert_ne!(first, second);
    // Replaying an old reply cannot clear the newer reservation.
    let (status, _) = f
        .post(
            "/v1/supervisor/conversation/replies",
            Some(&token),
            request.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        f.owner()
            .inspect_actor(&f.user, f.actor)
            .await
            .unwrap()
            .current_reservation
            .unwrap()
            .id,
        next.id
    );
    // One physical generation remains one immutable turn.
    assert!(
        dispatch::prepare_turn(
            &f.pool,
            f.actor,
            next.id,
            1,
            dispatch_session(&p),
            1_000_000
        )
        .await
        .is_err()
    );
    let p2 = f.plan(next.id, 2).await;
    assert!(
        f.sessions().prepare(p2.clone(), clock()).await.is_err(),
        "successor still needs stopped writers"
    );
    f.report(
        &p,
        Event::Stopped {
            evidence: StopEvidenceRef {
                host: observation(&p),
                stopped_writers: artifact(),
            },
        },
    )
    .await;
    f.start(&p2).await;
    assert_eq!(
        f.post("/v1/supervisor/conversation/replies", Some(&token), request)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    f.answer(&p2).await;
    f.assert_consumed(next.id).await;
    assert!(
        f.preview().await.batch.is_none(),
        "completed mail is never reselected"
    );
    // Reopening validates consumed proofs and retains both generations.
    f.db.store().await;
    assert!(f.sessions().generation(f.actor, 1).await.unwrap().is_some());
    assert!(
        dispatch::prepare_turn(
            &f.pool,
            f.actor,
            reservation.id,
            2,
            dispatch_session(&p2),
            1_000_000
        )
        .await
        .is_err()
    );
}
fn dispatch_session(p: &CentralPlan) -> Uuid {
    match p.session {
        SessionIdentity::Central { session_id, .. } => session_id,
        _ => panic!(),
    }
}

#[tokio::test]
async fn terminal_not_delivered_consumes_but_unknown_and_running_hold_mail() {
    let f = Fixture::new(aq_server::STATE_FORMAT_VERSION).await;
    let first = f.send().await;
    let receipt = f.reserve().await;
    let r = receipt.reservation.unwrap();
    let p = f.plan(r.id, 1).await;
    let s = f.start(&p).await;
    f.report(
        &p,
        Event::Running {
            observation: observation(&p),
        },
    )
    .await;
    assert_eq!(
        f.preview().await.status,
        PreviewStatus::ReservationOutstanding
    );
    let association = s.run.association().unwrap().clone();
    f.report(
        &p,
        Event::ReportUnknown {
            association: association.clone(),
        },
    )
    .await;
    f.send().await;
    assert_eq!(
        f.preview().await.status,
        PreviewStatus::ReservationOutstanding
    );
    // The old pre-plan cancellation route cannot release an uncertain dispatch.
    assert!(
        f.owner()
            .cancel_reservation(
                &f.user,
                &serde_json::to_vec(&Cancel {
                    operation_id: Uuid::new_v4(),
                    actor_id: f.actor,
                    reservation_id: r.id,
                    expected_reservation_revision: 1
                })
                .unwrap()
            )
            .await
            .is_err()
    );
    assert!(sqlx::query("UPDATE central_supervisor_reservations SET state='consumed',revision=2 WHERE reservation_id=$1").bind(r.id).execute(&f.pool).await.is_err(), "unknown is not terminal consumption evidence");
    f.db.store().await;
    assert_eq!(
        f.preview().await.status,
        PreviewStatus::ReservationOutstanding
    );
    let event = Event::ReconcileHandoff {
        evidence: HandoffEvidence {
            association,
            disposition: HandoffDisposition::NotHandedOff,
            receipt: artifact(),
        },
    };
    f.report(&p, event.clone()).await;
    f.assert_consumed(r.id).await;
    // Repeated terminal reports are idempotent; terminal failure is not an automatic retry.
    f.report(&p, event).await;
    let next = f.reserve().await.reservation.unwrap();
    assert!(next.batch.messages.iter().all(|m| m.message_id != first));
    f.db.store().await;
}

#[tokio::test]
async fn unknown_reply_releases_reservation_without_resending_unknown_input() {
    let f = Fixture::new(aq_server::STATE_FORMAT_VERSION).await;
    let first = f.send().await;
    let r = f.reserve().await.reservation.unwrap();
    let p = f.plan(r.id, 1).await;
    let s = f.start(&p).await;
    f.report(
        &p,
        Event::ReportUnknown {
            association: s.run.association().unwrap().clone(),
        },
    )
    .await;
    f.answer(&p).await;
    f.assert_consumed(r.id).await;
    let retained = f.sessions().inspect(f.actor).await.unwrap().unwrap();
    assert!(matches!(
        retained.run.dispatch(),
        DispatchState::Unknown { .. }
    ));
    let second = f.send().await;
    let next = f.reserve().await.reservation.unwrap();
    assert_eq!(
        next.batch
            .messages
            .iter()
            .map(|m| m.message_id)
            .collect::<Vec<_>>(),
        vec![second]
    );
    assert_ne!(first, second);
    let p2 = f.plan(next.id, 2).await;
    f.report(
        &p,
        Event::Stopped {
            evidence: StopEvidenceRef {
                host: observation(&p),
                stopped_writers: artifact(),
            },
        },
    )
    .await;
    f.start(&p2).await;
    assert!(matches!(
        f.sessions()
            .generation(f.actor, 1)
            .await
            .unwrap()
            .unwrap()
            .run
            .dispatch(),
        DispatchState::Unknown { .. }
    ));
}

#[tokio::test]
async fn upgrade_consumes_already_recorded_reply_and_keeps_receipt() {
    let f = Fixture::new(48).await;
    f.send().await;
    let receipt = f.reserve().await;
    let r = receipt.reservation.as_ref().unwrap();
    let p = f.plan(r.id, 1).await;
    f.start(&p).await;
    f.answer(&p).await;
    assert!(
        f.owner()
            .inspect_actor(&f.user, f.actor)
            .await
            .unwrap()
            .current_reservation
            .is_some()
    );
    f.db.store().await;
    f.assert_consumed(r.id).await;
    assert_eq!(
        f.owner()
            .lookup_operation(&f.user, f.actor, receipt.operation_id)
            .await
            .unwrap()
            .receipt,
        receipt
    );
    let second = f.send().await;
    assert_eq!(
        f.reserve().await.reservation.unwrap().batch.messages[0].message_id,
        second
    );
}

#[tokio::test]
async fn upgrade_consumes_known_failure_and_retains_unknown_delivery() {
    for unknown in [false, true] {
        let f = Fixture::new(48).await;
        f.send().await;
        let r = f.reserve().await.reservation.unwrap();
        let p = f.plan(r.id, 1).await;
        let s = f.start(&p).await;
        let association = s.run.association().unwrap().clone();
        let event = if unknown {
            Event::ReportUnknown { association }
        } else {
            Event::Handoff {
                evidence: HandoffEvidence {
                    association,
                    disposition: HandoffDisposition::NotHandedOff,
                    receipt: artifact(),
                },
            }
        };
        f.report(&p, event).await;
        f.db.store().await;
        if unknown {
            assert_eq!(
                f.preview().await.status,
                PreviewStatus::ReservationOutstanding
            );
        } else {
            f.assert_consumed(r.id).await;
            assert_eq!(f.preview().await.status, PreviewStatus::NoEligibleMail);
        }
    }
}

fn conversation_schema(v: &Value) {
    let s: Value = serde_json::from_str(include_str!(
        "../../../protocol/human-supervisor-conversation/v1/schema.json"
    ))
    .unwrap();
    assert!(aq_agent_api::check(&s, &s, v), "public schema {v}");
}

#[test]
fn attachment_fixtures_match_schema_and_published_digest() {
    use sha2::Digest as _;
    let schema = include_bytes!("../../../protocol/human-supervisor-conversation/v1/schema.json");
    let pinned = include_str!("../../../protocol/human-supervisor-conversation/v1/schema.sha256");
    assert_eq!(
        format!("sha256:{}", hex::encode(sha2::Sha256::digest(schema))),
        pinned.trim(),
        "update schema.sha256 and README when the schema changes"
    );
    let history: Value = serde_json::from_str(include_str!(
        "../../../protocol/human-supervisor-conversation/v1/history-attachments.example.json"
    ))
    .unwrap();
    conversation_schema(&history);
    let draft = &history["items"][1]["attachments"][0];
    assert_eq!(
        draft["payload_digest"],
        format!(
            "sha256:{}",
            aq_agent_api::digest(&draft["payload"]).unwrap()
        )
    );
    let s: Value = serde_json::from_slice(schema).unwrap();
    let request: Value = serde_json::from_str(include_str!(
        "../../../protocol/human-supervisor-conversation/v1/reply-attachments.example.json"
    ))
    .unwrap();
    for a in request["attachments"].as_array().unwrap() {
        assert!(aq_agent_api::check(
            &s,
            &s["$defs"]["ReplyAttachmentRequest"],
            a
        ));
    }
    // Additive: a pre-attachment supervisor item remains valid.
    let mut old = history.clone();
    old["items"][1]
        .as_object_mut()
        .unwrap()
        .remove("attachments");
    conversation_schema(&old);
    old["items"][1]["attachments"] = json!([{"type": "unknown"}]);
    assert!(!aq_agent_api::check(&s, &s, &old));
}

#[tokio::test]
async fn reply_attachments_serve_typed_drafts_and_session_receipts_through_history() {
    let f = Fixture::new(aq_server::STATE_FORMAT_VERSION).await;
    f.send().await;
    let receipt = f.reserve().await;
    let reservation = receipt.reservation.as_ref().unwrap();
    let p = f.plan(reservation.id, 1).await;
    let s = f.start(&p).await;
    f.report(
        &p,
        Event::Handoff {
            evidence: HandoffEvidence {
                association: s.run.association().unwrap().clone(),
                disposition: HandoffDisposition::HandedOff,
                receipt: artifact(),
            },
        },
    )
    .await;
    let session = dispatch_session(&p);
    let project = Uuid::new_v4();
    let (task_a, task_b, edge, change_set) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    let journal = |principal: Uuid, disposition: &'static str| {
        let operation = Uuid::new_v4();
        let request_digest = format!("sha256:{}", "ab".repeat(32));
        let response = json!({"operation_id":operation,"disposition":disposition,"request_digest":request_digest,"scope":{},"created_at":"2026-01-01T00:00:00.000Z","updated_at":"2026-01-01T00:00:00.000Z",
            "subject_refs":[{"type":"project","id":project,"revision":"3"}],
            "receipt_refs":[{"type":"graph_change_set","id":change_set}],
            "affected_entities":[{"type":"task","id":task_a,"revision":"1"},{"type":"task","id":task_b,"revision":"1"},{"type":"graph_edge","id":edge,"revision":"1"}],
            "recovery":"lookup_original","current_versions":{}});
        let pool = f.pool.clone();
        async move {
            sqlx::query("INSERT INTO dashboard_operations(operation_id,principal_id,kind,scope,request_digest,disposition,response,created_at,updated_at) VALUES($1,$2,'task.graph.create','{}',$3,$4,$5,now(),now())")
                .bind(operation).bind(principal).bind(&request_digest).bind(disposition).bind(response)
                .execute(&pool).await.unwrap();
            (operation, request_digest)
        }
    };
    let (confirmed, request_digest) = journal(session, "confirmed").await;
    let (pending, _) = journal(session, "pending").await;
    let (owners, _) = journal(f.user.id, "confirmed").await;
    let draft = Uuid::new_v4();
    let payload = json!({"operation_id":Uuid::new_v4(),"project_id":project,"expected_version":"4",
        "tasks":[{"id":Uuid::new_v4(),"title":"Draft task","priority":10}],"edges":[]});
    let token = reply::issue_credential(&f.pool, f.actor, 60_000, 999_000)
        .await
        .unwrap();
    let message = Uuid::new_v4();
    let request = |attachments: Value| json!({"operation_id":Uuid::new_v4(),"message_id":message,"dispatch_id":p.dispatch_id,"body":"supervisor answer","attachments":attachments});
    let path = "/v1/supervisor/conversation/replies";
    let draft_item = json!({"type":"proposed_task_draft","draft_id":draft,"payload":payload});
    // Refusals precede any write and leave the turn answerable.
    let mut bad_payload = payload.clone();
    bad_payload["tasks"][0]["title"] = json!(" ");
    for (attachments, status) in [
        (
            json!([{"type":"action_receipt","operation_id":owners}]),
            StatusCode::CONFLICT,
        ),
        (
            json!([{"type":"action_receipt","operation_id":pending}]),
            StatusCode::CONFLICT,
        ),
        (
            json!([{"type":"action_receipt","operation_id":Uuid::new_v4()}]),
            StatusCode::CONFLICT,
        ),
        (
            json!([{"type":"proposed_task_draft","draft_id":draft,"payload":bad_payload}]),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!([draft_item.clone(), draft_item.clone()]),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!([{"type":"proposed_task_draft","draft_id":Uuid::nil(),"payload":payload}]),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!([{"type":"action_receipt","operation_id":confirmed,"extra":1}]),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!(
                (0..5)
                    .map(|_| json!({"type":"action_receipt","operation_id":Uuid::new_v4()}))
                    .collect::<Vec<_>>()
            ),
            StatusCode::BAD_REQUEST,
        ),
    ] {
        let (got, body) = f
            .post(path, Some(&token), request(attachments.clone()))
            .await;
        assert_eq!(got, status, "{attachments} -> {body}");
    }
    let accepted = request(json!([draft_item, {"type":"action_receipt","operation_id":confirmed}]));
    let (status, first) = f.post(path, Some(&token), accepted.clone()).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let (status, replayed) = f.post(path, Some(&token), accepted).await;
    assert_eq!((status, &replayed), (StatusCode::OK, &first));
    f.assert_consumed(reservation.id).await;

    let response = f
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/human/conversations/{}/messages",
                    f.conversation
                ))
                .header("x-aq-dev-email", "owner@localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let history: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    conversation_schema(&history);
    let items = history["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{history}");
    assert!(
        items[0].get("attachments").is_none(),
        "owner items are unchanged"
    );
    let reply = &items[1];
    assert_eq!(reply["message_id"], json!(message));
    // The draft is stored in its normalized form (defaults materialized), and
    // its digest covers exactly the served payload.
    let mut normalized = payload.clone();
    normalized["tasks"][0].as_object_mut().unwrap().extend([
        ("body".into(), json!("")),
        ("acceptance_criteria".into(), json!("")),
        ("labels".into(), json!([])),
        ("task_state".into(), json!("active")),
    ]);
    assert_eq!(
        reply["attachments"],
        json!([
            {"type":"proposed_task_draft","message_id":message,"draft_id":draft,"project_id":project,"revision":"4",
             "payload_digest":format!("sha256:{}",aq_agent_api::digest(&normalized).unwrap()),"payload":normalized},
            {"type":"action_receipt","message_id":message,"operation_id":confirmed,"kind":"task.graph.create","project_id":project,"project_revision":"3",
             "created_task_ids":[task_a,task_b],"created_edge_ids":[edge],
             "receipt_refs":[{"type":"graph_change_set","id":change_set}],"request_digest":request_digest}
        ])
    );
}

#[tokio::test]
async fn runtime_reports_the_live_central_binding() {
    let f = Fixture::new(aq_server::STATE_FORMAT_VERSION).await;
    let unavailable =
        json!({"state":"unavailable","binding":null,"reason":"central_runtime_not_connected"});
    let (status, discovery) = f
        .human("GET", "/api/human/conversations/global", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{discovery}");
    assert_eq!(discovery["runtime"], unavailable);
    assert_eq!(discovery["limits"]["context_refs"], json!(16));
    assert_eq!(discovery["limits"]["attachments"], json!(4));
    conversation_schema(&discovery);
    f.send().await;
    let reservation = f.reserve().await.reservation.unwrap();
    let p = f.plan(reservation.id, 1).await;
    let s = f.start(&p).await;
    let available = json!({"state":"available","binding":{"actor_id":f.actor,"generation":1,"session_id":dispatch_session(&p)},"reason":"central_session_active"});
    let (_, discovery) = f
        .human("GET", "/api/human/conversations/global", None)
        .await;
    assert_eq!(discovery["runtime"], available);
    conversation_schema(&discovery);
    let (status, history) = f.human("GET", &f.history_path(), None).await;
    assert_eq!(status, StatusCode::OK, "{history}");
    assert_eq!(history["runtime"], available);
    conversation_schema(&history);
    f.report(
        &p,
        Event::Handoff {
            evidence: HandoffEvidence {
                association: s.run.association().unwrap().clone(),
                disposition: HandoffDisposition::HandedOff,
                receipt: artifact(),
            },
        },
    )
    .await;
    f.report(
        &p,
        Event::Stopped {
            evidence: StopEvidenceRef {
                host: observation(&p),
                stopped_writers: artifact(),
            },
        },
    )
    .await;
    let (_, history) = f.human("GET", &f.history_path(), None).await;
    assert_eq!(
        history["runtime"], unavailable,
        "a stopped session is not live"
    );
}

#[tokio::test]
async fn context_refs_are_validated_echoed_and_rendered_into_the_turn() {
    let f = Fixture::new(aq_server::STATE_FORMAT_VERSION).await;
    let (task, project) = (Uuid::new_v4(), Uuid::new_v4());
    let create = |context: Value| json!({"operation_id":Uuid::new_v4(),"message_id":Uuid::new_v4(),"expected_conversation_revision":"1","body":"look at this","context":context,"attachments":[]});
    let many: Vec<Value> = (0..17)
        .map(|_| json!({"type":"task","id":Uuid::new_v4(),"revision":null}))
        .collect();
    for bad in [
        json!([{"type":"task","id":task,"revision":"1"},{"type":"task","id":task,"revision":"2"}]),
        json!([{"type":"task","id":Uuid::nil(),"revision":null}]),
        json!([{"type":"task","id":task,"revision":"0"}]),
        json!([{"type":"task","id":task}]),
        json!([{"type":"widget","id":task,"revision":null}]),
        json!([{"type":"task","id":task,"revision":null,"extra":1}]),
        json!(many),
    ] {
        let (status, body) = f.post(&f.history_path(), None, create(bad.clone())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad} -> {body}");
    }
    let context = json!([{"type":"task","id":task,"revision":"3"},{"type":"project","id":project,"revision":null}]);
    let request = create(context.clone());
    let (status, receipt) = f.post(&f.history_path(), None, request.clone()).await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    let (status, history) = f.human("GET", &f.history_path(), None).await;
    assert_eq!(status, StatusCode::OK, "{history}");
    conversation_schema(&history);
    let items = history["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{history}");
    assert_eq!(items[0]["message_id"], request["message_id"]);
    assert_eq!(items[0]["context"], context);
    let plain = f.send().await;
    let (_, history) = f.human("GET", &f.history_path(), None).await;
    let item = history["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["message_id"] == json!(plain))
        .unwrap()
        .clone();
    assert!(item.get("context").is_none(), "refless items are unchanged");
    let reservation = f.reserve().await.reservation.unwrap();
    let turn = dispatch::prepare_turn(
        &f.pool,
        f.actor,
        reservation.id,
        1,
        Uuid::new_v4(),
        1_000_000,
    )
    .await
    .unwrap();
    let input = String::from_utf8(turn.input).unwrap();
    assert!(
        input.contains(&format!(
            "look at this\n[context task {task} revision 3]\n[context project {project}]\n"
        )),
        "{input}"
    );
    assert!(input.contains("next owner message\n"), "{input}");
}

#[tokio::test]
async fn read_marker_is_per_principal_monotonic_and_bounded_by_latest() {
    let f = Fixture::new(aq_server::STATE_FORMAT_VERSION).await;
    let path = f.marker_path();
    let (status, marker) = f.human("GET", &path, None).await;
    assert_eq!(status, StatusCode::OK, "{marker}");
    conversation_schema(&marker);
    assert_eq!(marker["principal_id"], json!(f.user.id));
    assert_eq!(marker["read_cursor"], Value::Null);
    assert_eq!(marker["latest_cursor"], Value::Null);
    assert_eq!(marker["unread"], json!({"count":0,"capped":false}));
    let put = |cursor: Value| f.human("PUT", &path, Some(json!({"cursor":cursor})));
    assert_eq!(
        put(json!("1")).await.0,
        StatusCode::BAD_REQUEST,
        "nothing to read yet"
    );

    f.send().await;
    let reservation = f.reserve().await.reservation.unwrap();
    let p = f.plan(reservation.id, 1).await;
    let s = f.start(&p).await;
    f.report(
        &p,
        Event::Handoff {
            evidence: HandoffEvidence {
                association: s.run.association().unwrap().clone(),
                disposition: HandoffDisposition::HandedOff,
                receipt: artifact(),
            },
        },
    )
    .await;
    f.answer(&p).await;
    let (_, history) = f.human("GET", &f.history_path(), None).await;
    let items = history["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{history}");
    let (owner, reply) = (items[0]["cursor"].clone(), items[1]["cursor"].clone());

    let (_, marker) = f.human("GET", &path, None).await;
    conversation_schema(&marker);
    assert_eq!(marker["latest_cursor"], reply);
    assert_eq!(
        marker["unread"],
        json!({"count":1,"capped":false}),
        "the owner's own message is never unread"
    );
    let (status, marker) = put(owner.clone()).await;
    assert_eq!(status, StatusCode::OK, "{marker}");
    assert_eq!(
        (&marker["read_cursor"], marker["unread"]["count"].as_u64()),
        (&owner, Some(1))
    );
    let (status, marker) = put(reply.clone()).await;
    assert_eq!(status, StatusCode::OK, "{marker}");
    conversation_schema(&marker);
    assert_eq!(
        (&marker["read_cursor"], marker["unread"]["count"].as_u64()),
        (&reply, Some(0))
    );
    let (status, marker) = put(owner.clone()).await;
    assert_eq!(status, StatusCode::OK, "{marker}");
    assert_eq!(
        marker["read_cursor"], reply,
        "the marker never moves backward"
    );
    let (anchor, r) = reply.as_str().unwrap().split_once('.').unwrap();
    let r: u64 = r.parse().unwrap();
    for bad in [
        json!({"cursor":format!("{anchor}.{}", r + 1)}),
        json!({"cursor":(anchor.parse::<u64>().unwrap() + 1).to_string()}),
        json!({"cursor":"x"}),
        json!({"cursor":"0"}),
        json!({"cursor":owner,"extra":1}),
    ] {
        assert_eq!(
            f.human("PUT", &path, Some(bad.clone())).await.0,
            StatusCode::BAD_REQUEST,
            "{bad}"
        );
    }
    // Markers are keyed by principal: another principal's row is invisible here.
    let other: Uuid = sqlx::query_scalar("INSERT INTO users(id,email,role,created_at) SELECT gen_random_uuid(),'viewer@localhost','viewer',created_at FROM users WHERE id=$1 RETURNING id")
        .bind(f.user.id)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO global_supervisor_read_markers(principal_id,conversation_id,anchor,reply,updated_at_ms) VALUES($1,$2,1,0,1)")
        .bind(other)
        .bind(f.conversation)
        .execute(&f.pool)
        .await
        .unwrap();
    let (_, marker) = f.human("GET", &path, None).await;
    assert_eq!(marker["read_cursor"], reply);
}

/// agile-vault-27: one raw request (owner dev headers unless a bearer is given).
async fn raw(
    f: &Fixture,
    method: &str,
    path: &str,
    content_type: Option<&str>,
    token: Option<&str>,
    body: Vec<u8>,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut request = Request::builder().method(method).uri(path);
    if let Some(t) = content_type {
        request = request.header("content-type", t);
    }
    request = match token {
        Some(token) => request.header("authorization", format!("Bearer {token}")),
        None => request
            .header("x-aq-dev-email", "owner@localhost")
            .header("x-aq-request", "1"),
    };
    let response = f
        .app
        .clone()
        .oneshot(request.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let (status, headers) = (response.status(), response.headers().clone());
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, bytes.to_vec())
}
fn attachment_path(f: &Fixture, id: Uuid) -> String {
    format!(
        "/api/human/conversations/{}/attachments/{id}",
        f.conversation
    )
}
async fn upload(
    f: &Fixture,
    id: Uuid,
    name: &str,
    media: &str,
    bytes: &[u8],
) -> (StatusCode, Value) {
    let name: String = name.bytes().map(|b| format!("%{b:02X}")).collect();
    let (status, _, body) = raw(
        f,
        "PUT",
        &format!("{}?name={name}", attachment_path(f, id)),
        Some(media),
        None,
        bytes.to_vec(),
    )
    .await;
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}
fn sha(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    format!("sha256:{}", hex::encode(sha2::Sha256::digest(bytes)))
}

#[tokio::test]
async fn owner_attachments_are_stored_bound_echoed_rendered_and_served() {
    let f = Fixture::new(aq_server::STATE_FORMAT_VERSION).await;
    let (status, discovery) = f
        .human("GET", "/api/human/conversations/global", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(discovery["limits"]["attachments"], json!(4));
    conversation_schema(&discovery);
    let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    let (notes, image) = (Uuid::new_v4(), Uuid::new_v4());
    for (id, name, media, bytes, want) in [
        (
            notes,
            "a.html",
            "text/html",
            b"<b>".to_vec(),
            StatusCode::BAD_REQUEST,
        ),
        (
            notes,
            "a.txt",
            "text/plain; charset=utf-8",
            b"x".to_vec(),
            StatusCode::BAD_REQUEST,
        ),
        (
            notes,
            "a.png",
            "image/png",
            b"not a png".to_vec(),
            StatusCode::BAD_REQUEST,
        ),
        (
            notes,
            "a.txt",
            "text/plain",
            b"a\0b".to_vec(),
            StatusCode::BAD_REQUEST,
        ),
        (
            notes,
            "a.json",
            "application/json",
            b"{".to_vec(),
            StatusCode::BAD_REQUEST,
        ),
        (
            notes,
            "a.txt",
            "text/plain",
            vec![],
            StatusCode::BAD_REQUEST,
        ),
        (
            notes,
            "a/b.txt",
            "text/plain",
            b"x".to_vec(),
            StatusCode::BAD_REQUEST,
        ),
        (
            notes,
            "..",
            "text/plain",
            b"x".to_vec(),
            StatusCode::BAD_REQUEST,
        ),
        (
            Uuid::nil(),
            "a.txt",
            "text/plain",
            b"x".to_vec(),
            StatusCode::BAD_REQUEST,
        ),
        (
            notes,
            "big.txt",
            "text/plain",
            vec![b'x'; (1 << 20) + 1],
            StatusCode::PAYLOAD_TOO_LARGE,
        ),
    ] {
        let (status, body) = upload(&f, id, name, media, &bytes).await;
        assert_eq!(status, want, "{name} {media} -> {body}");
    }
    let text = "# plan\n\"quoted\" ]\n".as_bytes();
    let (status, staged) = upload(&f, notes, "plan \"v2\"].md", "text/markdown", text).await;
    assert_eq!(status, StatusCode::CREATED, "{staged}");
    conversation_schema(&staged);
    assert_eq!(staged["state"], "staged");
    assert_eq!(staged["digest"], sha(text));
    assert_eq!(staged["expires_at_ms"], json!(1_000_000 + 86_400_000));
    let (status, again) = upload(&f, notes, "plan \"v2\"].md", "text/markdown", text).await;
    assert_eq!(
        (status, &again),
        (StatusCode::OK, &staged),
        "identical retry"
    );
    let (status, _) = upload(&f, notes, "other.md", "text/markdown", text).await;
    assert_eq!(status, StatusCode::CONFLICT, "id reuse with other metadata");
    let (status, _) = upload(&f, image, "shot.png", "image/png", &png).await;
    assert_eq!(status, StatusCode::CREATED);
    let r = |id: Uuid, digest: &str| json!({"id":id,"digest":digest});
    let create = |attachments: Value| json!({"operation_id":Uuid::new_v4(),"message_id":Uuid::new_v4(),"expected_conversation_revision":"1","body":"see files","context":[],"attachments":attachments});
    for bad in [
        json!([r(notes, &sha(text)), r(notes, &sha(text))]),
        json!([r(Uuid::nil(), &sha(text))]),
        json!([r(notes, "sha256:XYZ")]),
        json!([{"id":notes}]),
        json!(
            (0..5)
                .map(|_| r(Uuid::new_v4(), &sha(text)))
                .collect::<Vec<_>>()
        ),
    ] {
        let (status, body) = f.post(&f.history_path(), None, create(bad.clone())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad} -> {body}");
    }
    for refused in [
        json!([r(notes, &sha(b"other"))]),
        json!([r(Uuid::new_v4(), &sha(text))]),
    ] {
        let (status, receipt) = f.post(&f.history_path(), None, create(refused)).await;
        assert_eq!(status, StatusCode::CONFLICT, "{receipt}");
        assert_eq!(receipt["reason"], "attachment_unavailable");
        conversation_schema(&receipt);
    }
    let refs = json!([r(notes, &sha(text)), r(image, &sha(&png))]);
    let request = create(refs.clone());
    let (status, receipt) = f.post(&f.history_path(), None, request.clone()).await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    let (status, bound) = upload(&f, notes, "plan \"v2\"].md", "text/markdown", text).await;
    assert_eq!(status, StatusCode::OK);
    conversation_schema(&bound);
    assert_eq!(bound["state"], "attached");
    assert_eq!(bound["message_id"], request["message_id"]);
    let (status, receipt) = f
        .post(
            &f.history_path(),
            None,
            create(json!([r(image, &sha(&png))])),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "an attached file binds once");
    assert_eq!(receipt["reason"], "attachment_unavailable");
    let (_, history) = f.human("GET", &f.history_path(), None).await;
    conversation_schema(&history);
    let items = history["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{history}");
    assert_eq!(
        items[0]["attachments"],
        json!([
            {"id":notes,"name":"plan \"v2\"].md","media_type":"text/markdown","size_bytes":text.len(),"digest":sha(text)},
            {"id":image,"name":"shot.png","media_type":"image/png","size_bytes":png.len(),"digest":sha(&png)},
        ])
    );
    let plain = f.send().await;
    let (_, history) = f.human("GET", &f.history_path(), None).await;
    let item = history["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["message_id"] == json!(plain))
        .unwrap()
        .clone();
    assert!(
        item.get("attachments").is_none(),
        "fileless items are unchanged"
    );
    let (status, headers, bytes) =
        raw(&f, "GET", &attachment_path(&f, notes), None, None, vec![]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, text);
    assert_eq!(headers["content-type"], "text/markdown");
    assert_eq!(headers["x-content-type-options"], "nosniff");
    assert_eq!(headers["x-aq-content-digest"], sha(text).as_str());
    assert_eq!(
        headers["content-disposition"],
        "attachment; filename*=UTF-8''plan%20%22v2%22%5D.md"
    );
    // The supervisor may read only files whose message was dispatched to it.
    let reservation = f.reserve().await.reservation.unwrap();
    let p = f.plan(reservation.id, 1).await;
    let input: Vec<u8> = sqlx::query_scalar(
        "SELECT input_bytes FROM central_supervisor_dispatches WHERE dispatch_id=$1",
    )
    .bind(p.dispatch_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    let input = String::from_utf8(input).unwrap();
    assert!(
        input.contains(&format!(
            "see files\n[attachment {notes} name \"plan \\\"v2\\\"].md\" text/markdown {} bytes {}]\n[attachment {image} name \"shot.png\" image/png {} bytes {}]",
            text.len(),
            sha(text),
            png.len(),
            sha(&png)
        )),
        "{input}"
    );
    f.start(&p).await;
    let token = reply::issue_credential(&f.pool, f.actor, 60_000, 999_000)
        .await
        .unwrap();
    let supervisor = |id: Uuid| format!("/v1/supervisor/conversation/attachments/{id}");
    let (status, _, bytes) = raw(&f, "GET", &supervisor(image), None, Some(&token), vec![]).await;
    assert_eq!((status, bytes), (StatusCode::OK, png.clone()));
    let staged = Uuid::new_v4();
    upload(&f, staged, "later.txt", "text/plain", b"later").await;
    let (status, _, _) = raw(&f, "GET", &supervisor(staged), None, Some(&token), vec![]).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "undispatched files stay private"
    );
    let (status, _, _) = raw(
        &f,
        "GET",
        &supervisor(image),
        None,
        Some("aqsv1_bogus"),
        vec![],
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn staged_attachments_expire_and_are_bounded_per_conversation() {
    let f = Fixture::new(aq_server::STATE_FORMAT_VERSION).await;
    let old = Uuid::new_v4();
    let (status, _) = upload(&f, old, "old.txt", "text/plain", b"old").await;
    assert_eq!(status, StatusCode::CREATED);
    f.skew
        .store(86_400_000, std::sync::atomic::Ordering::SeqCst);
    let request = json!({"operation_id":Uuid::new_v4(),"message_id":Uuid::new_v4(),"expected_conversation_revision":"1","body":"late","context":[],"attachments":[{"id":old,"digest":sha(b"old")}]});
    let (status, receipt) = f.post(&f.history_path(), None, request).await;
    assert_eq!(status, StatusCode::CONFLICT, "{receipt}");
    assert_eq!(receipt["reason"], "attachment_unavailable");
    let (status, _, _) = raw(&f, "GET", &attachment_path(&f, old), None, None, vec![]).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "expired staged files are gone"
    );
    for i in 0..16 {
        let (status, body) =
            upload(&f, Uuid::new_v4(), &format!("{i}.txt"), "text/plain", b"x").await;
        assert_eq!(status, StatusCode::CREATED, "{i}: {body}");
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM global_supervisor_attachments")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(count, 16, "the expired row was purged on upload");
    let (status, body) = upload(&f, Uuid::new_v4(), "17.txt", "text/plain", b"x").await;
    assert!(!status.is_success(), "staged quota: {status} {body}");
}
