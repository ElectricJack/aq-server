//! Actual mailbox/reservation/session persistence with synthetic lifecycle evidence.
//! No host processes, provider calls, or sleeps.
use aq_central_authority::{owner::*, *};
use aq_server::{
    AppState,
    agent_access::supervisor_conversation::{
        self as conversation, dispatch, reply,
        reservations::{
            Cancel, Configure, Disposition, LocalPolicy, PolicyKind, Preview, PreviewRequest,
            PreviewStatus, ReservationOwner, ReservationState, Reserve,
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

#[cfg(not(feature = "supervisor_full_access"))]
mod admin;
mod owner;

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
        Self::configured(version, "").await
    }
    /// `extra` is appended to the vault `config.toml` (a trailing table).
    async fn configured(version: u32, extra: &str) -> Self {
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
        let config: Config = toml::from_str(&("format_version=1\nenvironment='development'\n[server]\nadmin_bind='127.0.0.1:0'\nworker_bind='127.0.0.1:0'\npublic_origin='http://127.0.0.1:8080'\n[auth]\nmode='dev'\ndev_email='owner@localhost'\n[bootstrap]\nowner_email='owner@localhost'\nenabled=true\n".to_owned() + extra)).unwrap();
        let state = AppState {
            internal_work: Default::default(),
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
    /// Makes the supervisor the one the binding trigger created for `project`.
    async fn scope_to(&mut self, project: Uuid) {
        self.conversation = sqlx::query_scalar(
            "SELECT conversation_id FROM supervisor_conversation_ids WHERE project_id=$1",
        )
        .bind(project)
        .fetch_one(&self.pool)
        .await
        .unwrap();
        self.actor =
            sqlx::query_scalar("SELECT actor_id FROM supervisor_controls WHERE conversation_id=$1")
                .bind(self.conversation)
                .fetch_one(&self.pool)
                .await
                .unwrap();
        sqlx::query(
        "UPDATE supervisor_controls SET enabled=true,profile='{}',approved_by=$2 WHERE actor_id=$1",
    )
    .bind(self.actor)
    .bind(self.user.id)
    .execute(&self.pool)
    .await
    .unwrap();
        self.owner()
            .configure_actor(
                &self.user,
                &serde_json::to_vec(&Configure {
                    operation_id: Uuid::new_v4(),
                    actor_id: self.actor,
                    expected_actor_revision: 0,
                    policy: LocalPolicy {
                        schema_version: 1,
                        kind: PolicyKind::GlobalSupervisorReservation,
                        actor_id: self.actor,
                        conversation_id: self.conversation,
                        enabled: true,
                    },
                })
                .unwrap(),
            )
            .await
            .unwrap();
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
            "../../../aq-central-authority/artifacts/examples.json"
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
                PermitWindow::new(100_000, clock()),
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

/// Hands the plan's input to the agent, then stops it before any reply (an operator stop).
async fn stop_after_handoff(f: &Fixture, p: &CentralPlan) {
    let s = f.start(p).await;
    f.report(
        p,
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
        p,
        Event::Stopped {
            evidence: StopEvidenceRef {
                host: observation(p),
                stopped_writers: artifact(),
            },
        },
    )
    .await;
}

#[tokio::test]
async fn stopped_turn_without_reply_holds_mail_until_released_and_then_retries_it() {
    let f = Fixture::new(aq_server::STATE_FORMAT_VERSION).await;
    let first = f.send().await;
    let r = f.reserve().await.reservation.unwrap();
    let p = f.plan(r.id, 1).await;
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
    let release = |id| Cancel {
        operation_id: Uuid::new_v4(),
        actor_id: f.actor,
        reservation_id: id,
        expected_reservation_revision: 1,
    };
    let bytes = serde_json::to_vec(&release(r.id)).unwrap();
    assert!(
        f.owner().cancel_reservation(&f.user, &bytes).await.is_err(),
        "a running turn keeps its reservation"
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
    assert_eq!(
        f.preview().await.status,
        PreviewStatus::ReservationOutstanding,
        "a turn that stopped without a reply holds its mail"
    );
    let bytes = serde_json::to_vec(&release(r.id)).unwrap();
    let released = f.owner().cancel_reservation(&f.user, &bytes).await.unwrap();
    assert_eq!(released.disposition, Disposition::CancelledBeforePlan);
    let next = f.reserve().await.reservation.unwrap();
    let ids: Vec<_> = next.batch.messages.iter().map(|m| m.message_id).collect();
    assert_eq!(ids, vec![first], "released mail is reserved again");
    f.db.store().await;
}

#[tokio::test]
async fn upgrade_consumes_a_stopped_handed_off_turn_stranded_before_136() {
    let f = Fixture::new(135).await;
    f.send().await;
    let r = f.reserve().await.reservation.unwrap();
    let p = f.plan(r.id, 1).await;
    stop_after_handoff(&f, &p).await;
    assert_eq!(
        f.preview().await.status,
        PreviewStatus::ReservationOutstanding,
        "format 135 strands the reservation"
    );
    f.db.store().await;
    f.assert_consumed(r.id).await;
    assert_eq!(f.preview().await.status, PreviewStatus::NoEligibleMail);
}

fn conversation_schema(v: &Value) {
    let s: Value = serde_json::from_str(include_str!(
        "../../../../protocol/human-supervisor-conversation/v1/schema.json"
    ))
    .unwrap();
    assert!(aq_agent_api::check(&s, &s, v), "public schema {v}");
}

#[test]
fn attachment_fixtures_match_schema_and_published_digest() {
    use sha2::Digest as _;
    let schema =
        include_bytes!("../../../../protocol/human-supervisor-conversation/v1/schema.json");
    let pinned =
        include_str!("../../../../protocol/human-supervisor-conversation/v1/schema.sha256");
    assert_eq!(
        format!("sha256:{}", hex::encode(sha2::Sha256::digest(schema))),
        pinned.trim(),
        "update schema.sha256 and README when the schema changes"
    );
    let history: Value = serde_json::from_str(include_str!(
        "../../../../protocol/human-supervisor-conversation/v1/history-attachments.example.json"
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
        "../../../../protocol/human-supervisor-conversation/v1/reply-attachments.example.json"
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
    // Owner upload contract: discovery, upload state and structured refusals.
    let example = |raw: &str| -> Value { serde_json::from_str(raw).unwrap() };
    let discovery = example(include_str!(
        "../../../../protocol/human-supervisor-conversation/v1/discovery.example.json"
    ));
    conversation_schema(&discovery);
    assert_eq!(
        discovery["attachments"],
        serde_json::to_value(aq_supervisor_control::attachments::capability(
            &Default::default()
        ))
        .unwrap(),
        "the published example advertises the enforced limits"
    );
    let mut legacy = discovery.clone();
    legacy.as_object_mut().unwrap().remove("attachments");
    conversation_schema(&legacy);
    for raw in [
        include_str!(
            "../../../../protocol/human-supervisor-conversation/v1/attachment-upload.example.json"
        ),
        include_str!(
            "../../../../protocol/human-supervisor-conversation/v1/attachment-error.example.json"
        ),
    ] {
        conversation_schema(&example(raw));
    }
    let types = &s["$defs"]["AttachmentMediaType"]["enum"];
    assert_eq!(
        types,
        &json!(aq_supervisor_control::attachments::MEDIA_TYPES),
        "schema media types match the server"
    );
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
    assert_eq!(
        discovery["attachments"],
        serde_json::to_value(aq_supervisor_control::attachments::capability(
            &Default::default()
        ))
        .unwrap()
    );
    assert_eq!(discovery["attachments"]["max_file_bytes"], json!(1 << 20));
    assert_eq!(discovery["attachments"]["max_files_per_message"], json!(4));
    conversation_schema(&discovery);
    let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    let (notes, image) = (Uuid::new_v4(), Uuid::new_v4());
    for (id, name, media, bytes, want) in [
        (
            notes,
            "a.html",
            "text/html",
            b"<b>".to_vec(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ),
        (
            notes,
            "a.txt",
            "text/plain; charset=utf-8",
            b"x".to_vec(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
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
        if !id.is_nil() {
            conversation_schema(&body);
            assert!(body["error"].as_str().unwrap().starts_with("attachment_"));
        }
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
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "staged quota: {body}"
    );
    assert_eq!(body["error"], "attachment_capacity_exhausted");
}

/// A minimal stored ZIP with the named empty entries (built at runtime).
fn zip(names: &[&str]) -> Vec<u8> {
    let (mut out, mut directory) = (Vec::new(), Vec::new());
    for name in names {
        let offset = out.len() as u32;
        out.extend(b"PK\x03\x04\x14\0");
        out.extend([0; 20]);
        out.extend((name.len() as u16).to_le_bytes());
        out.extend([0, 0]);
        out.extend(name.as_bytes());
        directory.extend(b"PK\x01\x02\x14\0\x14\0");
        directory.extend([0; 20]);
        directory.extend((name.len() as u16).to_le_bytes());
        directory.extend([0; 12]);
        directory.extend(offset.to_le_bytes());
        directory.extend(name.as_bytes());
    }
    let start = out.len() as u32;
    out.extend(&directory);
    out.extend(b"PK\x05\x06\0\0\0\0");
    out.extend((names.len() as u16).to_le_bytes());
    out.extend((names.len() as u16).to_le_bytes());
    out.extend((directory.len() as u32).to_le_bytes());
    out.extend(start.to_le_bytes());
    out.extend([0, 0]);
    out
}
const DOCX: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
/// One request as another current owner (`second@localhost`).
async fn as_second(f: &Fixture, method: &str, path: &str) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("x-aq-dev-email", "second@localhost")
        .header("x-aq-request", "1")
        .body(Body::empty())
        .unwrap();
    let response = f.app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}
fn refusal(status: StatusCode, body: &Value) -> (StatusCode, &str) {
    conversation_schema(body);
    (status, body["error"].as_str().unwrap_or(""))
}

#[tokio::test]
async fn owner_attachment_contract_supports_documents_metadata_unstage_and_denial() {
    let mut f = Fixture::new(aq_server::STATE_FORMAT_VERSION).await;
    sqlx::query("INSERT INTO users(id,email,role,issuer,subject,created_at) VALUES($1,'second@localhost','owner','dev','second@localhost',now())")
        .bind(Uuid::new_v4())
        .execute(&f.pool)
        .await
        .unwrap();
    let docx = zip(&["[Content_Types].xml", "_rels/.rels", "word/document.xml"]);
    let pdf = b"%PDF-1.7\n1 0 obj << >> endobj\ntrailer << >>\n%%EOF\n".to_vec();
    let markdown = "# Plan\n\n- r\u{e9}sum\u{e9}\n".as_bytes().to_vec();
    let files = [
        (Uuid::new_v4(), "design notes.docx", DOCX, docx.clone()),
        (Uuid::new_v4(), "spec.pdf", "application/pdf", pdf),
        (Uuid::new_v4(), "plan.md", "text/markdown", markdown),
        (
            Uuid::new_v4(),
            "notes.txt",
            "text/plain",
            b"plain notes\n".to_vec(),
        ),
    ];
    let meta = |f: &Fixture, id: Uuid| format!("{}/metadata", attachment_path(f, id));
    for (id, name, media, bytes) in &files {
        let (status, staged) = upload(&f, *id, name, media, bytes).await;
        assert_eq!(status, StatusCode::CREATED, "{name}: {staged}");
        conversation_schema(&staged);
        assert_eq!(
            (&staged["media_type"], &staged["digest"]),
            (&json!(media), &json!(sha(bytes)))
        );
        let (status, read) = f.human("GET", &meta(&f, *id), None).await;
        assert_eq!((status, &read), (StatusCode::OK, &staged), "metadata");
        let (status, headers, served) =
            raw(&f, "GET", &attachment_path(&f, *id), None, None, vec![]).await;
        assert_eq!((status, &served), (StatusCode::OK, bytes));
        assert_eq!(headers["content-type"], *media);
    }
    // Unsupported, unreadable and oversized inputs carry structured codes.
    let id = Uuid::new_v4();
    let put = format!("{}?name=a.txt", attachment_path(&f, id));
    let (status, _, body) = raw(&f, "PUT", &put, None, None, b"x".to_vec()).await;
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        refusal(status, &body),
        (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "attachment_media_type_unsupported"
        )
    );
    let (unsupported, unreadable, large) = (
        (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "attachment_media_type_unsupported",
        ),
        (StatusCode::BAD_REQUEST, "attachment_unreadable"),
        (StatusCode::PAYLOAD_TOO_LARGE, "attachment_too_large"),
    );
    for (media, bytes, want) in [
        (
            "application/msword",
            b"\xd0\xcf\x11\xe0".to_vec(),
            unsupported,
        ),
        (
            DOCX,
            zip(&["[Content_Types].xml", "xl/workbook.xml"]),
            unreadable,
        ),
        (DOCX, docx[..docx.len() - 1].to_vec(), unreadable),
        ("application/pdf", b"%PDF-1.7\n1 0 obj".to_vec(), unreadable),
        ("text/markdown", b"\xc3\x28".to_vec(), unreadable),
        ("text/plain", vec![b'x'; (1 << 20) + 1], large),
    ] {
        let (status, body) = upload(&f, id, "file", media, &bytes).await;
        assert_eq!(refusal(status, &body), want, "{media}: {body}");
    }
    let (status, body) = upload(&f, id, "", "text/plain", b"x").await;
    assert_eq!(
        refusal(status, &body),
        (StatusCode::BAD_REQUEST, "attachment_name_invalid")
    );
    let (status, body) = upload(&f, files[3].0, "notes.txt", "text/plain", b"changed").await;
    assert_eq!(
        refusal(status, &body),
        (StatusCode::CONFLICT, "attachment_id_conflict")
    );
    let not_found = (StatusCode::NOT_FOUND, "attachment_not_found");
    // Another owner sees none of these staged files and cannot unstage them.
    for (id, ..) in &files {
        for (method, path) in [
            ("GET", meta(&f, *id)),
            ("GET", attachment_path(&f, *id)),
            ("DELETE", attachment_path(&f, *id)),
        ] {
            let (status, body) = as_second(&f, method, &path).await;
            assert_eq!(refusal(status, &body), not_found, "{method} {path}");
        }
    }
    // Unstage: 204, then gone everywhere; a stale ref is refused; the id is reusable.
    let gone = files[3].0;
    let (status, _, body) = raw(&f, "DELETE", &attachment_path(&f, gone), None, None, vec![]).await;
    assert_eq!((status, body.len()), (StatusCode::NO_CONTENT, 0));
    let (status, body) = f.human("DELETE", &attachment_path(&f, gone), None).await;
    assert_eq!(refusal(status, &body), not_found, "retried unstage");
    assert_eq!(
        f.human("GET", &meta(&f, gone), None).await.0,
        StatusCode::NOT_FOUND
    );
    let r = |id: Uuid, bytes: &[u8]| json!({"id":id,"digest":sha(bytes)});
    let create = |refs: Value| json!({"operation_id":Uuid::new_v4(),"message_id":Uuid::new_v4(),"expected_conversation_revision":"1","body":"documents","context":[],"attachments":refs});
    let stale = create(json!([r(files[0].0, &files[0].3), r(gone, &files[3].3)]));
    let (status, receipt) = f.post(&f.history_path(), None, stale).await;
    assert_eq!(status, StatusCode::CONFLICT, "stale ref: {receipt}");
    assert_eq!(receipt["reason"], "attachment_unavailable");
    let (_, unbound) = f.human("GET", &meta(&f, files[0].0), None).await;
    assert_eq!(unbound["state"], "staged", "nothing was bound");
    let (status, _) = upload(&f, gone, "notes.txt", "text/plain", b"plain notes\n").await;
    assert_eq!(status, StatusCode::CREATED, "a deleted id can be reused");
    // Bind three; bound files cannot be unstaged and any current owner reads them.
    let refs: Vec<Value> = files[..3].iter().map(|(id, _, _, b)| r(*id, b)).collect();
    let request = create(json!(refs));
    let (status, receipt) = f.post(&f.history_path(), None, request.clone()).await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    let (status, body) = f
        .human("DELETE", &attachment_path(&f, files[0].0), None)
        .await;
    assert_eq!(
        refusal(status, &body),
        (StatusCode::CONFLICT, "attachment_bound")
    );
    let (status, bound) = as_second(&f, "GET", &meta(&f, files[0].0)).await;
    assert_eq!(status, StatusCode::OK, "{bound}");
    conversation_schema(&bound);
    assert_eq!(bound["state"], "attached");
    assert_eq!(bound["message_id"], request["message_id"]);
    assert_eq!(bound["expires_at_ms"], Value::Null);
    // Persistence: a fresh router (as after a restart), past the staged TTL.
    f.skew
        .store(2 * 86_400_000, std::sync::atomic::Ordering::SeqCst);
    f.app = conversation::routes(&f.state).with_state(f.state.clone());
    for (id, _, media, bytes) in &files[..3] {
        let (status, headers, served) =
            raw(&f, "GET", &attachment_path(&f, *id), None, None, vec![]).await;
        assert_eq!((status, &served), (StatusCode::OK, bytes), "{media}");
        assert_eq!(headers["x-aq-content-digest"], sha(bytes).as_str());
        let (_, read) = f.human("GET", &meta(&f, *id), None).await;
        assert_eq!(read["state"], "attached");
    }
    let (status, body) = f.human("GET", &meta(&f, gone), None).await;
    assert_eq!(refusal(status, &body), not_found, "the staged file expired");
    // Another conversation neither reads, unstages nor binds these files.
    let project = Uuid::new_v4();
    sqlx::query("INSERT INTO project_execution_bindings(project_id,revision,binding,validated_at_ms,binding_observation) VALUES($1,1,'{\"name\":\"Other\",\"enabled\":true}',1,'{}')").bind(project).execute(&f.pool).await.unwrap();
    let home = f.conversation;
    f.conversation = sqlx::query_scalar(
        "SELECT conversation_id FROM supervisor_conversation_ids WHERE project_id=$1",
    )
    .bind(project)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    let discovery = format!("/api/human/conversations/projects/{project}");
    let (status, discovery) = f.human("GET", &discovery, None).await;
    assert_eq!(status, StatusCode::OK, "{discovery}");
    assert_eq!(discovery["attachments"]["media_types"][5], DOCX);
    let fresh = Uuid::new_v4();
    let (status, _) = upload(&f, fresh, "here.txt", "text/plain", b"here").await;
    assert_eq!(status, StatusCode::CREATED);
    for (method, path) in [
        ("GET", meta(&f, files[0].0)),
        ("GET", attachment_path(&f, files[0].0)),
        ("DELETE", attachment_path(&f, files[0].0)),
    ] {
        let (status, body) = f.human(method, &path, None).await;
        assert_eq!(refusal(status, &body), not_found, "{method} {path}");
    }
    f.conversation = home;
    let (status, body) = f.human("GET", &meta(&f, fresh), None).await;
    assert_eq!(refusal(status, &body), not_found);
    let (status, receipt) = f
        .post(&f.history_path(), None, create(json!([r(fresh, b"here")])))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{receipt}");
    assert_eq!(receipt["reason"], "attachment_unavailable");
}

#[tokio::test]
async fn project_supervisor_mail_history_and_reads_remain_scoped_and_stop_revokes_tools() {
    let mut f = Fixture::new(aq_server::STATE_FORMAT_VERSION).await;
    let project = Uuid::new_v4();
    sqlx::query("INSERT INTO project_execution_bindings(project_id,revision,binding,validated_at_ms,binding_observation) VALUES($1,1,'{\"name\":\"Scoped test\",\"enabled\":true}',1,'{}')").bind(project).execute(&f.pool).await.unwrap();
    f.scope_to(project).await;
    f.send().await;
    let r = f.reserve().await.reservation.unwrap();
    let p = f.plan(r.id, 1).await;
    f.start(&p).await;
    let token = reply::issue_credential(&f.pool, f.actor, 60000, 999000)
        .await
        .unwrap();
    #[cfg(not(feature = "supervisor_full_access"))]
    let machine = aq_server::build_worker_router(f.state.clone());
    #[cfg(feature = "supervisor_full_access")]
    let machine = aq_server::build_worker_router_with_owner_app(
        f.state.clone(),
        aq_server::build_admin_router(f.state.clone()),
    );
    let read = |path: String| {
        let app = machine.clone();
        let token = token.clone();
        async move {
            let response = app
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .header("authorization", format!("Bearer {token}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = response.status();
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            (status, serde_json::from_slice::<Value>(&bytes).unwrap())
        }
    };
    // Another project: narrowed away.
    let other = Uuid::new_v4();
    sqlx::query("INSERT INTO project_execution_bindings(project_id,revision,binding,validated_at_ms,binding_observation) VALUES($1,1,'{\"name\":\"Other\",\"enabled\":true}',1,'{}')").bind(other).execute(&f.pool).await.unwrap();
    let (status, projects) = read("/v1/supervisor/projects".into()).await;
    assert_eq!(status, StatusCode::OK, "{projects}");
    let ids: Vec<&str> = projects["projects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap())
        .collect();
    let (project, other) = (project.to_string(), other.to_string());
    assert_eq!(ids, [project.as_str()]);
    assert!(!ids.contains(&other.as_str()));
    // Project task reads use this nest only in the limited-access build.
    #[cfg(not(feature = "supervisor_full_access"))]
    {
        let cross = format!(
            "/v1/supervisor/read/api/human/projects/{}/tasks",
            Uuid::new_v4()
        );
        assert_eq!(read(cross).await.0, StatusCode::FORBIDDEN);
    }
    let (id, digest) = (Uuid::new_v4(), "0".repeat(64));
    for path in [
        format!("/v1/supervisor/read/api/human/results/{id}/revisions/{digest}"),
        format!("/v1/supervisor/read/api/v1/results/{id}/revisions/{digest}"),
        format!("/v1/supervisor/read/api/human/results?project_id={other}"),
        format!("/v1/supervisor/read/api/execution/attempts/{id}"),
    ] {
        let (status, body) = read(path.clone()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}: {body}");
        assert_eq!(body["error"], "supervisor_refused", "{path}");
    }
    let (status, body) = read("/v1/supervisor/read/api/human/nope".into()).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"], "supervisor_refused");
    // With it, the owner bridge refuses another project.
    #[cfg(feature = "supervisor_full_access")]
    {
        let cross = format!("/v1/supervisor/owner/api/human/conversations/projects/{other}");
        let (status, out) = read(cross).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{out}");
    }
    f.answer(&p).await;
    f.assert_consumed(r.id).await;
    f.send().await;
    let next = f.reserve().await.reservation.unwrap();
    let p2 = f.plan(next.id, 2).await;
    let input: Vec<u8> = sqlx::query_scalar(
        "SELECT input_bytes FROM central_supervisor_dispatches WHERE dispatch_id=$1",
    )
    .bind(p2.dispatch_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    let input = String::from_utf8(input).unwrap();
    assert!(input.contains("Prior conversation context"));
    assert!(input.contains("supervisor answer"));
    sqlx::query("UPDATE supervisor_controls SET enabled=false WHERE actor_id=$1")
        .bind(f.actor)
        .execute(&f.pool)
        .await
        .unwrap();
    assert_eq!(
        read("/v1/supervisor/projects".into()).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        f.human("GET", &f.history_path(), None).await.0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn configured_attachment_limits_are_advertised_and_enforced() {
    let f = Fixture::configured(
        aq_server::STATE_FORMAT_VERSION,
        "[supervisor_attachments]\nmax_file_bytes = 1024\nmax_files_per_message = 2\n",
    )
    .await;
    let (status, discovery) = f
        .human("GET", "/api/human/conversations/global", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    conversation_schema(&discovery);
    assert_eq!(discovery["limits"]["attachments"], json!(2));
    assert_eq!(discovery["attachments"]["max_file_bytes"], json!(1024));
    assert_eq!(discovery["attachments"]["max_files_per_message"], json!(2));
    // Everything else stays at its published value.
    let mut defaults = serde_json::to_value(aq_supervisor_control::attachments::capability(
        &Default::default(),
    ))
    .unwrap();
    defaults["max_file_bytes"] = json!(1024);
    defaults["max_files_per_message"] = json!(2);
    assert_eq!(discovery["attachments"], defaults);
    // The byte limit is inclusive and the refusal is the structured 413.
    let (exact, over) = (Uuid::new_v4(), Uuid::new_v4());
    let (status, body) = upload(&f, exact, "exact.txt", "text/plain", &[b'x'; 1024]).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = upload(&f, over, "over.txt", "text/plain", &[b'x'; 1025]).await;
    assert_eq!(
        refusal(status, &body),
        (StatusCode::PAYLOAD_TOO_LARGE, "attachment_too_large")
    );
    let (second, third) = (Uuid::new_v4(), Uuid::new_v4());
    for id in [second, third] {
        let (status, body) = upload(&f, id, "a.txt", "text/plain", b"abc").await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    let r = |id: Uuid, bytes: &[u8]| json!({"id":id,"digest":sha(bytes)});
    let create = |refs: Value| json!({"operation_id":Uuid::new_v4(),"message_id":Uuid::new_v4(),"expected_conversation_revision":"1","body":"files","context":[],"attachments":refs});
    // Three refs exceed the configured count: refused, nothing bound.
    let three = create(json!([
        r(exact, &[b'x'; 1024]),
        r(second, b"abc"),
        r(third, b"abc")
    ]));
    let (status, body) = f.post(&f.history_path(), None, three).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (_, staged) = f
        .human(
            "GET",
            &format!("{}/metadata", attachment_path(&f, exact)),
            None,
        )
        .await;
    assert_eq!(staged["state"], "staged");
    let two = create(json!([r(exact, &[b'x'; 1024]), r(second, b"abc")]));
    let (status, receipt) = f.post(&f.history_path(), None, two).await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
}

#[tokio::test]
async fn attachment_limits_default_to_one_mebibyte_and_four_files() {
    let f = Fixture::new(aq_server::STATE_FORMAT_VERSION).await;
    let (_, discovery) = f
        .human("GET", "/api/human/conversations/global", None)
        .await;
    assert_eq!(discovery["limits"]["attachments"], json!(4));
    assert_eq!(discovery["attachments"]["max_file_bytes"], json!(1_048_576));
    let id = Uuid::new_v4();
    let (status, body) = upload(&f, id, "max.txt", "text/plain", &vec![b'x'; 1 << 20]).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

#[test]
fn attachment_limits_in_the_vault_config_reject_out_of_range_values() {
    let base = "format_version=1\nenvironment='development'\n[server]\nadmin_bind='127.0.0.1:0'\nworker_bind='127.0.0.1:0'\npublic_origin='http://127.0.0.1:8080'\n[auth]\nmode='dev'\ndev_email='owner@localhost'\n[bootstrap]\nowner_email='owner@localhost'\nenabled=true\n";
    let vault = std::env::temp_dir().join(format!("aq-attachment-limits-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&vault).unwrap();
    for (extra, key) in [
        ("max_file_bytes = 1048577", "max_file_bytes"),
        ("max_file_bytes = 0", "max_file_bytes"),
        ("max_files_per_message = 5", "max_files_per_message"),
        ("max_files_per_message = 0", "max_files_per_message"),
    ] {
        std::fs::write(
            vault.join("config.toml"),
            format!("{base}[supervisor_attachments]\n{extra}\n"),
        )
        .unwrap();
        let error = aq_server::config::load(&vault).unwrap_err().to_string();
        assert!(error.contains("supervisor_attachments"), "{extra}: {error}");
        assert!(error.contains(key), "{extra}: {error}");
    }
    std::fs::write(
        vault.join("config.toml"),
        format!("{base}[supervisor_attachments]\nmax_files = 2\n"),
    )
    .unwrap();
    assert!(aq_server::config::load(&vault).is_err(), "unknown key");
    std::fs::write(
        vault.join("config.toml"),
        format!("{base}[supervisor_attachments]\nmax_file_bytes = 4096\n"),
    )
    .unwrap();
    let lowered = aq_server::config::load(&vault)
        .unwrap()
        .supervisor_attachments;
    assert_eq!(
        (lowered.max_file_bytes, lowered.max_files_per_message),
        (4096, 4)
    );
    std::fs::write(vault.join("config.toml"), base).unwrap();
    let loaded = aq_server::config::load(&vault).unwrap();
    assert_eq!(loaded.supervisor_attachments, Default::default());
    std::fs::remove_dir_all(&vault).unwrap();
}

/// LSB-first bit writer for a hand-built deflate stream.
struct Bits {
    out: Vec<u8>,
    acc: u64,
    n: u32,
}
impl Bits {
    fn put(&mut self, v: u64, c: u32) {
        self.acc |= v << self.n;
        self.n += c;
        while self.n >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.n -= 8;
        }
    }
    /// A Huffman code, most significant bit first.
    fn code(&mut self, v: u64, c: u32) {
        self.put((0..c).fold(0, |r, i| (r << 1) | ((v >> i) & 1)), c);
    }
}
/// A zlib stream (one fixed-Huffman block) that inflates to `prefix`,
/// `1 + 258 * k` bytes of `a`, then `suffix`: a small file with large text.
fn expanding(prefix: &[u8], k: usize, suffix: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let mut b = Bits {
        out: vec![0x78, 0x01],
        acc: 0,
        n: 0,
    };
    b.put(0b011, 3); // final block, fixed Huffman codes
    let literal = |b: &mut Bits, x: u8| {
        assert!(x < 144);
        b.code(0x30 + x as u64, 8);
    };
    prefix.iter().chain(b"a").for_each(|&x| literal(&mut b, x));
    for _ in 0..k {
        b.code(0xC5, 8); // length 258
        b.code(0, 5); // distance 1
    }
    suffix.iter().for_each(|&x| literal(&mut b, x));
    b.code(0, 7); // end of block
    let pad = (8 - b.n % 8) % 8;
    b.put(0, pad);
    let data = [prefix, &vec![b'a'; 1 + 258 * k], suffix].concat();
    let (mut s1, mut s2) = (1u32, 0u32);
    for &x in &data {
        s1 = (s1 + x as u32) % 65521;
        s2 = (s2 + s1) % 65521;
    }
    b.out.extend(((s2 << 16) | s1).to_be_bytes());
    (b.out, data)
}
/// A one-page PDF with font `/F1` = `font` and one content stream.
fn one_page_pdf(font: &str, content: &[u8], flate: bool) -> Vec<u8> {
    let mut out = b"%PDF-1.4\n".to_vec();
    for (n, dict) in [
        (1, "/Type /Catalog /Pages 2 0 R"),
        (
            2,
            "/Type /Pages /Count 1 /Kids [4 0 R] /Resources << /Font << /F1 3 0 R >> >>",
        ),
        (3, font),
        (4, "/Type /Page /Parent 2 0 R /Contents 5 0 R"),
    ] {
        out.extend(format!("{n} 0 obj\n<< {dict} >>\nendobj\n").bytes());
    }
    let filter = if flate { "/Filter /FlateDecode " } else { "" };
    out.extend(format!("5 0 obj\n<< {filter}/Length {} >>\nstream\n", content.len()).bytes());
    out.extend(content);
    out.extend(b"\nendstream\nendobj\ntrailer\n<< /Root 1 0 R >>\nstartxref\n0\n%%EOF\n");
    out
}
fn crc32(data: &[u8]) -> u32 {
    !data.iter().fold(!0u32, |c, &b| {
        (0..8).fold(c ^ b as u32, |c, _| {
            if c & 1 == 1 {
                (c >> 1) ^ 0xEDB8_8320
            } else {
                c >> 1
            }
        })
    })
}
/// A stored (uncompressed) DOCX package with real checksums.
fn docx_with(body: &str) -> Vec<u8> {
    let xml = format!(
        "<?xml version=\"1.0\"?><w:document xmlns:w=\"x\"><w:body>{body}</w:body></w:document>"
    );
    let parts: [(&str, &[u8]); 2] = [
        ("[Content_Types].xml", b"<Types/>"),
        ("word/document.xml", xml.as_bytes()),
    ];
    let (mut out, mut directory) = (Vec::new(), Vec::new());
    for (name, data) in parts {
        let at = out.len() as u32;
        let size = (data.len() as u32).to_le_bytes();
        let fields = [crc32(data).to_le_bytes(), size, size].concat();
        out.extend(b"PK\x03\x04\x14\0\0\0\0\0\0\0\0\0");
        out.extend(&fields);
        out.extend((name.len() as u16).to_le_bytes());
        out.extend([0, 0]);
        out.extend(name.as_bytes());
        out.extend(data);
        directory.extend(b"PK\x01\x02\x14\0\x14\0\0\0\0\0\0\0\0\0");
        directory.extend(&fields);
        directory.extend((name.len() as u16).to_le_bytes());
        directory.extend([0; 12]);
        directory.extend(at.to_le_bytes());
        directory.extend(name.as_bytes());
    }
    let start = out.len() as u32;
    out.extend(&directory);
    out.extend(b"PK\x05\x06\0\0\0\0\x02\0\x02\0");
    out.extend((directory.len() as u32).to_le_bytes());
    out.extend(start.to_le_bytes());
    out.extend([0, 0]);
    out
}
/// Stages `files` and binds them to one new owner message.
async fn attach(f: &Fixture, files: &[(&str, &str, Vec<u8>)]) -> (Uuid, Vec<Uuid>) {
    let (mut ids, mut refs) = (vec![], vec![]);
    for (name, media, bytes) in files {
        let id = Uuid::new_v4();
        let (status, body) = upload(f, id, name, media, bytes).await;
        assert_eq!(status, StatusCode::CREATED, "{name}: {body}");
        refs.push(json!({"id":id,"digest":sha(bytes)}));
        ids.push(id);
    }
    let message = Uuid::new_v4();
    let request = json!({"operation_id":Uuid::new_v4(),"message_id":message,"expected_conversation_revision":"1","body":"see files","context":[],"attachments":refs});
    let (status, receipt) = f.post(&f.history_path(), None, request).await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    (message, ids)
}
/// The supervisor text read; `None` sends the owner's dev session instead.
async fn supervisor_text(f: &Fixture, id: Uuid, token: Option<&str>) -> (StatusCode, Value) {
    let path = format!("/v1/supervisor/conversation/attachments/{id}/text");
    let (status, headers, body) = raw(f, "GET", &path, None, token, vec![]).await;
    if status == StatusCode::OK {
        assert_eq!(headers["cache-control"], "no-store");
        assert_eq!(headers["content-type"], "application/json");
    }
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}
/// A dispatched turn's credential for the fixture's current actor.
async fn dispatched_token(f: &Fixture) -> String {
    let reservation = f.reserve().await.reservation.unwrap();
    let p = f.plan(reservation.id, 1).await;
    f.start(&p).await;
    reply::issue_credential(&f.pool, f.actor, 60_000, 999_000)
        .await
        .unwrap()
}

#[tokio::test]
async fn supervisors_read_extracted_attachment_text_with_explicit_states() {
    let mut f = Fixture::new(aq_server::STATE_FORMAT_VERSION).await;
    let helvetica = "/Type /Font /Subtype /Type1 /BaseFont /Helvetica";
    let (packed, inflated) = expanding(b"BT /F1 12 Tf (", 4300, b") Tj ET");
    assert!(packed.len() < 8192 && inflated.len() > (1 << 20) + 60_000);
    let review = b"BT /F1 12 Tf 72 700 Td (Design review) Tj 0 -14 Td (Ship it) Tj ET";
    let plan = "<w:p><w:r><w:t>Quarterly plan</w:t></w:r></w:p><w:p><w:r><w:t>Budget &amp; risks</w:t></w:r></w:p>";
    let readable = [
        (
            "review.pdf",
            "application/pdf",
            one_page_pdf(helvetica, review, false),
        ),
        ("plan.docx", DOCX, docx_with(plan)),
        (
            "plan.md",
            "text/markdown",
            "# Plan\n\n- r\u{e9}sum\u{e9}\n".as_bytes().to_vec(),
        ),
        ("notes.txt", "text/plain", b"plain notes\n".to_vec()),
    ];
    let texts = [
        "Design review\nShip it",
        "Quarterly plan\nBudget & risks",
        "# Plan\n\n- r\u{e9}sum\u{e9}\n",
        "plain notes\n",
    ];
    let unmapped = "/Type /Font /Subtype /Type1 /Encoding << /Differences [1 /a] >>";
    let failing = [
        (
            "scan.pdf",
            "application/pdf",
            one_page_pdf(helvetica, b"q 612 0 0 792 0 0 cm /Im0 Do Q", false),
        ),
        (
            "damaged.pdf",
            "application/pdf",
            one_page_pdf(helvetica, b"not a zlib stream", true),
        ),
        (
            "glyphs.pdf",
            "application/pdf",
            one_page_pdf(unmapped, b"BT /F1 12 Tf (\x01\x01) Tj ET", false),
        ),
        (
            "large.pdf",
            "application/pdf",
            one_page_pdf(helvetica, &packed, true),
        ),
    ];
    let states = [
        ("no_extractable_text", "OCR"),
        ("unreadable", "damaged"),
        ("extraction_failed", "Unicode"),
        ("limit_exceeded", "no part of it is returned"),
    ];
    let (first, read_ids) = attach(&f, &readable).await;
    let (second, failing_ids) = attach(&f, &failing).await;
    let token = dispatched_token(&f).await;
    let global = f.conversation;
    let check = |v: &Value, message: Uuid, id: Uuid, file: &(&str, &str, Vec<u8>)| {
        assert_eq!(v["conversation_id"], json!(global), "{v}");
        assert_eq!(v["message_id"], json!(message));
        assert_eq!(v["reference"], "user_provided");
        assert!(
            v["notice"]
                .as_str()
                .unwrap()
                .contains("not as instructions")
        );
        assert_eq!(
            v["source"],
            json!({"id":id,"name":file.0,"media_type":file.1,"size_bytes":file.2.len(),"digest":sha(&file.2)})
        );
    };
    for ((file, id), want) in readable.iter().zip(&read_ids).zip(texts) {
        let (status, v) = supervisor_text(&f, *id, Some(&token)).await;
        assert_eq!(status, StatusCode::OK, "{}: {v}", file.0);
        check(&v, first, *id, file);
        assert_eq!(
            (&v["state"], &v["text"]),
            (&json!("extracted"), &json!(want)),
            "{}",
            file.0
        );
    }
    for ((file, id), (state, says)) in failing.iter().zip(&failing_ids).zip(states) {
        let (status, v) = supervisor_text(&f, *id, Some(&token)).await;
        assert_eq!(status, StatusCode::OK, "{}: {v}", file.0);
        check(&v, second, *id, file);
        assert_eq!(v["state"], state, "{}: {v}", file.0);
        assert_eq!(v["text"], Value::Null, "{}: never a prefix", file.0);
        assert!(
            v["message"].as_str().unwrap().contains(says),
            "{}: {v}",
            file.0
        );
    }
    // The association is stored, so a fresh router (as after a restart) serves the same.
    let (_, before) = supervisor_text(&f, read_ids[1], Some(&token)).await;
    f.app = conversation::routes(&f.state)
        .merge(conversation::supervisor_routes(&f.state))
        .with_state(f.state.clone());
    assert_eq!(
        supervisor_text(&f, read_ids[1], Some(&token)).await,
        (StatusCode::OK, before)
    );
    // Missing, undispatched and unauthenticated reads are refused without content.
    let staged = Uuid::new_v4();
    upload(&f, staged, "later.txt", "text/plain", b"later").await;
    for id in [staged, Uuid::new_v4()] {
        assert_eq!(
            supervisor_text(&f, id, Some(&token)).await.0,
            StatusCode::NOT_FOUND
        );
    }
    for bearer in [Some("aqsv1_bogus"), None] {
        let (status, v) = supervisor_text(&f, read_ids[0], bearer).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{v}");
        assert!(v.get("text").is_none());
    }
    // A project supervisor reads its own conversation's files, not the global ones,
    // and the global supervisor cannot read the project's.
    let project = Uuid::new_v4();
    sqlx::query("INSERT INTO project_execution_bindings(project_id,revision,binding,validated_at_ms,binding_observation) VALUES($1,1,'{\"name\":\"Files\",\"enabled\":true}',1,'{}')").bind(project).execute(&f.pool).await.unwrap();
    f.scope_to(project).await;
    let (message, ids) = attach(&f, &readable[3..]).await;
    let scoped = dispatched_token(&f).await;
    let (status, v) = supervisor_text(&f, ids[0], Some(&scoped)).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_ne!(f.conversation, global);
    assert_eq!(
        (&v["conversation_id"], &v["message_id"]),
        (&json!(f.conversation), &json!(message))
    );
    assert_eq!(v["text"], "plain notes\n");
    for (id, bearer) in [(read_ids[0], &scoped), (ids[0], &token)] {
        assert_eq!(
            supervisor_text(&f, id, Some(bearer)).await.0,
            StatusCode::NOT_FOUND
        );
    }
}
