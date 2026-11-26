use super::*;
use crate::execution::authority::AuthorityView;
use aq_selection::{ResolvedSelection, StructuredFailure, derive_worker_observation};
use axum::http::HeaderMap;
use sqlx::{Postgres, Transaction};
type Tx<'a> = Transaction<'a, Postgres>;

struct Failure<'a> {
    node: Uuid,
    request: &'a StructuredFailure,
    payload: aq_selection::Digest,
}
impl<'a> Failure<'a> {
    fn authenticate(node: Uuid, request: &'a StructuredFailure) -> Result<Self> {
        let identified = request.schema_version == 1
            && request.node_id == node
            && !request.operation_id.is_nil()
            && !request.attempt_id.is_nil();
        if !identified || !(1..=i64::MAX as u64).contains(&request.sequence) {
            return Err(bad("failure identity/sequence"));
        }
        let payload = aq_selection::digest(request).map_err(|_| bad("failure digest"))?;
        Ok(Self {
            node,
            request,
            payload,
        })
    }
    async fn check_boot(&self, tx: &mut Tx<'_>) -> Result<()> {
        let revoked: Option<bool> = sqlx::query_scalar(
            "SELECT revoked_at IS NOT NULL FROM nodes WHERE id=$1 FOR KEY SHARE",
        )
        .bind(self.node)
        .fetch_optional(&mut **tx)
        .await
        .map_err(db)?;
        if revoked != Some(false) {
            return Err(AppError::CredentialRevoked);
        }
        let registration: Value =
            sqlx::query_scalar("SELECT state FROM execution_nodes WHERE node_id=$1 FOR SHARE")
                .bind(self.node)
                .fetch_optional(&mut **tx)
                .await
                .map_err(db)?
                .ok_or_else(|| conflict("execution boot absent"))?;
        if registration["identity"]["boot_id"] != self.request.boot_id.to_string() {
            return Err(conflict("failure boot stale"));
        }
        Ok(())
    }
    async fn replay(&self, tx: &mut Tx<'_>) -> Result<Option<Json<Value>>> {
        if let Some(row)=sqlx::query("SELECT kind,payload_digest,response FROM execution_receipts WHERE node_id=$1 AND operation_id=$2").bind(self.node).bind(self.request.operation_id).fetch_optional(&mut **tx).await.map_err(db)? {
        if row.get::<String,_>("kind")!="intelligence_failure" || row.get::<String,_>("payload_digest")!=self.payload.as_str(){return Err(conflict("failure operation ID conflict"));}
        return Ok(Some(Json(row.get("response"))));
    }
        Ok(None)
    }
    async fn sealed(
        &self,
        state: &AppState,
        tx: &mut Tx<'_>,
    ) -> Result<(AuthorityView, ResolvedSelection)> {
        let attempt = crate::execution::authority::load_attempt_locked(
        state,
        tx,
        self.node,
        self.request.attempt_id,
        crate::execution::authority::Policy::GraphActive,
    )
    .await
    .map_err(|_| {
        conflict(
            "attempt has no actual sealed profiled authority; legacy fixture feedback is refused",
        )
    })?;
        if attempt.kind != crate::execution::authority::Kind::ProfiledV1 {
            return Err(conflict(
                "attempt has no actual sealed profiled selection; legacy fixture feedback is refused",
            ));
        }
        let sealed: Value = sqlx::query_scalar(
        "SELECT selection FROM intelligence_attempt_selections WHERE attempt_id=$1 FOR SHARE",
    )
    .bind(self.request.attempt_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db)?
    .ok_or_else(|| {
        conflict(
            "attempt has no actual sealed profiled selection; legacy fixture feedback is refused",
        )
    })?;
        let sealed: ResolvedSelection =
            serde_json::from_value(sealed).map_err(|_| AppError::Internal)?;
        Ok((attempt, sealed))
    }
    fn observation(
        &self,
        sealed: &ResolvedSelection,
        revision: u64,
        time: u64,
    ) -> Result<AvailabilityObservation> {
        derive_worker_observation(
            self.request,
            &aq_selection::FailureContext {
                observation_id: Uuid::new_v4(),
                authenticated_node: self.node,
                current_boot: self.request.boot_id,
                sealed_attempt: self.request.attempt_id,
                sealed,
                revision,
                now_ms: time,
                ttl_ms: 30_000,
                cooldown_ms: 30_000,
            },
        )
        .map_err(|_| bad("failure lacks exact sealed classifier/selection qualification"))
    }
    async fn record(&self, tx: &mut Tx<'_>, value: &Value) -> Result<()> {
        sqlx::query("INSERT INTO execution_receipts(node_id,operation_id,kind,payload_digest,response) VALUES($1,$2,'intelligence_failure',$3,$4)").bind(self.node).bind(self.request.operation_id).bind(self.payload.as_str()).bind(value).execute(&mut **tx).await.map_err(db)?;
        Ok(())
    }
}
fn current_grant(attempt: &AuthorityView, time: u64, boot: Uuid) -> bool {
    attempt.status == crate::execution::profiled::Status::Granted
        && time < attempt.grant_expires_at_ms
        && attempt.identity.boot_id == boot
}
pub(super) async fn report(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<StructuredFailure>,
) -> Result<Json<Value>> {
    let node = crate::worker_api::authenticate(&state, &headers).await?;
    let failure = Failure::authenticate(node.id, &request)?;
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    failure.check_boot(&mut tx).await?;
    if let Some(response) = failure.replay(&mut tx).await? {
        return Ok(response);
    }
    let (attempt, sealed) = failure.sealed(&state, &mut tx).await?;
    let revision = registry::check_current(&state, &mut tx)
        .await?
        .checked_add(1)
        .filter(|r| *r <= i64::MAX as u64)
        .ok_or(AppError::Internal)?;
    let time = now(&state)?;
    if !current_grant(&attempt, time, request.boot_id) {
        return Err(conflict(
            "failure attempt is not currently granted to this boot",
        ));
    }
    let observation = failure.observation(&sealed, revision, time)?;
    if !loaded(&state)?
        .for_scope(&observation.scope)
        .any(|c| c.enabled)
    {
        return Err(conflict("failure credential revision no longer current"));
    }
    let key = aq_selection::digest(&(&observation.scope, &observation.source))
        .map_err(|_| AppError::Internal)?;
    let previous = observations::previous(&mut tx, &key).await?;
    if previous.is_some_and(|v| request.sequence <= v as u64) {
        return Err(conflict("failure sequence must increase"));
    }
    let value = serde_json::to_value(observation).map_err(|_| AppError::Internal)?;
    registry::quota(&mut tx, previous.is_none(), &value).await?;
    registry::machine_quota(&mut tx, node.id, &value).await?;
    observations::save(&mut tx, &key, (request.sequence, revision), &value).await?;
    failure.record(&mut tx, &value).await?;
    if now(&state)? >= attempt.grant_expires_at_ms {
        return Err(conflict("failure grant expired during lock wait"));
    }
    tx.commit().await.map_err(db)?;
    Ok(Json(value))
}
