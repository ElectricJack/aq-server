use super::*;

impl Service {
    /// Authenticated human answer only. New answers check current B1 head,
    /// project revision and current eligible owner after lock waits. Exact old
    /// receipt replay never creates a new vote or restamps current authority.
    pub async fn resolve(&self, actor: Uuid, answer: &ResolveRequest) -> Result<DecisionReceipt> {
        self.resolve_subject(
            ReviewActor::new(actor, None),
            answer,
            ReviewScope::new(SubjectProfile::Code, None),
        )
        .await
    }
    pub(super) async fn resolve_subject(
        &self,
        owner: ReviewActor<'_>,
        answer: &ResolveRequest,
        scope: ReviewScope<'_>,
    ) -> Result<DecisionReceipt> {
        let actor = owner.id;
        answer.validate().map_err(|_| invalid())?;
        let answer_digest = hash(&json!({"actor":actor,"answer":answer}))?;
        let mut tx = self.tx().await?;
        Self::budget(&mut tx).await?;
        // Read immutable identities without a decision row lock. All operations
        // take common authority before subject/decision; correction takes only
        // common authority/head and cannot form a decision→attempt cycle.
        let locked = self.lock_subject(&mut tx, answer.decision_id).await?;
        scope.validate(&locked.subject)?;
        locked.validate_binding()?;
        let (request, row) = self
            .lock_answer_request(&mut tx, answer.decision_id, &locked.subject)
            .await?;
        let resolution = Resolution {
            owner,
            answer,
            digest: &answer_digest,
            profile: scope.profile,
        };
        if let Some(receipt) = resolution.replay(self, &mut tx, &request).await? {
            tx.commit().await.map_err(db)?;
            return Ok(receipt);
        }
        resolution.validate_pending(&locked, &request, &row)?;
        owner.authorize(&mut tx).await?;
        self.project(
            &mut tx,
            &locked.subject,
            request.eligibility_project_revision,
        )
        .await?;
        let at = now(&self.state)?;
        if at >= request.expires_at_ms {
            return Err(conflict());
        }
        let receipt = records::new_receipt(
            &request,
            answer.decision_id,
            at,
            DecisionFinality::HumanAnswer {
                owner_user_id: actor,
                operation_id: answer.operation_id,
                choice: answer.choice,
                comment_digest: answer
                    .comment
                    .as_ref()
                    .map(|s| aq_session_protocol::Digest::of(s.as_bytes())),
            },
        )?;
        resolution.persist(self, &mut tx, &receipt).await?;
        tx.commit().await.map_err(db)?;
        Ok(receipt)
    }
    async fn lock_answer_request(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        decision: Uuid,
        subject: &OwnedSubject,
    ) -> Result<(DecisionRequest, sqlx::postgres::PgRow)> {
        let row =
            sqlx::query("SELECT * FROM human_decision_requests WHERE decision_id=$1 FOR UPDATE")
                .bind(decision)
                .fetch_one(&mut **tx)
                .await
                .map_err(db)?;
        let request: DecisionRequest = self.decode("request", &decision.to_string(), &row)?;
        request.validate().map_err(|_| AppError::Internal)?;
        if request.subject != subject.artifact().map_err(|_| AppError::Internal)?
            || hash(&request)? != row.get::<String, _>("request_digest")
        {
            return Err(AppError::Internal);
        }
        Ok((request, row))
    }
}

impl ReviewScope<'_> {
    fn validate(&self, subject: &OwnedSubject) -> Result<()> {
        if subject.profile() != self.profile
            || self
                .expected
                .is_some_and(|pin| subject.artifact().as_ref().ok() != Some(pin))
        {
            return Err(forbidden());
        }
        Ok(())
    }
}
struct Resolution<'a> {
    owner: ReviewActor<'a>,
    answer: &'a ResolveRequest,
    digest: &'a str,
    profile: SubjectProfile,
}
impl Resolution<'_> {
    fn validate_pending(
        &self,
        locked: &LockedSubject,
        request: &DecisionRequest,
        row: &sqlx::postgres::PgRow,
    ) -> Result<()> {
        if !locked.result.is_current_report_head {
            return Err(conflict());
        }
        if self.answer.expected_decision_revision != 1 || !records::pending(row) {
            return Err(conflict());
        }
        if !request.eligible_owners.contains(&self.owner.id) {
            return Err(forbidden());
        }
        if self.answer.choice == Choice::Approve && !locked.subject.may_approve() {
            return Err(conflict());
        }
        Ok(())
    }
    async fn replay(
        &self,
        service: &Service,
        tx: &mut Transaction<'_, Postgres>,
        request: &DecisionRequest,
    ) -> Result<Option<DecisionReceipt>> {
        let answer = self.answer;
        let old=sqlx::query("SELECT * FROM human_decision_finality WHERE decision_id=$1 OR (operation_id=$2 AND actor_id=$3) FOR SHARE").bind(answer.decision_id).bind(answer.operation_id).bind(self.owner.id).fetch_all(&mut **tx).await.map_err(db)?;
        match old.as_slice() {
            [] => Ok(None),
            [row] => {
                self.validate_replay(row)?;
                let receipt = service.read_receipt(answer.decision_id, row, request)?;
                self.owner.authorize(tx).await?;
                if let Some(pin) = self.owner.pin
                    && self.profile == SubjectProfile::SourceAndEffects
                {
                    records::validate_actor_pin(row, pin)?;
                }
                Ok(Some(receipt))
            }
            _ => Err(conflict()),
        }
    }
    fn validate_replay(&self, row: &sqlx::postgres::PgRow) -> Result<()> {
        if row.get::<Uuid, _>("decision_id") != self.answer.decision_id
            || row.get::<Option<Uuid>, _>("actor_id") != Some(self.owner.id)
        {
            return Err(conflict());
        }
        if row.get::<Uuid, _>("operation_id") != self.answer.operation_id
            || row.get::<String, _>("request_digest") != self.digest
        {
            return Err(conflict());
        }
        Ok(())
    }
    async fn persist(
        &self,
        service: &Service,
        tx: &mut Transaction<'_, Postgres>,
        receipt: &DecisionReceipt,
    ) -> Result<()> {
        let answer = self.answer;
        let receipt_digest = hash(receipt)?;
        let s = service.seal(
            "finality",
            &answer.decision_id.to_string(),
            &encode(receipt)?,
        )?;
        if self.profile == SubjectProfile::SourceAndEffects {
            sqlx::query("SELECT set_config('aq.settlement_owner','aq.settlement-owner.1',true)")
                .execute(&mut **tx)
                .await
                .map_err(db)?;
        }
        let combined_actor = if self.profile == SubjectProfile::SourceAndEffects {
            Some(self.owner.pin.ok_or_else(forbidden)?)
        } else {
            None
        };
        sqlx::query("INSERT INTO human_decision_finality(decision_id,actor_id,operation_id,request_digest,receipt_digest,sealed_version,key_id,nonce,ciphertext,combined_actor_digest,combined_actor_revision) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)").bind(answer.decision_id).bind(self.owner.id).bind(answer.operation_id).bind(self.digest).bind(&receipt_digest).bind(s.version as i32).bind(s.key_id).bind(s.nonce).bind(s.ciphertext).bind(combined_actor.map(|p|p.identity_digest()).transpose()?).bind(combined_actor.map(|p|p.revision())).execute(&mut **tx).await.map_err(db)?;
        service.emit_decision(tx, receipt, &receipt_digest).await?;
        sqlx::query("UPDATE human_decision_requests SET decision_revision=2,state='answered' WHERE decision_id=$1 AND decision_revision=1").bind(answer.decision_id).execute(&mut **tx).await.map_err(db)?;
        Ok(())
    }
}
