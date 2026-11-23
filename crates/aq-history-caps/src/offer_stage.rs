use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

/// Stores before migration 0167 have no offer stages; their assemblies stay unstaged.
async fn available(pool: &PgPool) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT to_regclass('admission_offer_stages') IS NOT NULL")
        .fetch_one(pool)
        .await
}

/// Opens the stage; `false` when the store predates offer stages.
pub async fn open(
    pool: &PgPool,
    stage: Uuid,
    attempt: Uuid,
    project: Uuid,
    now: u64,
) -> Result<bool, sqlx::Error> {
    if !available(pool).await? {
        return Ok(false);
    }
    sqlx::query("INSERT INTO admission_offer_stages(stage_id,attempt_id,project_id,expires_at_ms) VALUES ($1,$2,$3,$4)")
        .bind(stage).bind(attempt).bind(project).bind(super::ms(now.saturating_add(600_000)))
        .execute(pool).await?;
    Ok(true)
}

pub async fn pin(
    tx: &mut PgConnection,
    stage: Uuid,
    artifact: Uuid,
    at: u64,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT admission_offer_pin($1,$2,$3)")
        .bind(stage)
        .bind(artifact)
        .bind(super::ms(at))
        .fetch_one(tx)
        .await
}

/// Release capacity after all publication retries finish. Existing links and
/// any other assembly's pins retain their own charges.
pub async fn close(pool: &PgPool, stage: Uuid, at: u64) -> Result<(), sqlx::Error> {
    if !available(pool).await? {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    sqlx::query(
        "SELECT set_config('lock_timeout','500ms',true),set_config('statement_timeout','2s',true)",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query("WITH closed AS (UPDATE admission_offer_stages SET closed_at_ms=$2 WHERE stage_id=$1 AND closed_at_ms IS NULL RETURNING stage_id)
                 DELETE FROM admission_offer_stage_pins WHERE stage_id IN (SELECT stage_id FROM closed)")
        .bind(stage).bind(super::ms(at)).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}
