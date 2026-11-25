use super::*;
use sqlx::{Executor, Postgres, Transaction};

impl ManualObservation {
    fn valid(&self) -> bool {
        let identity =
            !self.operation_id.is_nil() && (1..=i64::MAX as u64).contains(&self.sequence);
        let timing = (1000..=3_600_000).contains(&self.ttl_ms) && self.cooldown_ms <= self.ttl_ms;
        identity && timing && self.expected_revision <= i64::MAX as u64
    }
    fn response(self, source: ObservationSource, revision: u64, received: u64) -> Result<Value> {
        let observation = AvailabilityObservation {
            observation_id: Uuid::new_v4(),
            scope: self.scope,
            source,
            sequence: self.sequence,
            revision,
            state: self.state,
            received_at_ms: received,
            expires_at_ms: received
                .checked_add(self.ttl_ms)
                .ok_or_else(|| bad("expiry overflow"))?,
            cooldown_until_ms: received
                .checked_add(self.cooldown_ms)
                .ok_or_else(|| bad("cooldown overflow"))?,
        };
        observation
            .validate()
            .map_err(|_| bad("invalid observation scope"))?;
        serde_json::to_value(&observation).map_err(|_| AppError::Internal)
    }
}

struct Operation<'a> {
    actor: Uuid,
    id: Uuid,
    payload: &'a aq_selection::Digest,
}
impl Operation<'_> {
    async fn replay<'e>(
        &self,
        db_session: impl Executor<'e, Database = Postgres>,
    ) -> Result<Option<Json<Value>>> {
        let row=sqlx::query("SELECT payload_digest,response FROM intelligence_operations WHERE actor_id=$1 AND operation_id=$2").bind(self.actor).bind(self.id).fetch_optional(db_session).await.map_err(db)?;
        row.map(|row| {
            if row.get::<String, _>("payload_digest") != self.payload.as_str() {
                return Err(conflict("operation ID reused with changed observation"));
            }
            Ok(Json(row.get("response")))
        })
        .transpose()
    }
    async fn record(&self, tx: &mut Transaction<'_, Postgres>, response: &Value) -> Result<()> {
        sqlx::query("INSERT INTO intelligence_operations(actor_id,operation_id,payload_digest,response) VALUES($1,$2,$3,$4)").bind(self.actor).bind(self.id).bind(self.payload.as_str()).bind(response).execute(&mut **tx).await.map_err(db)?;
        Ok(())
    }
}

pub(super) async fn previous(
    tx: &mut Transaction<'_, Postgres>,
    key: &aq_selection::Digest,
) -> Result<Option<i64>> {
    sqlx::query_scalar(
        "SELECT sequence FROM intelligence_observations WHERE scope_source_digest=$1",
    )
    .bind(key.as_str())
    .fetch_optional(&mut **tx)
    .await
    .map_err(db)
}

pub(super) async fn save(
    tx: &mut Transaction<'_, Postgres>,
    key: &aq_selection::Digest,
    (sequence, revision): (u64, u64),
    response: &Value,
) -> Result<()> {
    sqlx::query("INSERT INTO intelligence_observations(scope_source_digest,sequence,observation) VALUES($1,$2,$3) ON CONFLICT(scope_source_digest) DO UPDATE SET sequence=excluded.sequence,observation=excluded.observation").bind(key.as_str()).bind(sequence as i64).bind(response).execute(&mut **tx).await.map_err(db)?;
    sqlx::query("UPDATE intelligence_registry SET revision=$1 WHERE singleton=TRUE")
        .bind(revision as i64)
        .execute(&mut **tx)
        .await
        .map_err(db)?;
    Ok(())
}

async fn authorize(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    scope: &ObservationScope,
) -> Result<()> {
    let loaded = loaded(state)?;
    let credential = loaded
        .for_scope(scope)
        .next()
        .ok_or_else(|| bad("unknown exact credential scope"))?;
    if !credential.enabled {
        return Err(conflict("credential disabled"));
    }
    if let Some(node) = scope.node_id {
        let entry = loaded
            .config
            .credentials
            .iter()
            .find(|c| c.credential.profile == credential.profile)
            .ok_or(AppError::Internal)?;
        if !entry.admits(node, aq_machine_control::enrolled(tx, node).await?) {
            return Err(bad("observation node outside credential authority"));
        }
    }
    Ok(())
}

pub(super) async fn observe(
    State(state): State<AppState>,
    RequireOwner(user): RequireOwner,
    Json(request): Json<ManualObservation>,
) -> Result<Json<Value>> {
    if !request.valid() {
        return Err(bad("invalid observation identity/sequence/timing"));
    }
    let payload = aq_selection::digest(&request).map_err(|_| bad("observation digest"))?;
    let operation = Operation {
        actor: user.id,
        id: request.operation_id,
        payload: &payload,
    };
    // Accepted receipts precede current configuration validation.
    if let Some(replay) = operation.replay(&state.store.pool).await? {
        return Ok(replay);
    }
    registry::register(&state).await?;
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    sqlx::query("SELECT pg_advisory_xact_lock(712009)")
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    if let Some(replay) = operation.replay(&mut *tx).await? {
        return Ok(replay);
    }
    let registered = registry::check_current(&state, &mut tx).await?;
    authorize(&state, &mut tx, &request.scope).await?;
    let source = ObservationSource::Operator {
        principal_id: user.id,
    };
    let key = aq_selection::digest(&(&request.scope, &source)).map_err(|_| bad("scope digest"))?;
    let previous = previous(&mut tx, &key).await?;
    if registered != request.expected_revision {
        return Err(conflict("registry revision changed"));
    }
    if previous.is_some_and(|v| request.sequence <= v as u64) {
        return Err(conflict(
            "observation sequence must increase; expired rows retain replay fence",
        ));
    }
    let received = now(&state)?;
    let revision = registered
        .checked_add(1)
        .filter(|v| *v <= i64::MAX as u64)
        .ok_or(AppError::Internal)?;
    let sequence = request.sequence;
    let response = request.response(source, revision, received)?;
    registry::quota(&mut tx, previous.is_none(), &response).await?;
    save(&mut tx, &key, (sequence, revision), &response).await?;
    operation.record(&mut tx, &response).await?;
    tx.commit().await.map_err(db)?;
    Ok(Json(response))
}
