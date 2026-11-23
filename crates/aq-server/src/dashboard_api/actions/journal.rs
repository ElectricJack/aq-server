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
        return Err(ApiError::new(
            StatusCode::GONE,
            "operation_retired",
            "This operation is retired; its ID cannot be reused",
        )
        .operation(row.try_get("operation_id")?));
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
    let row=sqlx::query("SELECT operation_id,principal_id,response,retired_at FROM dashboard_operations WHERE operation_id=$1 AND principal_id=$2")
        .bind(id).bind(principal).fetch_optional(&mut *tx).await?.ok_or_else(||ApiError::for_status(StatusCode::NOT_FOUND))?;
    retained(&row, principal)
}
pub(super) async fn admit(
    state: &AppState,
    principal: Uuid,
    request: &ActionRequest,
    scope: Scope,
    digest: String,
    now: DateTime<Utc>,
    admission: Option<AgentAdmission<'_>>,
) -> Result<(Operation, bool), ApiError> {
    let stamp = now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let operation = Operation {
        operation_id: request.operation_id,
        disposition: Disposition::Pending,
        request_digest: digest.clone(),
        scope: scope.clone(),
        created_at: stamp.clone(),
        updated_at: stamp,
        subject_refs: vec![request.subject.clone()],
        receipt_refs: vec![],
        affected_entities: vec![],
        recovery: Recovery::LookupOriginal,
        current_versions: Default::default(),
        reason: None,
        activation: None,
    };
    let response = serde_json::to_value(&operation).map_err(|_| ApiError::unavailable())?;
    let mut tx = transaction(&state.store.pool).await?;
    let expires = if let Some(admission) = &admission {
        let expires = admission
            .caller
            .recheck_locked(state, admission.headers, &mut tx)
            .await
            .map_err(agent_error)?;
        grant_store::lock_operation(&mut tx, request.operation_id).await?;
        let legacy: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM agent_graph_operations WHERE operation_id=$1)",
        )
        .bind(request.operation_id)
        .fetch_one(&mut *tx)
        .await?;
        if legacy {
            return Err(ApiError::for_status(StatusCode::CONFLICT));
        }
        Some(expires)
    } else {
        None
    };
    let inserted=sqlx::query("INSERT INTO dashboard_operations(operation_id,principal_id,kind,scope,request_digest,disposition,response,created_at,updated_at) VALUES($1,$2,$3,$4,$5,'pending',$6,$7,$7) ON CONFLICT(operation_id) DO NOTHING")
        .bind(request.operation_id).bind(principal).bind(&request.kind).bind(serde_json::json!(scope)).bind(&digest).bind(response).bind(now).execute(&mut *tx).await?.rows_affected()==1;
    if inserted {
        if request.kind == "task.retry"
            && let Scope::Project { project_id } = scope
        {
            aq_machine_control::owner_retry::admit(
                &mut tx,
                request.operation_id,
                project_id,
                &request.subject.id,
                serde_json::to_value(request).map_err(|_| ApiError::invalid())?,
                now,
            )
            .await?;
        }
        if let Some(reservation) = admission.and_then(|a| a.reservation) {
            if principal != reservation.ids.attempt_id
                || scope
                    != (Scope::Project {
                        project_id: reservation.ids.project_id,
                    })
            {
                return Err(ApiError::invalid());
            }
            grant_store::reserve(
                &mut tx,
                request.operation_id,
                reservation,
                now.timestamp_millis(),
            )
            .await
            .map_err(|err| match err {
                ReserveError::QuotaExceeded => {
                    agent_error(aq_agent_grants::RefusalCode::QuotaExceeded)
                }
                ReserveError::Conflict => ApiError::for_status(StatusCode::CONFLICT),
                ReserveError::Store(error) => ApiError::from(error),
            })?;
        }
        if expires.is_some_and(|expires| expires <= crate::execution::api::now(state)) {
            return Err(agent_error(aq_agent_grants::RefusalCode::SessionExpired));
        }
        // An ambiguous commit is never a dispatch license. Caller retains the
        // original ID and queries again; a committed row remains Pending.
        tx.commit()
            .await
            .map_err(|_| ApiError::unavailable().operation(request.operation_id))?;
        return Ok((operation, true));
    }
    let row=sqlx::query("SELECT operation_id,principal_id,request_digest,response,retired_at FROM dashboard_operations WHERE operation_id=$1")
        .bind(request.operation_id).fetch_one(&mut *tx).await?;
    if row.try_get::<Uuid, _>("principal_id")? != principal {
        return Err(ApiError::for_status(StatusCode::NOT_FOUND));
    }
    if row.try_get::<String, _>("request_digest")? != digest {
        return Err(ApiError::for_status(StatusCode::CONFLICT).operation(request.operation_id));
    }
    Ok((retained(&row, principal)?, false))
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
    let rows=sqlx::query("UPDATE dashboard_operations SET response=$3,disposition=$4,updated_at=GREATEST(updated_at,$5) WHERE operation_id=$1 AND principal_id=$2 AND disposition IN ('pending','unknown') AND retired_at IS NULL")
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
    let rows=sqlx::query("UPDATE dashboard_operations SET response=NULL,retired_at=$1 WHERE retired_at IS NULL AND disposition IN ('confirmed','conflict','rejected') AND updated_at <= $1-interval '90 days'")
        .bind(now).execute(&mut *tx).await?.rows_affected();
    tx.commit().await?;
    Ok(rows)
}
