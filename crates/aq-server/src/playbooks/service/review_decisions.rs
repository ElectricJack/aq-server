use super::*;
struct ReviewGrant {
    revision: GrantRevision,
    live: bool,
}
use aq_playbook_domain::{decision_effects::decision_receipt, intent_store::StoredIntent};

impl Service {
    pub fn human_owner(&self) -> Option<Arc<crate::reviews::Service>> {
        self.reviews.clone()
    }
    fn decision_input(
        &self,
        intent: &Intent,
    ) -> Result<crate::playbooks::review_domain::DecisionInput> {
        if !self.profile.has_reviews()
            || intent.command != crate::playbooks::review_domain::decision_contract()
        {
            return Err(Error::Forbidden);
        }
        aq_playbook_domain::validate_intent_identity(intent)?;
        let input: crate::playbooks::review_domain::DecisionInput =
            serde_json::from_value(intent.input.clone())?;
        input.validate()?;
        Ok(input)
    }
    fn decision_id(&self, intent: &Intent) -> Result<Uuid> {
        stable_id(
            "aq.playbook.human-decision",
            self.namespace.namespace_id,
            self.namespace.generation,
            &intent.id,
        )
    }
    fn exact_review_recipient(
        recipient: &Recipient,
        subject: &aq_review_protocol::ReviewSubject,
    ) -> bool {
        if recipient.scope()
            != (
                subject.task.workspace_id,
                subject.project_id,
                subject.task.task_id,
            )
        {
            return false;
        }
        match (recipient, &subject.binding.session) {
            (Recipient::Task { .. }, _) => true,
            (
                Recipient::Session {
                    session_id,
                    attempt_id,
                    generation,
                    ..
                },
                aq_session_protocol::SessionIdentity::Remote {
                    session_id: actual_session,
                    attempt_id: actual_attempt,
                    assignment_generation,
                    ..
                },
            ) => {
                session_id == actual_session
                    && attempt_id == actual_attempt
                    && generation == assignment_generation
            }
            _ => false,
        }
    }
    pub(in crate::playbooks) async fn request_decision(
        &self,
        owner: &RuntimeOwner,
        grant: GrantRevision,
        intent: &Intent,
    ) -> Result<DecisionResult> {
        let input = self.decision_input(intent)?;
        // Exact historical lookup precedes fresh Git I/O and cannot admit a new
        // question. Unknown reads remain unknown, never a second operation ID.
        if let Some(old) = self.reconcile_decision(grant, intent).await? {
            return Ok(old);
        }
        let reviews = self.reviews.as_ref().ok_or(Error::Forbidden)?;
        let (prepared, request) = self.prepare_question(reviews, intent, &input).await?;
        let identity = self.identity(intent)?;
        let stored = StoredIntent::new(&self.namespace, intent, &identity);
        let (mut tx, scope) = self
            .review_transaction(
                owner,
                ReviewGrant {
                    revision: grant,
                    live: true,
                },
                intent,
            )
            .await?;
        if !scope.permits(&input.recipient) || intent.deadline_at_ms <= self.now()? {
            return Err(Error::Forbidden);
        }
        if stored.cancelled(&mut tx).await? {
            Self::owner(&mut tx, self.namespace.owner_user_id).await?;
            tx.commit().await?;
            return Ok(DecisionResult::Cancelled);
        }
        let receipt = decision_receipt(request.decision_id, &request.subject);
        if stored
            .prior_question(&mut tx, request.operation_id, &receipt)
            .await?
        {
            Self::owner(&mut tx, self.namespace.owner_user_id).await?;
            tx.commit().await?;
            return Ok(DecisionResult::Accepted {
                handle: request.decision_id.to_string(),
            });
        }
        // SAME transaction: current namespace/grant, human owner question and
        // bridge effect receipt. No grant revocation/expiry admission window.
        let proposal = prepared.proposal(&request);
        reviews
            .admit_in_tx(&mut tx, self.namespace.owner_user_id, proposal)
            .await
            .map_err(review_error)?;
        self.check_expiry(&mut tx, grant.id).await?;
        sqlx::query("INSERT INTO playbook_effects(namespace_id,generation,intent_id,run_id,command_name,request_digest,command_fingerprint,input_digest,grant_id,grant_revision,operation_id,status,receipt,created_at_ms) VALUES($1,$2,$3,$4,'decision.request',$5,$6,$7,$8,$9,$10,'accepted',$11,$12)").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(&intent.id).bind(&intent.run).bind(identity).bind(intent.command.fingerprint().map_err(|_|Error::Invalid)?).bind(&intent.input_digest).bind(grant.id).bind(grant.revision).bind(request.operation_id).bind(receipt).bind(self.now()?).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(DecisionResult::Accepted {
            handle: request.decision_id.to_string(),
        })
    }
    async fn review_transaction(
        &self,
        owner: &RuntimeOwner,
        access: ReviewGrant,
        intent: &Intent,
    ) -> Result<(Transaction<'_, Postgres>, GrantScope)> {
        let ReviewGrant {
            revision: grant,
            live,
        } = access;
        let mut tx = self.tx().await?;
        crate::reviews::Service::lock_budget(&mut tx)
            .await
            .map_err(review_error)?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        let scope = self
            .grant_record_locked(&mut tx, grant, GrantCheck::review(live))
            .await?;
        self.intent_authority(intent, grant, &scope)?;
        Ok((tx, scope))
    }
    async fn prepare_question(
        &self,
        reviews: &crate::reviews::Service,
        intent: &Intent,
        input: &crate::playbooks::review_domain::DecisionInput,
    ) -> Result<(
        crate::reviews::PreparedReview,
        aq_review_protocol::DecisionRequest,
    )> {
        let prepared = reviews
            .prepare(input.report_id, input.source_operation_id)
            .await
            .map_err(review_error)?;
        if prepared.subject().artifact().map_err(|_| Error::Invalid)? != input.expected_subject
            || !Self::exact_review_recipient(&input.recipient, prepared.subject())
        {
            return Err(Error::Forbidden);
        }
        let request = aq_review_protocol::DecisionRequest {
            schema_version: 1,
            operation_id: stable_id(
                "aq.playbook.human-operation",
                self.namespace.namespace_id,
                self.namespace.generation,
                &intent.id,
            )?,
            decision_id: self.decision_id(intent)?,
            subject: input.expected_subject.clone(),
            eligible_owners: input.eligible_owners.clone(),
            eligibility_project_revision: input.eligibility_project_revision,
            review_policy: prepared.subject().validation_policy.clone(),
            expires_at_ms: input.expires_at_ms,
        };
        Ok((prepared, request))
    }
    pub(in crate::playbooks) async fn reconcile_decision(
        &self,
        grant: GrantRevision,
        intent: &Intent,
    ) -> Result<Option<DecisionResult>> {
        let input = self.decision_input(intent)?;
        let identity = self.identity(intent)?;
        let stored = StoredIntent::new(&self.namespace, intent, &identity);
        let mut tx = self.tx().await?;
        self.namespace_locked(&mut tx).await?;
        let scope = self.grant_locked(&mut tx, grant, false).await?;
        self.intent_authority(intent, grant, &scope)?;
        if !scope.permits(&input.recipient) {
            return Err(Error::Forbidden);
        }
        let row = stored.decision_row(&mut tx).await?;
        let decision = self.decision_id(intent)?;
        if let Some(row) = row {
            stored
                .reconcile_question(&row, &decision_receipt(decision, &input.expected_subject))?;
            tx.commit().await?;
            return self
                .decision_status(decision, &input.expected_subject)
                .await
                .map(Some);
        }
        let cancelled = stored.confirmed_cancellation(&mut tx).await?;
        tx.commit().await?;
        Ok(cancelled.then_some(DecisionResult::Cancelled))
    }
    pub(super) async fn decision_status(
        &self,
        decision: Uuid,
        expected: &aq_session_protocol::ArtifactRef,
    ) -> Result<DecisionResult> {
        let reviews = self.reviews.as_ref().ok_or(Error::Forbidden)?;
        let view = reviews
            .inspect(self.namespace.owner_user_id, decision)
            .await
            .map_err(review_error)?;
        let request: aq_review_protocol::DecisionRequest =
            serde_json::from_value(view["request"].clone())?;
        if request.decision_id != decision || &request.subject != expected {
            return Err(Error::Conflict);
        }
        let Some(value) = view.get("receipt").filter(|v| !v.is_null()) else {
            return Ok(DecisionResult::Accepted {
                handle: decision.to_string(),
            });
        };
        let receipt: aq_review_protocol::DecisionReceipt = serde_json::from_value(value.clone())?;
        receipt
            .validate_for(&request)
            .map_err(|_| Error::Conflict)?;
        let outcome = match receipt.finality {
            aq_review_protocol::DecisionFinality::HumanAnswer {
                choice: aq_review_protocol::Choice::Approve,
                ..
            } => "approved",
            aq_review_protocol::DecisionFinality::HumanAnswer {
                choice: aq_review_protocol::Choice::Reject,
                ..
            } => "rejected",
            aq_review_protocol::DecisionFinality::Expired {} => "expired",
            aq_review_protocol::DecisionFinality::Cancelled { .. } => "cancelled",
        };
        Ok(DecisionResult::Completed {
            outcome: outcome.into(),
            output: json!({"receipt":receipt.artifact().map_err(|_|Error::Invalid)?,"subject":request.subject}),
        })
    }
    fn decision_lookup(&self, intent: &Intent) -> Result<DecisionLookup> {
        if !self.profile.has_reviews() {
            return Err(Error::Forbidden);
        }
        if intent.command != crate::playbooks::review_domain::operation_contract()
            || intent.input_digest != digest(&intent.input)?
        {
            return Err(Error::Forbidden);
        }
        let input: DecisionLookup = serde_json::from_value(intent.input.clone())?;
        input.recipient.validate()?;
        if input.kind != "human_decision" || input.handle.is_nil() {
            return Err(Error::Invalid);
        }
        Ok(input)
    }
    pub(in crate::playbooks) async fn operation_get(
        &self,
        owner: &RuntimeOwner,
        grant: GrantRevision,
        intent: &Intent,
    ) -> Result<DecisionResult> {
        let input = self.decision_lookup(intent)?;
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        let scope = self.grant_locked(&mut tx, grant, true).await?;
        self.intent_authority(intent, grant, &scope)?;
        if !scope.permits(&input.recipient) {
            return Err(Error::Forbidden);
        }
        let reference:Value=sqlx::query_scalar("SELECT receipt->'subject' FROM playbook_effects WHERE namespace_id=$1 AND generation=$2 AND command_name='decision.request' AND receipt->>'handle'=$3").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(input.handle.to_string()).fetch_optional(&mut *tx).await?.ok_or(Error::Forbidden)?;
        let expected: aq_session_protocol::ArtifactRef = serde_json::from_value(reference)?;
        self.check_expiry(&mut tx, grant.id).await?;
        tx.commit().await?;
        let reviews = self.reviews.as_ref().ok_or(Error::Forbidden)?;
        let view = reviews
            .inspect(self.namespace.owner_user_id, input.handle)
            .await
            .map_err(review_error)?;
        let subject: aq_review_protocol::ReviewSubject =
            serde_json::from_value(view["subject"].clone())?;
        if subject.artifact().map_err(|_| Error::Invalid)? != expected
            || !Self::exact_review_recipient(&input.recipient, &subject)
        {
            return Err(Error::Forbidden);
        }
        match self.decision_status(input.handle, &expected).await? {
            DecisionResult::Accepted { handle } => Ok(DecisionResult::Completed {
                outcome: "pending".into(),
                output: json!({"handle":handle,"subject":expected}),
            }),
            result => Ok(result),
        }
    }
    pub(in crate::playbooks) async fn cancel_decision(
        &self,
        owner: &RuntimeOwner,
        grant: GrantRevision,
        intent: &Intent,
    ) -> Result<CancellationResult> {
        let input = self.decision_input(intent)?;
        let identity = self.identity(intent)?;
        let stored = StoredIntent::new(&self.namespace, intent, &identity);
        let key = aq_playbook_host::runner::cancellation_key(intent).map_err(|_| Error::Invalid)?;
        let operation = stable_id(
            "aq.playbook.human-cancellation",
            self.namespace.namespace_id,
            self.namespace.generation,
            &key,
        )?;
        let decision = self.decision_id(intent)?;
        let (mut tx, scope) = self
            .review_transaction(
                owner,
                ReviewGrant {
                    revision: grant,
                    live: false,
                },
                intent,
            )
            .await?;
        if !scope.permits(&input.recipient) {
            return Err(Error::Forbidden);
        }
        if let Some(retained) = stored.retained_cancel(&mut tx, &key).await? {
            Self::owner(&mut tx, self.namespace.owner_user_id).await?;
            return Ok(retained);
        }
        let receipt = decision_receipt(decision, &input.expected_subject);
        let mut disposition = "confirmed";
        if stored.question_exists(&mut tx, &receipt).await? {
            // Finality cannot be retracted. A known final receipt is TooLate;
            // missing/corrupt owner records remain Unknown and retain the key.
            let finality: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM human_decision_finality WHERE decision_id=$1)",
            )
            .bind(decision)
            .fetch_one(&mut *tx)
            .await?;
            if finality {
                disposition = "too_late";
                Self::owner(&mut tx, self.namespace.owner_user_id).await?;
            } else {
                let cancellation =
                    crate::reviews::Cancellation::at_first_revision(operation, decision);
                self.reviews
                    .as_ref()
                    .ok_or(Error::Forbidden)?
                    .cancel_in_tx(&mut tx, self.namespace.owner_user_id, cancellation)
                    .await
                    .map_err(review_error)?;
            }
        } else {
            Self::owner(&mut tx, self.namespace.owner_user_id).await?;
        }
        stored
            .record_cancel(&mut tx, &key, (disposition, self.now()?))
            .await?;
        tx.commit().await?;
        Ok(if disposition == "confirmed" {
            CancellationResult::Confirmed
        } else {
            CancellationResult::TooLate
        })
    }
    /// Only this namespace's retained accepted questions; expiry admits no new
    /// vote. Release namespace locks before taking the review budget lock.
    pub(in crate::playbooks) async fn expire_decisions(
        &self,
        owner: &RuntimeOwner,
        limit: u32,
    ) -> Result<usize> {
        if !self.profile.has_reviews() {
            return Ok(0);
        }
        if !(1..=2).contains(&limit) {
            return Err(Error::Invalid);
        }
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        let ids: Vec<Uuid> = sqlx::query_scalar("SELECT d.decision_id FROM human_decision_requests d JOIN playbook_effects e ON e.receipt->>'handle'=d.decision_id::text WHERE e.namespace_id=$1 AND e.generation=$2 AND e.command_name='decision.request' AND e.status='accepted' AND d.state='pending' AND d.expires_at_ms<=$3 ORDER BY d.expires_at_ms,d.decision_id LIMIT $4")
            .bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(self.now()?).bind(limit as i64).fetch_all(&mut *tx).await?;
        tx.commit().await?;
        let reviews = self.reviews.as_ref().ok_or(Error::Forbidden)?;
        let mut expired = 0;
        for id in ids {
            match reviews.expire(id).await {
                Ok(_) => expired += 1,
                Err(crate::error::AppError::Conflict(_)) => {}
                Err(e) => return Err(review_error(e)),
            }
        }
        Ok(expired)
    }
}
