//! Fair multi-project scheduling (worker spec §8.2): configured priority plus
//! aging, with a hard starvation bound. Ordering is pure; the last-service
//! record is the only durable input.
use crate::error::AppError;
use std::collections::BTreeMap;
use uuid::Uuid;

pub use aq_profiled_state::fairness::{Candidate, order};

/// First store format carrying `profiled_project_schedule` (migration 52).
pub const SCHEDULE_FORMAT: u32 = 52;

/// Whether this store records fair-share service. A store held at an earlier
/// migration prefix has no schedule table, so it schedules every project as
/// never served and records nothing, instead of failing the whole poll.
pub async fn recorded(pool: &sqlx::PgPool) -> Result<bool, AppError> {
    let version: String =
        sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
            .fetch_one(pool)
            .await
            .map_err(|_| AppError::Internal)?;
    Ok(version.parse::<u32>().map_err(|_| AppError::Internal)? >= SCHEDULE_FORMAT)
}

pub async fn served(pool: &sqlx::PgPool) -> Result<BTreeMap<Uuid, u64>, AppError> {
    let rows: Vec<(Uuid, i64)> =
        sqlx::query_as("SELECT project_id,last_served_at_ms FROM profiled_project_schedule")
            .fetch_all(pool)
            .await
            .map_err(|_| AppError::Internal)?;
    Ok(rows
        .into_iter()
        .filter_map(|(id, at)| u64::try_from(at).ok().map(|at| (id, at)))
        .collect())
}

pub async fn record(pool: &sqlx::PgPool, project: Uuid, now: u64) -> Result<(), AppError> {
    sqlx::query("INSERT INTO profiled_project_schedule(project_id,last_served_at_ms,served_count) VALUES($1,$2,1) ON CONFLICT(project_id) DO UPDATE SET last_served_at_ms=GREATEST(profiled_project_schedule.last_served_at_ms,EXCLUDED.last_served_at_ms),served_count=profiled_project_schedule.served_count+1")
        .bind(project)
        .bind(i64::try_from(now).map_err(|_| AppError::Internal)?)
        .execute(pool)
        .await
        .map_err(|_| AppError::Internal)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn service_record_never_moves_backwards() {
        let db = crate::test_support::TestDb::create().await;
        let store = db.store().await;
        let project = Uuid::new_v4();
        record(&store.pool, project, 20).await.unwrap();
        record(&store.pool, project, 10).await.unwrap();
        assert_eq!(
            served(&store.pool).await.unwrap(),
            BTreeMap::from([(project, 20)])
        );
    }
}
