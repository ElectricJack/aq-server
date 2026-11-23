use super::*;

struct Acknowledgment<'a> {
    ack: &'a wire::AckRequest,
    request: &'a wire::Request,
    digest: wire::Digest,
}

/// Consume the resume intent exactly once. A duplicate admission returns the
/// original receipt; a stale continuation or outcome consumes nothing.
pub async fn ack(
    state: &AppState,
    node: Uuid,
    attempt: Uuid,
    request: &wire::Request,
) -> Result<wire::Response> {
    let mut tx = state.store.pool.begin().await?;
    let caller = load_caller(state, &mut tx, node, attempt).await?;
    if let Err(error) = request.validate() {
        return Ok(refused(request, contract_refusal(&error)));
    }
    let wire::Command::ContinuationAck(ack) = &request.body.command else {
        return Ok(refused(request, wire::RefusalCode::InvalidRequest));
    };
    if &ack.successor != caller.binding() {
        return Ok(refused(request, wire::RefusalCode::ScopeDenied));
    }
    let admission = Acknowledgment {
        ack,
        request,
        digest: wire::canonical_digest(ack)?,
    };
    let Some(stored) = load_stored(&mut tx, ack.continuation.wait.wait_id, true).await? else {
        return Ok(refused(request, wire::RefusalCode::ScopeDenied));
    };
    if !same_lineage(&stored.snapshot.registration().binding, &ack.successor) {
        return Ok(refused(request, wire::RefusalCode::ScopeDenied));
    }

    if let Some(response) = admission.replay(&mut tx).await? {
        return Ok(response);
    }
    let Some(intent_id) = admission.intent(&mut tx, &stored).await? else {
        return Ok(refused(request, wire::RefusalCode::StaleGeneration));
    };
    let response = admission.consume(&mut tx, state, intent_id).await?;
    tx.commit().await?;
    Ok(response)
}

impl Acknowledgment<'_> {
    async fn replay(&self, tx: &mut Transaction<'_, Postgres>) -> Result<Option<wire::Response>> {
        if let Some(row) =
            sqlx::query("SELECT request_digest,response FROM agent_wait_acks WHERE admission_id=$1")
                .bind(self.ack.admission_id)
                .fetch_optional(&mut **tx)
                .await?
        {
            if row.get::<String, _>("request_digest") != self.digest.as_str() {
                return Ok(Some(refused(
                    self.request,
                    wire::RefusalCode::ChangedPayload,
                )));
            }
            let original: wire::Response = serde_json::from_value(row.get("response"))?;
            return Ok(Some(respond(self.request, original.body)));
        }
        Ok(None)
    }
    async fn intent(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        stored: &Stored,
    ) -> Result<Option<Uuid>> {
        let intent = sqlx::query("SELECT intent_id,continuation_id,continuation_generation,outcome_id FROM agent_wait_resume_intents WHERE wait_id=$1 AND state='ready' FOR UPDATE")
        .bind(self.ack.continuation.wait.wait_id)
        .fetch_optional(&mut **tx)
        .await?;
        let registration = stored.snapshot.registration();
        Ok(intent
            .filter(|row| {
                row.get::<Uuid, _>("continuation_id") == self.ack.continuation.continuation_id
                    && row.get::<i64, _>("continuation_generation") as u64
                        == self.ack.continuation.generation
                    && row.get::<Uuid, _>("outcome_id") == self.ack.outcome.outcome_id
                    && self.ack.continuation.wait == registration.wait_ref()
                    && stored
                        .snapshot
                        .outcome()
                        .is_some_and(|o| o.outcome == self.ack.outcome)
            })
            .map(|row| row.get("intent_id")))
    }
    async fn consume(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        state: &AppState,
        intent_id: Uuid,
    ) -> Result<wire::Response> {
        let response = engine::receipts::acknowledgment(self.request, self.ack, intent_id);
        response.validate_against(self.request)?;
        sqlx::query("INSERT INTO agent_wait_acks(admission_id,intent_id,request_digest,response,created_at_ms) VALUES($1,$2,$3,$4,$5)")
        .bind(self.ack.admission_id)
        .bind(intent_id)
        .bind(self.digest.as_str())
        .bind(serde_json::to_value(&response)?)
        .bind(i64_of(crate::agent_access::now(state))?)
        .execute(&mut **tx)
        .await?;
        sqlx::query(
        "UPDATE agent_wait_resume_intents SET state='consumed',admission_id=$2 WHERE intent_id=$1",
    )
    .bind(intent_id)
    .bind(self.ack.admission_id)
    .execute(&mut **tx)
    .await?;
        Ok(response)
    }
}
