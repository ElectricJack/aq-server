use super::*;
pub(super) fn receipt(op: &Operation, raw: &Value) -> Result<Receipt> {
    if !contract::upstream_valid("MutationResult", raw)
        || super::super::super::canonical(raw)
            .map_err(|_| unavailable())?
            .len()
            > op.payload.len() + 8192
    {
        return Err(unavailable());
    }
    let payload: Value = serde_json::from_slice(&op.payload).map_err(|_| unavailable())?;
    Creation {
        task: op.id,
        workspace: op.workspace,
        actor: &op.service["actor"],
    }
    .receipt(&payload, raw, contract::upstream_valid)
    .ok_or_else(unavailable)
}
pub(super) async fn run(
    state: &AppState,
    r: &ReadRuntime,
    actor: &User,
    id: Uuid,
) -> Result<Status> {
    let original = journal::load(r, id)
        .await?
        .ok_or_else(|| AppError::NotFound("operation not found".into()))?;
    journal::owns(&original, actor)?;
    let mut tx = journal::transaction(r).await?;
    let blocked = journal::guard(state, &mut tx, actor, &original).await?;
    if original.status.terminal() || blocked.is_some() {
        tx.commit().await.map_err(db)?;
        return journal::status(state, r, actor, &original).await;
    }
    let op = locked(&mut tx, id).await?;
    if op.dispatch_blocked(state) {
        tx.commit().await.map_err(db)?;
        return journal::status(state, r, actor, &op).await;
    }
    let now = stamp(state);
    if now < op.created || now >= op.expires {
        tx.commit().await.map_err(db)?;
        return journal::status(state, r, actor, &op).await;
    }
    let claim = Claim::reserve(tx, op, now).await?;
    claim.dispatch(state, r).await?;
    let saved = journal::load(r, id).await?.ok_or_else(unavailable)?;
    journal::status(state, r, actor, &saved).await
}

impl Operation {
    fn dispatch_blocked(&self, state: &AppState) -> bool {
        let occupied =
            self.status.terminal() || self.lease_until.is_some_and(|until| until > stamp(state));
        occupied || self.count >= MAX_ATTEMPTS || self.stop.is_some()
    }
}
async fn locked(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, id: Uuid) -> Result<Operation> {
    Operation::decode(
        sqlx::query("SELECT * FROM human_graph_operations WHERE operation_id=$1 FOR UPDATE")
            .bind(id)
            .fetch_one(&mut **tx)
            .await
            .map_err(db)?,
    )
}
struct Claim {
    lease: Uuid,
    op: Operation,
    status: Status,
}

impl Claim {
    async fn reserve(
        mut tx: sqlx::Transaction<'_, sqlx::Postgres>,
        op: Operation,
        now: i64,
    ) -> Result<Self> {
        let lease = Uuid::new_v4();
        let mut claimed = op.status.clone();
        claimed.state = OperationState::Unknown;
        claimed.effect = Effect::Possible;
        claimed.recovery = Recovery {
            allowed: false,
            reason: Some("dispatch_in_progress".into()),
        };
        sqlx::query("UPDATE human_graph_operations SET status=$2,dispatch_count=dispatch_count+1,lease_id=$3,lease_expires_at_ms=$4 WHERE operation_id=$1")
            .bind(op.id)
            .bind(serde_json::to_value(&claimed).map_err(|_|unavailable())?)
            .bind(lease)
            .bind(now.checked_add(6000).ok_or_else(unavailable)?)
            .execute(&mut *tx)
            .await.map_err(db)?;
        tx.commit().await.map_err(db)?;
        Ok(Self {
            lease,
            op,
            status: claimed,
        })
    }
    async fn dispatch(self, state: &AppState, r: &ReadRuntime) -> Result<()> {
        let id = self.op.id;
        // Exact retained bytes only; no authority transaction crosses HTTP.
        let outcome = tokio::time::timeout(
            Duration::from_secs(3),
            super::super::super::mutations::post_human_creation(
                runtime(state)?,
                self.op.workspace,
                self.op.payload.clone(),
                false,
            ),
        )
        .await;
        let mut completion = Completion::awaiting(self.status);
        if let Ok(Ok((code, raw))) = outcome {
            completion.observe(&self.op, code, raw);
        }
        let mut tx = journal::transaction(r).await?;
        let current = locked(&mut tx, id).await?;
        if current.lease == Some(self.lease) {
            sqlx::query("UPDATE human_graph_operations SET status=$2,raw_receipt=$3,lease_id=NULL,lease_expires_at_ms=NULL,stop_reason=$4 WHERE operation_id=$1")
                .bind(id)
                .bind(serde_json::to_value(&completion.status).map_err(|_|unavailable())?)
                .bind(completion.raw_receipt)
                .bind(completion.stop)
                .execute(&mut *tx)
                .await.map_err(db)?;
        }
        tx.commit().await.map_err(db)?;
        Ok(())
    }
}

struct Completion {
    status: Status,
    raw_receipt: Option<Value>,
    stop: Option<String>,
}

impl Completion {
    fn awaiting(mut status: Status) -> Self {
        status.recovery = Recovery {
            allowed: true,
            reason: None,
        };
        Self {
            status,
            raw_receipt: None,
            stop: None,
        }
    }
    fn observe(&mut self, op: &Operation, code: StatusCode, raw: Value) {
        if code.is_success() {
            if let Ok(proof) = receipt(op, &raw) {
                self.status.receipt = Some(proof);
                self.raw_receipt = Some(raw);
                self.terminal(OperationState::Confirmed, Effect::Confirmed);
            }
            return;
        }
        let error = &raw["error"]["code"];
        let explicit_rejection =
            error.is_string() && matches!(code.as_u16(), 400 | 401 | 403 | 404 | 422);
        if code == StatusCode::CONFLICT && error == "conflict" {
            self.conflict(op.count);
        } else if op.count == 0 && explicit_rejection {
            self.terminal(OperationState::Rejected, Effect::NotApplied);
        }
    }
    fn conflict(&mut self, prior_dispatches: i32) {
        if prior_dispatches == 0 {
            self.terminal(OperationState::Conflict, Effect::NotApplied);
        } else {
            self.stop = Some("graph_conflict_after_possible_effect".into());
            self.status.recovery = Recovery {
                allowed: false,
                reason: self.stop.clone(),
            };
        }
    }
    fn terminal(&mut self, state: OperationState, effect: Effect) {
        self.status.state = state;
        self.status.effect = effect;
        self.status.recovery.allowed = false;
    }
}
