//! Durable operation identity and retained responses. No effects are performed
//! inside a journal transaction, and a duplicate admission never dispatches.
use super::*;
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
pub(super) struct Lookup {
    pub kind: String,
    pub scope: Scope,
    pub operation: Option<Operation>,
}
pub(super) async fn lookup_for_authorization(
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
    pool: &PgPool,
    principal: Uuid,
    request: &ActionRequest,
    scope: Scope,
    digest: String,
    now: DateTime<Utc>,
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
    };
    let response = serde_json::to_value(&operation).map_err(|_| ApiError::unavailable())?;
    let mut tx = transaction(pool).await?;
    let inserted=sqlx::query("INSERT INTO dashboard_operations(operation_id,principal_id,kind,scope,request_digest,disposition,response,created_at,updated_at) VALUES($1,$2,$3,$4,$5,'pending',$6,$7,$7) ON CONFLICT(operation_id) DO NOTHING")
        .bind(request.operation_id).bind(principal).bind(&request.kind).bind(serde_json::json!(scope)).bind(&digest).bind(response).bind(now).execute(&mut *tx).await?.rows_affected()==1;
    if inserted {
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
