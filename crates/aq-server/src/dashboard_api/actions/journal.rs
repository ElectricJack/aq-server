//! Durable operation identity and retained responses. No effects are performed
//! inside a journal transaction, and a duplicate admission never dispatches.
use super::*;
use aq_agent_grants::store::{self as grant_store, ReserveError};
use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, Row, Transaction};

async fn transaction(pool: &PgPool) -> Result<Transaction<'_, Postgres>, ApiError> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL statement_timeout='2s'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL lock_timeout='1s'")
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}
fn retained(row: &sqlx::postgres::PgRow, principal: Uuid) -> Result<Operation, ApiError> {
    if row.try_get::<Uuid, _>("principal_id")? != principal {
        return Err(ApiError::for_status(StatusCode::NOT_FOUND));
    }
    if row
        .try_get::<Option<DateTime<Utc>>, _>("retired_at")?
        .is_some()
    {
        return Err(retired(row.try_get("operation_id")?));
    }
    serde_json::from_value(row.try_get("response")?).map_err(|_| ApiError::unavailable())
}
pub(crate) struct Lookup {
    pub kind: String,
    pub scope: Scope,
    pub operation: Option<Operation>,
}
pub(crate) async fn lookup_for_authorization(
    pool: &PgPool,
    principal: Uuid,
    id: Uuid,
) -> Result<Option<Lookup>, ApiError> {
    let mut tx = transaction(pool).await?;
    let Some(row) = sqlx::query(
        "SELECT principal_id,kind,scope,response FROM dashboard_operations WHERE operation_id=$1",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Ok(None);
    };
    if row.try_get::<Uuid, _>("principal_id")? != principal {
        return Err(ApiError::for_status(StatusCode::NOT_FOUND));
    }
    Ok(Some(Lookup {
        kind: row.try_get("kind")?,
        scope: serde_json::from_value(row.try_get("scope")?)
            .map_err(|_| ApiError::unavailable())?,
        operation: row
            .try_get::<Option<serde_json::Value>, _>("response")?
            .map(serde_json::from_value)
            .transpose()
            .map_err(|_| ApiError::unavailable())?,
    }))
}
/// Whether an operation with this ID was admitted (by any principal, in any state).
pub(crate) async fn admitted(pool: &PgPool, id: Uuid) -> Result<bool, ApiError> {
    let found = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM dashboard_operations WHERE operation_id=$1)",
    );
    Ok(found.bind(id).fetch_one(pool).await?)
}
pub(super) fn retired(id: Uuid) -> ApiError {
    ApiError::new(
        StatusCode::GONE,
        "operation_retired",
        "This operation is retired; its ID cannot be reused",
    )
    .operation(id)
}
pub(in crate::dashboard_api) async fn lookup(
    pool: &PgPool,
    principal: Uuid,
    id: Uuid,
) -> Result<Operation, ApiError> {
    let mut tx = transaction(pool).await?;
    let row = sqlx::query("SELECT operation_id,principal_id,response,retired_at FROM dashboard_operations WHERE operation_id=$1 AND principal_id=$2")
        .bind(id).bind(principal).fetch_optional(&mut *tx).await?.ok_or_else(||ApiError::for_status(StatusCode::NOT_FOUND))?;
    retained(&row, principal)
}
pub(super) struct NewEntry<'a> {
    kind: &'a str,
    operation: Operation,
    at: DateTime<Utc>,
}
impl<'a> NewEntry<'a> {
    pub(super) fn pending(
        request: &'a ActionRequest,
        scope: Scope,
        digest: String,
        now: DateTime<Utc>,
    ) -> Self {
        let stamp = now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let operation = Operation::pending(request, scope, digest, stamp);
        Self {
            kind: &request.kind,
            operation,
            at: now,
        }
    }
    pub(super) async fn admit(
        self,
        state: &AppState,
        principal: Uuid,
        admission: Option<AgentAdmission<'_>>,
    ) -> Result<(Operation, bool), ApiError> {
        let response =
            serde_json::to_value(&self.operation).map_err(|_| ApiError::unavailable())?;
        let mut tx = transaction(&state.store.pool).await?;
        let expires = match &admission {
            Some(agent) => Some(
                agent
                    .lock_operation(state, &mut tx, self.operation.operation_id)
                    .await?,
            ),
            None => None,
        };
        if !self.insert(&mut tx, principal, response).await? {
            return Ok((self.previous(&mut tx, principal).await?, false));
        }
        self.reserve(&mut tx, principal, admission.and_then(|a| a.reservation))
            .await?;
        if expires.is_some_and(|expires| expires <= crate::execution::api::now(state)) {
            return Err(agent_error(aq_agent_grants::RefusalCode::SessionExpired));
        }
        // Only a successful commit grants permission to dispatch the new operation.
        tx.commit()
            .await
            .map_err(|_| ApiError::unavailable().operation(self.operation.operation_id))?;
        Ok((self.operation, true))
    }
    async fn insert(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        principal: Uuid,
        response: serde_json::Value,
    ) -> Result<bool, ApiError> {
        let inserted = sqlx::query("INSERT INTO dashboard_operations(operation_id,principal_id,kind,scope,request_digest,disposition,response,created_at,updated_at) VALUES($1,$2,$3,$4,$5,'pending',$6,$7,$7) ON CONFLICT(operation_id) DO NOTHING")
            .bind(self.operation.operation_id)
            .bind(principal)
            .bind(self.kind)
            .bind(serde_json::json!(self.operation.scope))
            .bind(&self.operation.request_digest)
            .bind(response)
            .bind(self.at)
            .execute(&mut **tx).await?.rows_affected() == 1;
        Ok(inserted)
    }
    async fn previous(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        principal: Uuid,
    ) -> Result<Operation, ApiError> {
        let row = sqlx::query("SELECT operation_id,principal_id,request_digest,response,retired_at FROM dashboard_operations WHERE operation_id=$1")
        .bind(self.operation.operation_id).fetch_one(&mut **tx).await?;
        if row.try_get::<Uuid, _>("principal_id")? != principal {
            return Err(ApiError::for_status(StatusCode::NOT_FOUND));
        }
        if row.try_get::<String, _>("request_digest")? != self.operation.request_digest {
            return Err(
                ApiError::for_status(StatusCode::CONFLICT).operation(self.operation.operation_id)
            );
        }
        retained(&row, principal)
    }
    async fn reserve(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        principal: Uuid,
        reservation: Option<DraftReservation>,
    ) -> Result<(), ApiError> {
        if let Some(reservation) = reservation {
            if principal != reservation.ids.attempt_id
                || self.operation.scope
                    != (Scope::Project {
                        project_id: reservation.ids.project_id,
                    })
            {
                return Err(ApiError::invalid());
            }
            grant_store::reserve(
                tx,
                self.operation.operation_id,
                reservation,
                self.at.timestamp_millis(),
            )
            .await
            .map_err(reservation_error)?;
        }
        Ok(())
    }
}
impl AgentAdmission<'_> {
    async fn lock_operation(
        &self,
        state: &AppState,
        tx: &mut Transaction<'_, Postgres>,
        operation: Uuid,
    ) -> Result<u64, ApiError> {
        let expires = self
            .caller
            .recheck_locked(state, self.headers, tx)
            .await
            .map_err(agent_error)?;
        grant_store::lock_operation(tx, operation).await?;
        let legacy: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM agent_graph_operations WHERE operation_id=$1)",
        )
        .bind(operation)
        .fetch_one(&mut **tx)
        .await?;
        if legacy {
            return Err(ApiError::for_status(StatusCode::CONFLICT));
        }
        Ok(expires)
    }
}
fn reservation_error(err: ReserveError) -> ApiError {
    match err {
        ReserveError::QuotaExceeded => agent_error(aq_agent_grants::RefusalCode::QuotaExceeded),
        ReserveError::Conflict => ApiError::for_status(StatusCode::CONFLICT),
        ReserveError::Store(error) => ApiError::from(error),
    }
}
fn agent_error(code: aq_agent_grants::RefusalCode) -> ApiError {
    let status = StatusCode::from_u16(code.status()).unwrap_or(StatusCode::FORBIDDEN);
    ApiError::new(status, code.as_str(), "Agent action admission refused")
}

pub(super) async fn finish(
    pool: &PgPool,
    principal: Uuid,
    operation: &Operation,
    now: DateTime<Utc>,
) -> Result<(), ApiError> {
    let mut tx = transaction(pool).await?;
    let rows = sqlx::query("UPDATE dashboard_operations SET response=$3,disposition=$4,updated_at=GREATEST(updated_at,$5) WHERE operation_id=$1 AND principal_id=$2 AND disposition IN ('pending','unknown') AND retired_at IS NULL")
        .bind(operation.operation_id).bind(principal).bind(serde_json::json!(operation)).bind(serde_json::to_value(operation.disposition).map_err(|_|ApiError::unavailable())?.as_str().ok_or_else(ApiError::unavailable)?).bind(now).execute(&mut *tx).await?.rows_affected();
    if rows != 1 {
        return Err(ApiError::unavailable());
    }
    tx.commit().await?;
    Ok(())
}
/// Explicit maintenance only. Unknown and pending rows are never retired. The
/// immutable tombstone remains under the same unique operation ID forever.
pub async fn retire(pool: &PgPool, now: DateTime<Utc>) -> Result<u64, ApiError> {
    let mut tx = transaction(pool).await?;
    let rows = sqlx::query("UPDATE dashboard_operations SET response=NULL,retired_at=$1 WHERE retired_at IS NULL AND disposition IN ('confirmed','conflict','rejected') AND updated_at <= $1-interval '90 days'")
        .bind(now).execute(&mut *tx).await?.rows_affected();
    tx.commit().await?;
    Ok(rows)
}
