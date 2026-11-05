//! H4 durable waits: registration replay and a HumanDecision wake that is
//! recorded once and consumed once.
use super::*;
use aq_server::agent_access::waits;
use aq_wait_engine::wire;

pub(super) struct WaitEnv {
    pub(super) env: ReviewEnv,
    pub(super) decision_id: Uuid,
    pub(super) scope: wire::Scope,
    pub(super) binding: wire::Binding,
}
impl WaitEnv {
    pub(super) fn node(&self) -> Uuid {
        self.env.f.node.node_id
    }
    pub(super) fn attempt(&self) -> Uuid {
        self.env.report.binding.attempt_id
    }
    pub(super) fn registration(
        &self,
        wait_id: Uuid,
        condition: wire::Condition,
    ) -> wire::WaitRegistration {
        let predicate = wire::Predicate::Single {
            member: wire::Member {
                member_id: Uuid::new_v4(),
                condition,
            },
        };
        wire::WaitRegistration {
            scope: self.scope.clone(),
            binding: self.binding.clone(),
            wait_id,
            wait_generation: 1,
            continuation_generation: 1,
            policy: waits::default_policy().reference,
            predicate_digest: wire::canonical_digest(&predicate).unwrap(),
            predicate,
            preference: wire::WaitPreference::Economical,
        }
    }
    pub(super) async fn condition(&self) -> wire::Condition {
        let row = sqlx::query("SELECT d.subject_digest,d.request_digest,s.report_id FROM human_decision_requests d JOIN review_subjects s USING(subject_digest) WHERE d.decision_id=$1")
            .bind(self.decision_id)
            .fetch_one(&self.env.f.db.pool().await)
            .await
            .unwrap();
        let subject: String = sqlx::Row::get(&row, "subject_digest");
        let asked: String = sqlx::Row::get(&row, "request_digest");
        wire::Condition::HumanDecision {
            decision_id: self.decision_id,
            subject_id: sqlx::Row::get(&row, "report_id"),
            subject_revision: wire::Text::new(subject).unwrap(),
            question_digest: serde_json::from_value(json!(asked)).unwrap(),
        }
    }
    pub(super) async fn register(&self, request: &wire::Request) -> wire::Response {
        waits::register(&self.env.f.state, self.node(), self.attempt(), request)
            .await
            .unwrap()
    }
}

pub(super) async fn wait_env() -> WaitEnv {
    let env = review_env(1).await;
    let f = &env.f;
    let request = aq_review_protocol::DecisionRequest {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        decision_id: Uuid::new_v4(),
        subject: env.prepared.subject().artifact().unwrap(),
        eligible_owners: vec![f.actor],
        eligibility_project_revision: 1,
        review_policy: env.prepared.subject().validation_policy.clone(),
        expires_at_ms: NOW + 3_600_000,
    };
    env.service
        .admit(f.actor, &env.prepared, &request)
        .await
        .unwrap();
    let (scope, binding) =
        waits::caller_identity(&f.state, f.node.node_id, env.report.binding.attempt_id)
            .await
            .unwrap();
    WaitEnv {
        decision_id: request.decision_id,
        env,
        scope,
        binding,
    }
}
fn refusal(response: &wire::Response) -> Option<wire::RefusalCode> {
    match &response.body {
        wire::ResponseBody::Refused { code, .. } => Some(code.clone()),
        _ => None,
    }
}
pub(super) fn registered(response: &wire::Response) -> wire::RegisteredWait {
    match &response.body {
        wire::ResponseBody::WaitRegistered(wait) => (**wait).clone(),
        other => panic!("expected registration, got {other:?}"),
    }
}

#[tokio::test]
async fn wait_registration_replay_is_idempotent() {
    let w = Box::pin(wait_env()).await;
    let f = &w.env.f;
    let condition = w.condition().await;
    let wait_id = Uuid::new_v4();
    let operation = Uuid::new_v4();
    let request = wire::Request::seal(
        operation,
        wire::Command::RegisterWait(w.registration(wait_id, condition.clone())),
    )
    .unwrap();
    let first = w.register(&request).await;
    let original = registered(&first);
    // A replay after time passes returns the original record: same age, same
    // deadline, same budgets.
    f.time.store((NOW + 5_000) as i64, Ordering::SeqCst);
    let replay = w.register(&request).await;
    assert_eq!(json!(replay), json!(first));
    assert_eq!(registered(&replay).original_created_at_ms, NOW);
    assert_eq!(original.original_created_at_ms, NOW);
    // Same operation, changed bytes: conflict, never a second wait.
    let changed = wire::Request::seal(
        operation,
        wire::Command::RegisterWait(w.registration(Uuid::new_v4(), condition.clone())),
    )
    .unwrap();
    assert_eq!(
        refusal(&w.register(&changed).await),
        Some(wire::RefusalCode::ChangedPayload)
    );
    // A second active wait for the same attempt is refused.
    let second = wire::Request::seal(
        Uuid::new_v4(),
        wire::Command::RegisterWait(w.registration(Uuid::new_v4(), condition)),
    )
    .unwrap();
    assert_eq!(
        refusal(&w.register(&second).await),
        Some(wire::RefusalCode::InvalidRequest)
    );
    let pool = f.db.pool().await;
    let (rows, deadline): (i64, i64) =
        sqlx::query_as("SELECT count(*),max(deadline_ms) FROM agent_waits")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(rows, 1);
    assert_eq!(deadline as u64, NOW + aq_wait_engine::MAX_AGE_MS);
}

#[tokio::test]
async fn wait_on_decision_wakes_once_after_resolution() {
    let w = Box::pin(wait_env()).await;
    let f = &w.env.f;
    let pool = f.db.pool().await;
    let wait_id = Uuid::new_v4();
    let request = wire::Request::seal(
        Uuid::new_v4(),
        wire::Command::RegisterWait(w.registration(wait_id, w.condition().await)),
    )
    .unwrap();
    let wait = registered(&w.register(&request).await);
    let phase: String = sqlx::query_scalar("SELECT phase FROM agent_waits WHERE wait_id=$1")
        .bind(wait_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(phase, "pending");

    Box::pin(w.env.service.resolve(
        f.actor,
        &aq_review_protocol::ResolveRequest {
            schema_version: 1,
            operation_id: Uuid::new_v4(),
            decision_id: w.decision_id,
            expected_decision_revision: 1,
            choice: aq_review_protocol::Choice::Approve,
            comment: Some("Synthetic".into()),
        },
    ))
    .await
    .unwrap();
    f.time.store((NOW + 61_000) as i64, Ordering::SeqCst);
    // Duplicate reconciliation records exactly one outcome and one intent.
    waits::reconcile_due(&f.state, 64).await.unwrap();
    f.time.store((NOW + 125_000) as i64, Ordering::SeqCst);
    waits::reconcile_due(&f.state, 64).await.unwrap();
    let (outcomes, kind): (i64, Option<String>) =
        sqlx::query_as("SELECT count(*),max(kind) FROM agent_wait_outcomes WHERE wait_id=$1")
            .bind(wait_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((outcomes, kind.as_deref()), (1, Some("satisfied")));
    let intents: Vec<String> =
        sqlx::query_scalar("SELECT state FROM agent_wait_resume_intents WHERE wait_id=$1")
            .bind(wait_id)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(intents, ["ready"]);

    let inspect = wire::Request::seal(
        Uuid::new_v4(),
        wire::Command::Inspect(wire::InspectRequest {
            scope: w.scope.clone(),
            target: wire::InspectTarget::Wait {
                wait: wait.wait.clone(),
            },
            page: wire::PageRequest {
                limit: 1,
                cursor: None,
            },
        }),
    )
    .unwrap();
    let page = waits::inspect(&f.state, w.node(), w.attempt(), &inspect)
        .await
        .unwrap();
    let wire::ResponseBody::Inspection(page) = page.body else {
        panic!("expected inspection page");
    };
    let [wire::InspectionItem::Wait(view)] = page.items.as_slice() else {
        panic!("expected one wait view");
    };
    let outcome = view.outcome.clone().expect("recorded outcome");
    assert_eq!(outcome.kind, wire::WaitOutcomeKind::Satisfied);

    let ack = |admission_id: Uuid, generation: u64| {
        let mut continuation = wait.continuation.clone();
        continuation.generation = generation;
        wire::Request::seal(
            Uuid::new_v4(),
            wire::Command::ContinuationAck(wire::AckRequest {
                admission_id,
                continuation,
                successor: w.binding.clone(),
                attachment_digest: wire::Digest::of(b"synthetic attachment"),
                outcome: outcome.outcome.clone(),
            }),
        )
        .unwrap()
    };
    // A stale continuation generation cannot consume the intent.
    let stale = waits::ack(&f.state, w.node(), w.attempt(), &ack(Uuid::new_v4(), 2))
        .await
        .unwrap();
    assert_eq!(refusal(&stale), Some(wire::RefusalCode::StaleGeneration));
    let admission = Uuid::new_v4();
    let request = ack(admission, 1);
    let first = waits::ack(&f.state, w.node(), w.attempt(), &request)
        .await
        .unwrap();
    let duplicate = waits::ack(&f.state, w.node(), w.attempt(), &request)
        .await
        .unwrap();
    let wire::ResponseBody::ContinuationAcknowledged {
        acknowledgment_receipt,
        ..
    } = &first.body
    else {
        panic!("expected acknowledgment, got {:?}", first.body);
    };
    assert_eq!(json!(duplicate), json!(first));
    assert!(
        acknowledgment_receipt
            .id
            .as_str()
            .contains(&admission.to_string())
    );
    let intents: Vec<String> =
        sqlx::query_scalar("SELECT state FROM agent_wait_resume_intents WHERE wait_id=$1")
            .bind(wait_id)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(intents, ["consumed"]);
}

mod hibernation {
    //! H5: CON-03 suspend → checkpoint → park → disk release → successor
    //! continuation → bootstrap acknowledgment.
    use super::*;
    use aq_lifecycle_api::worker_lifecycle as lc;
    use aq_server::{error::AppError, hibernation};

    fn receipts(binding: &lc::Binding) -> Vec<lc::RetentionReceipt> {
        let digest = lc::canonical_digest(binding).unwrap();
        [lc::ArtifactRole::Source, lc::ArtifactRole::Checkpoint]
            .into_iter()
            .map(|role| lc::RetentionReceipt {
                receipt_id: Uuid::new_v4(),
                owner: lc::Text::new("aq-retention-owner").unwrap(),
                binding_digest: digest.clone(),
                artifact: artifact("retained", format!("{role:?}").as_bytes()),
                role,
                bytes: 10,
                retained_at_ms: NOW,
                retained_until_ms: NOW + 86_400_000,
            })
            .collect()
    }
    fn parking(
        intent: &lc::SuspensionIntent,
        disposition: lc::SuspensionDisposition,
        central_receipts: Vec<lc::RetentionReceipt>,
        directive: Option<lc::ArtifactRef>,
    ) -> lc::Message {
        lc::Message::Parking(lc::ParkingReport {
            binding: intent.binding.clone(),
            continuation: intent.continuation.clone(),
            disposition,
            stop: lc::StopObservation::ReportedStopped {
                observation_id: Uuid::new_v4(),
                guardian_instance_id: Uuid::new_v4(),
                writer_set_digest: lc::Digest::of(b"writers"),
                writer_count: 0,
                observed_at_ms: NOW,
                proof: artifact("stop-proof", b"stopped"),
            },
            local_retained_bytes: 0,
            central_receipts,
            effects: lc::EffectsObservation::Reconciled {
                receipt: artifact("effects", b"reconciled"),
            },
            compute_released: true,
            protected_disk_released: directive.is_some(),
            disk_release_directive: directive,
        })
    }
    /// A fresh attempt and session for the same task on the same node.
    async fn successor(f: &mut Fixture, predecessor: Uuid) -> Uuid {
        // Synthetic: release the predecessor's task hold without the real
        // release owner (outside this H5 slice).
        let pool = f.db.pool().await;
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("SET LOCAL session_replication_role = replica")
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("UPDATE execution_attempts SET holds_task=false WHERE attempt_id=$1")
            .bind(predecessor)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let (old_state, old_decision) = (f.profiled.clone(), f.decision);
        let attempt = Uuid::new_v4();
        let session = aq_session_protocol::SessionIdentity::Remote {
            node_id: f.node.node_id,
            boot_id: f.node.boot_id,
            attempt_id: attempt,
            assignment_generation: 1,
            session_id: Uuid::new_v4(),
        };
        f.decision = Uuid::new_v4();
        f.profiled.offer.binding.attempt_id = attempt;
        f.profiled.offer.binding.offer_id = Uuid::new_v4();
        f.profiled.offer.binding.session = session.clone();
        f.profiled.offer.manifest.bindings.session = session;
        f.profiled.offer.manifest.bindings.attempt_id = attempt;
        f.profiled.offer.binding.launch_manifest_digest =
            aq_session_protocol::canonical_digest(&f.profiled.offer.manifest).unwrap();
        f.profiled.offer.binding.execution_spec_digest = aq_selection::digest(&json!({"schema_version":1,"kind":"synthetic_profiled_authority_fixture","task_id":f.selection.lineage.task_id})).unwrap();
        if let Some(grant) = f.profiled.grant.as_mut() {
            grant.binding = f.profiled.offer.binding.clone();
            grant.grant_id = Uuid::new_v4();
        }
        common::profiled::insert_sibling(f).await;
        f.profiled = old_state;
        f.decision = old_decision;
        attempt
    }

    #[tokio::test]
    async fn hibernation_suspend_park_continue_acknowledge() {
        let mut w = Box::pin(wait_env()).await;
        let wait_id = Uuid::new_v4();
        let request = wire::Request::seal(
            Uuid::new_v4(),
            wire::Command::RegisterWait(w.registration(wait_id, w.condition().await)),
        )
        .unwrap();
        let wait = registered(&w.register(&request).await);
        let state = w.env.f.state.clone();
        let auth = context(&w.env.f).await;

        // (1) The Suspend directive is bound to the exact predecessor binding.
        let directive = hibernation::issue_suspension(&state, wait_id)
            .await
            .unwrap();
        let lc::Message::Suspend(intent) = &directive.body.message else {
            panic!("expected Suspend");
        };
        assert_eq!(intent.binding, w.binding);
        assert_eq!(intent.continuation, wait.continuation);
        assert_eq!(
            hibernation::issue_suspension(&state, wait_id)
                .await
                .unwrap(),
            directive
        );
        let delivered = hibernation::directives(&state, &auth, w.attempt())
            .await
            .unwrap();
        assert_eq!(delivered.suspend.as_ref(), Some(&directive));

        // (2) Checkpoint receipt with message_id + body_digest dedup.
        let summary = lc::SummaryClaim {
            text: lc::SummaryText::new("parked awaiting human decision").unwrap(),
            context_references: vec![],
            pending_operation_ids: vec![Uuid::new_v4()],
        };
        let checkpoint = |text: &str| {
            let mut summary = summary.clone();
            summary.text = lc::SummaryText::new(text).unwrap();
            lc::Message::Checkpoint(lc::Checkpoint {
                binding: intent.binding.clone(),
                continuation: intent.continuation.clone(),
                claim: lc::CheckpointClaim::Supplied {
                    summary_digest: lc::canonical_digest(&summary).unwrap(),
                    summary,
                },
            })
        };
        let message_id = Uuid::new_v4();
        let packet = lc::Packet::seal(message_id, checkpoint("parked")).unwrap();
        let receipt = hibernation::receive(&state, &auth, &packet).await.unwrap();
        assert_eq!(
            receipt.disposition,
            lc::ReceiptDisposition::CheckpointRecorded
        );
        receipt.validate_against(&packet).unwrap();
        assert_eq!(
            hibernation::receive(&state, &auth, &packet).await.unwrap(),
            receipt
        );
        let changed = lc::Packet::seal(message_id, checkpoint("changed")).unwrap();
        assert!(matches!(
            hibernation::receive(&state, &auth, &changed).await,
            Err(AppError::Conflict(_))
        ));
        // (5) No disk release before a retained suspension.
        assert!(matches!(
            hibernation::issue_disk_release(&state, intent.continuation.continuation_id).await,
            Err(AppError::Conflict(_))
        ));

        // (2) RetainedSuspended parking report.
        let central = receipts(&intent.binding);
        let parked = lc::Packet::seal(
            Uuid::new_v4(),
            parking(
                intent,
                lc::SuspensionDisposition::RetainedSuspended,
                central.clone(),
                None,
            ),
        )
        .unwrap();
        let receipt = hibernation::receive(&state, &auth, &parked).await.unwrap();
        assert_eq!(receipt.disposition, lc::ReceiptDisposition::ParkingRecorded);

        // (5) Explicit protected-disk release, then the second report.
        let cid = intent.continuation.continuation_id;
        let release = hibernation::issue_disk_release(&state, cid).await.unwrap();
        assert_eq!(
            hibernation::issue_disk_release(&state, cid).await.unwrap(),
            release
        );
        let wrong = lc::Packet::seal(
            Uuid::new_v4(),
            parking(
                intent,
                lc::SuspensionDisposition::RetainedSuspended,
                central.clone(),
                Some(artifact("forged-release", b"forged")),
            ),
        )
        .unwrap();
        assert!(matches!(
            hibernation::receive(&state, &auth, &wrong).await,
            Err(AppError::Conflict(_))
        ));
        let released = lc::Packet::seal(
            Uuid::new_v4(),
            parking(
                intent,
                lc::SuspensionDisposition::RetainedSuspended,
                central.clone(),
                Some(release.clone()),
            ),
        )
        .unwrap();
        let receipt = hibernation::receive(&state, &auth, &released)
            .await
            .unwrap();
        assert_eq!(receipt.disposition, lc::ReceiptDisposition::ParkingRecorded);
        let pool = w.env.f.db.pool().await;
        let (phase, disk_released): (String, bool) = sqlx::query_as(
            "SELECT phase,disk_released FROM hibernation_suspensions WHERE continuation_id=$1",
        )
        .bind(cid)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((phase.as_str(), disk_released), ("parked_retained", true));

        // Resolve the wait so its resume intent is ready.
        let f = &w.env.f;
        Box::pin(w.env.service.resolve(
            f.actor,
            &aq_review_protocol::ResolveRequest {
                schema_version: 1,
                operation_id: Uuid::new_v4(),
                decision_id: w.decision_id,
                expected_decision_revision: 1,
                choice: aq_review_protocol::Choice::Approve,
                comment: Some("Synthetic".into()),
            },
        ))
        .await
        .unwrap();
        f.time.store((NOW + 61_000) as i64, Ordering::SeqCst);
        waits::reconcile_due(&f.state, 64).await.unwrap();

        // (3) The successor grant's attachment retains the parking receipts.
        let predecessor = w.attempt();
        let next = successor(&mut w.env.f, predecessor).await;
        let node = w.node();
        let remaining = lc::RemainingBudgets {
            lineage_id: intent.binding.lineage_id,
            policy: artifact("budget-policy", b"policy"),
            ledger_checkpoint: artifact("ledger", b"checkpoint"),
            remaining_failure_retries: 1,
            remaining_elapsed_ms: 60_000,
            cost: lc::CostBudget::NotConfigured,
        };
        let fresh = lc::Digest::of(b"fresh offer");
        let packet = hibernation::attach_continuation(
            &state,
            cid,
            node,
            next,
            fresh.clone(),
            remaining.clone(),
        )
        .await
        .unwrap();
        let lc::Message::Continuation(attachment) = &packet.body.message else {
            panic!("expected Continuation");
        };
        assert_eq!(attachment.retained, central);
        assert_eq!(attachment.predecessor, intent.binding);
        assert_eq!(attachment.successor.attempt_id, next);
        assert_eq!(
            hibernation::attach_continuation(&state, cid, node, next, fresh, remaining)
                .await
                .unwrap(),
            packet
        );
        let auth = context(&w.env.f).await;
        let delivered = hibernation::directives(&state, &auth, next).await.unwrap();
        assert_eq!(delivered.continuation.as_ref(), Some(&packet));

        // H4 acknowledgment, then the exact BootstrapAck.
        let digest = lc::canonical_digest(&**attachment).unwrap();
        let ack = wire::Request::seal(
            Uuid::new_v4(),
            wire::Command::ContinuationAck(wire::AckRequest {
                admission_id: attachment.admission_id,
                continuation: attachment.continuation.clone(),
                successor: attachment.successor.clone(),
                attachment_digest: digest.clone(),
                outcome: attachment.outcome.clone(),
            }),
        )
        .unwrap();
        let acked = waits::ack(&state, node, next, &ack).await.unwrap();
        let wire::ResponseBody::ContinuationAcknowledged {
            acknowledgment_receipt,
            ..
        } = acked.body
        else {
            panic!("expected acknowledgment, got {:?}", acked.body);
        };
        let bootstrap = |receipt: lc::ArtifactRef| {
            lc::Packet::seal(
                Uuid::new_v4(),
                lc::Message::BootstrapAck(lc::BootstrapAck {
                    admission_id: attachment.admission_id,
                    continuation: attachment.continuation.clone(),
                    successor: attachment.successor.clone(),
                    attachment_digest: digest.clone(),
                    outcome: attachment.outcome.clone(),
                    acknowledgment_receipt: receipt,
                }),
            )
            .unwrap()
        };
        assert!(matches!(
            hibernation::receive(&state, &auth, &bootstrap(artifact("forged-ack", b"x"))).await,
            Err(AppError::Conflict(_))
        ));
        let receipt = hibernation::receive(&state, &auth, &bootstrap(acknowledgment_receipt))
            .await
            .unwrap();
        assert_eq!(
            receipt.disposition,
            lc::ReceiptDisposition::BootstrapAcknowledgmentRecorded
        );
        let phase: String = sqlx::query_scalar(
            "SELECT phase FROM hibernation_suspensions WHERE continuation_id=$1",
        )
        .bind(cid)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(phase, "acknowledged");
    }
}
