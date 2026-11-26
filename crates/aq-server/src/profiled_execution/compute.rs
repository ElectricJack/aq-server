//! Typed historical stop proofs gate compute reuse without releasing task ownership.
use crate::{
    error::AppError,
    execution::{
        authority::hot::HotState,
        profiled::{Status, Stop},
    },
};
use aq_admission::LaunchBinding;
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

enum Occupancy {
    Busy,
    LostRelease,
    Stopped(serde_json::Value),
}

/// Caller owns the current enrolled-node lock. Bound work and fail closed when
/// history is unavailable; never infer stop from `holds_task` or status alone.
pub(crate) async fn busy_locked(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
) -> Result<bool, AppError> {
    let rows = sqlx::query("SELECT a.attempt_id,a.kind,l.launch_binding FROM execution_attempts a LEFT JOIN profiled_launches l USING(attempt_id) WHERE a.node_id=$1 AND a.holds_task ORDER BY a.attempt_id LIMIT 1025 FOR UPDATE OF a")
        .bind(node).fetch_all(&mut **tx).await.map_err(|_| AppError::Internal)?;
    if rows.len() > 1024 {
        return Ok(true);
    }
    let mut pins = serde_json::Map::new();
    let mut busy = false;
    for row in rows {
        match occupancy_locked(tx, node, &row).await? {
            Occupancy::Busy => busy = true,
            Occupancy::LostRelease => {}
            Occupancy::Stopped(pin) => {
                pins.insert(row.get::<Uuid, _>("attempt_id").to_string(), pin);
            }
        }
    }
    // Transaction-local witness for the incarnation SQL guard, never persisted
    // authority. SQL independently joins every exact immutable stopped owner.
    sqlx::query("SELECT set_config('aq.compute_stopped',$1,true)")
        .bind(serde_json::Value::Object(pins).to_string())
        .execute(&mut **tx)
        .await
        .map_err(|_| AppError::Internal)?;
    Ok(busy)
}

async fn occupancy_locked(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    row: &sqlx::postgres::PgRow,
) -> Result<Occupancy, AppError> {
    if row.get::<String, _>("kind") != "profiled_v1" {
        return Ok(Occupancy::Busy);
    }
    let id: Uuid = row.get("attempt_id");
    let Some(binding) = row.get::<Option<serde_json::Value>, _>("launch_binding") else {
        return Ok(Occupancy::Busy);
    };
    let binding: LaunchBinding = serde_json::from_value(binding)
        .map_err(|_| AppError::Conflict("compute history binding unavailable".into()))?;
    let hot: Option<serde_json::Value> = sqlx::query_scalar(
        "SELECT state FROM profiled_authority_hot WHERE attempt_id=$1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|_| AppError::Internal)?;
    let Some(hot) = hot else {
        return Ok(Occupancy::Busy);
    };
    let hot: HotState = serde_json::from_value(hot)
        .map_err(|_| AppError::Conflict("compute history unavailable".into()))?;
    hot.validate_against(&binding)
        .map_err(|_| AppError::Conflict("compute history invalid".into()))?;
    let (Status::Finished, Stop::Stopped { receipt }, Some(grant)) =
        (hot.status, &hot.stop, &hot.grant)
    else {
        return Ok(Occupancy::Busy);
    };
    // A lost release frees compute while B4 can retain the task.
    if receipt.id.as_str().starts_with("aq.lost-attempt-release/") {
        return Ok(Occupancy::LostRelease);
    }
    let pin =
        super::results::compute_stop_locked(tx, node, &binding, grant.grant_id, receipt).await?;
    let valid: bool = sqlx::query_scalar("SELECT profiled_compute_stopped_valid($1,$2)")
        .bind(id)
        .bind(node)
        .fetch_one(&mut **tx)
        .await
        .map_err(|_| AppError::Internal)?;
    if !valid {
        return Ok(Occupancy::Busy);
    }
    Ok(Occupancy::Stopped(pin))
}

/// Validate every stopped occupant, retaining witnesses even when a sibling is unknown.
pub(crate) async fn free_locked(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    slot: u32,
) -> Result<bool, AppError> {
    let busy = busy_locked(tx, node).await?;
    if !aq_machine_control::slots::available(tx)
        .await
        .map_err(crate::machine_api::mapped)?
    {
        return Ok(slot == 1 && !busy);
    }
    aq_machine_control::slots::poll_free(tx, node, slot)
        .await
        .map_err(crate::machine_api::mapped)
}

pub(crate) fn manual_witness<'a, 'c>(
    tx: &'a mut Transaction<'c, Postgres>,
    node: Uuid,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = aq_manual_sessions::Result<()>> + Send + 'a>>
{
    Box::pin(async move {
        busy_locked(tx, node).await.map(|_| ()).map_err(|_| {
            aq_manual_sessions::Error(
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                "Compute history is unavailable",
            )
        })
    })
}
