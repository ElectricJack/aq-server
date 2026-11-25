use serde_json::Value;
use sqlx::PgPool;

pub(super) async fn diagnostics(pool: &PgPool, cutoff: i64) -> anyhow::Result<Value> {
    let start = cutoff.saturating_sub(crate::scheduling_diagnostics::WEEK);
    Ok(sqlx::query_scalar(include_str!("scheduling.sql"))
        .bind(start)
        .bind(cutoff)
        .fetch_one(pool)
        .await?)
}
