use super::*;
pub(crate) struct Stored {
    pub snapshot: Snapshot,
    pub registered: wire::RegisteredWait,
    pub intent_state: Option<String>,
    pub ack_receipt: Option<wire::ArtifactRef>,
}
pub(crate) async fn load_stored(
    tx: &mut Transaction<'_, Postgres>,
    wait_id: Uuid,
    lock: bool,
) -> Result<Option<Stored>> {
    let sql = if lock {
        "SELECT snapshot,response FROM agent_waits WHERE wait_id=$1 FOR UPDATE"
    } else {
        "SELECT snapshot,response FROM agent_waits WHERE wait_id=$1"
    };
    let Some(row) = sqlx::query(sql)
        .bind(wait_id)
        .fetch_optional(&mut **tx)
        .await?
    else {
        return Ok(None);
    };
    let snapshot = Snapshot::decode(&row.get::<Vec<u8>, _>("snapshot"))?;
    let response: wire::Response = serde_json::from_value(row.get("response"))?;
    let wire::ResponseBody::WaitRegistered(registered) = response.body else {
        return Err(WaitError("internal"));
    };
    let intent = sqlx::query("SELECT i.state,a.response FROM agent_wait_resume_intents i LEFT JOIN agent_wait_acks a USING(intent_id) WHERE i.wait_id=$1")
        .bind(wait_id)
        .fetch_optional(&mut **tx)
        .await?;
    let (intent_state, ack_receipt) = match intent {
        Some(row) => {
            let ack: Option<Value> = row.get("response");
            let receipt = ack
                .map(|v| {
                    serde_json::from_value(v["body"]["body"]["acknowledgment_receipt"].clone())
                })
                .transpose()?;
            (Some(row.get::<String, _>("state")), receipt)
        }
        None => (None, None),
    };
    Ok(Some(Stored {
        snapshot,
        registered: *registered,
        intent_state,
        ack_receipt,
    }))
}
/// H5: one wait's registered continuation and its recorded outcome, if any.
pub(crate) struct WaitContinuation {
    pub binding: wire::Binding,
    pub node_id: Uuid,
    pub continuation: wire::ContinuationRef,
    pub terminal: bool,
    pub outcome: Option<wire::OutcomeRef>,
    pub intent_ready: bool,
}
pub(crate) async fn continuation_of(
    tx: &mut Transaction<'_, Postgres>,
    wait_id: Uuid,
) -> Result<Option<WaitContinuation>> {
    let Some(stored) = load_stored(tx, wait_id, false).await? else {
        return Ok(None);
    };
    let row = sqlx::query("SELECT node_id,phase FROM agent_waits WHERE wait_id=$1")
        .bind(wait_id)
        .fetch_one(&mut **tx)
        .await?;
    Ok(Some(WaitContinuation {
        binding: stored.registered.binding.clone(),
        node_id: row.get("node_id"),
        continuation: stored.registered.continuation.clone(),
        terminal: row.get::<String, _>("phase") == "terminal",
        outcome: stored.snapshot.outcome().map(|o| o.outcome.clone()),
        intent_ready: stored.intent_state.as_deref() == Some("ready"),
    }))
}
