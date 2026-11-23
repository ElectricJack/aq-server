use super::*;

impl Service {
    /// Current requester can cancel a pending question; cancellation cannot
    /// retract an answer. No worker or playbook may impersonate a human answer.
    pub async fn cancel(
        &self,
        actor: Uuid,
        operation: Uuid,
        decision: Uuid,
        expected: u64,
    ) -> Result<DecisionReceipt> {
        self.finish_unanswered(
            Some(actor),
            Cancellation {
                operation,
                decision,
                expected,
            },
        )
        .await
    }
    /// Internal maintenance only, never an expiry flag accepted from JSON.
    pub async fn expire(&self, decision: Uuid) -> Result<DecisionReceipt> {
        self.finish_unanswered(None, Cancellation::at_first_revision(decision, decision))
            .await
    }
    pub(crate) async fn cancel_in_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        actor: Uuid,
        cancellation: Cancellation,
    ) -> Result<DecisionReceipt> {
        self.finish_unanswered_in_tx(tx, Some(actor), cancellation)
            .await
    }
    async fn finish_unanswered(
        &self,
        actor: Option<Uuid>,
        cancellation: Cancellation,
    ) -> Result<DecisionReceipt> {
        let mut tx = self.tx().await?;
        let receipt = self
            .finish_unanswered_in_tx(&mut tx, actor, cancellation)
            .await?;
        tx.commit().await.map_err(db)?;
        Ok(receipt)
    }
    async fn finish_unanswered_in_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        actor: Option<Uuid>,
        cancellation: Cancellation,
    ) -> Result<DecisionReceipt> {
        let decision = cancellation.decision;
        let digest = cancellation.digest(actor)?;
        Self::budget(tx).await?;
        let locked = self.lock_subject(tx, decision).await?;
        locked.validate_binding()?;
        let subject = locked.subject;
        let row =
            sqlx::query("SELECT * FROM human_decision_requests WHERE decision_id=$1 FOR UPDATE")
                .bind(decision)
                .fetch_one(&mut **tx)
                .await
                .map_err(db)?;
        let request: DecisionRequest = self.decode("request", &decision.to_string(), &row)?;
        request.validate().map_err(|_| AppError::Internal)?;
        if hash(&request)? != row.get::<String, _>("request_digest")
            || request.subject != subject.artifact().map_err(|_| AppError::Internal)?
        {
            return Err(AppError::Internal);
        }
        authorize_cancellation(tx, actor, &row).await?;
        if let Some(old) =
            sqlx::query("SELECT * FROM human_decision_finality WHERE decision_id=$1 FOR SHARE")
                .bind(decision)
                .fetch_optional(&mut **tx)
                .await
                .map_err(db)?
        {
            cancellation.validate_replay(&old, actor, &digest)?;
            let receipt = self.read_receipt(decision, &old, &request)?;
            return Ok(receipt);
        }
        if !records::pending(&row) {
            return Err(conflict());
        }
        let at = now(&self.state)?;
        let finality = cancellation.finality(actor, at, &request)?;
        let receipt = records::new_receipt(&request, decision, at, finality)?;
        let receipt_digest = hash(&receipt)?;
        let s = self.seal("finality", &decision.to_string(), &encode(&receipt)?)?;
        sqlx::query("INSERT INTO human_decision_finality(decision_id,actor_id,operation_id,request_digest,receipt_digest,sealed_version,key_id,nonce,ciphertext) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)").bind(decision).bind(actor).bind(cancellation.operation).bind(digest).bind(&receipt_digest).bind(s.version as i32).bind(s.key_id).bind(s.nonce).bind(s.ciphertext).execute(&mut **tx).await.map_err(db)?;
        self.emit_decision(tx, &receipt, &receipt_digest).await?;
        sqlx::query("UPDATE human_decision_requests SET decision_revision=2,state=$2 WHERE decision_id=$1 AND decision_revision=1").bind(decision).bind(if actor.is_none() {"expired"} else {"cancelled"}).execute(&mut **tx).await.map_err(db)?;
        Ok(receipt)
    }
}

pub(crate) struct Cancellation {
    operation: Uuid,
    decision: Uuid,
    expected: u64,
}
impl Cancellation {
    pub(crate) fn at_first_revision(operation: Uuid, decision: Uuid) -> Self {
        Self {
            operation,
            decision,
            expected: 1,
        }
    }
    fn digest(&self, actor: Option<Uuid>) -> Result<String> {
        if self.decision.is_nil() || self.operation.is_nil() {
            return Err(invalid());
        }
        if self.expected != 1 {
            return Err(invalid());
        }
        hash(
            &json!({"actor":actor,"operation":self.operation,"decision":self.decision,"expected":self.expected,"expiry":actor.is_none()}),
        )
    }
    fn validate_replay(
        &self,
        old: &sqlx::postgres::PgRow,
        actor: Option<Uuid>,
        digest: &str,
    ) -> Result<()> {
        if old.get::<String, _>("request_digest") != digest {
            return Err(conflict());
        }
        if old.get::<Uuid, _>("operation_id") != self.operation
            || old.get::<Option<Uuid>, _>("actor_id") != actor
        {
            return Err(conflict());
        }
        Ok(())
    }
    fn finality(
        &self,
        actor: Option<Uuid>,
        at: u64,
        request: &DecisionRequest,
    ) -> Result<DecisionFinality> {
        if actor.is_none() != (at >= request.expires_at_ms) {
            return Err(conflict());
        }
        Ok(match actor {
            None => DecisionFinality::Expired {},
            Some(_) => DecisionFinality::Cancelled {
                operation_id: self.operation,
            },
        })
    }
}
async fn authorize_cancellation(
    tx: &mut Transaction<'_, Postgres>,
    actor: Option<Uuid>,
    row: &sqlx::postgres::PgRow,
) -> Result<()> {
    if let Some(actor) = actor {
        if row.get::<Uuid, _>("requester_id") != actor {
            return Err(forbidden());
        }
        Service::owner(tx, actor).await?;
    }
    Ok(())
}
