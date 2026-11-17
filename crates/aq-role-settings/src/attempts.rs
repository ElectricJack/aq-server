//! What an attempt ran with (spec 0014 §7.1): a summary of a few hundred
//! bytes on the attempt's own record, which lasts exactly as long as that
//! record, and a pin that keeps the snapshot it names while the attempt
//! holds its task.
use crate::{Result, store};
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;
use uuid::Uuid;

/// The summary every task and attempt read shows as `ran_with`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RanWith {
    pub role: String,
    /// The intelligence profile: the level the router assigned, or the
    /// project's default.
    pub level: String,
    pub provider: String,
    pub model: String,
    pub reasoning: String,
    pub harness: String,
    pub execution_profile: String,
    /// The snapshot the attempt launched from. `None` (JSON null) for an
    /// attempt recorded before roles and profiles: its summary was filled
    /// from its launch record and no snapshot of that time exists.
    pub snapshot_digest: Option<String>,
}

/// Store the attempt's summary and pin the snapshot it names. Call it in the
/// transaction that creates the attempt.
pub async fn record(conn: &mut PgConnection, attempt_id: Uuid, ran_with: &RanWith) -> Result<()> {
    let summary = serde_json::to_value(ran_with).map_err(|_| crate::Error::Internal)?;
    sqlx::query(
        "INSERT INTO attempt_ran_with(attempt_id,ran_with) VALUES($1,$2)
         ON CONFLICT(attempt_id) DO UPDATE SET ran_with=EXCLUDED.ran_with",
    )
    .bind(attempt_id)
    .bind(summary)
    .execute(&mut *conn)
    .await?;
    // Attempts that finished since the last one no longer keep a snapshot.
    store::release_finished_pins(&mut *conn).await?;
    match &ran_with.snapshot_digest {
        Some(digest) => store::pin(conn, digest, attempt_id).await,
        None => Ok(()),
    }
}
/// The summary of an attempt, or `None` for an attempt from before roles
/// (it has none) or one that does not exist.
pub async fn ran_with(conn: &mut PgConnection, attempt_id: Uuid) -> Result<Option<RanWith>> {
    let summary: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT ran_with FROM attempt_ran_with WHERE attempt_id=$1")
            .bind(attempt_id)
            .fetch_optional(conn)
            .await?;
    Ok(summary.and_then(|value| serde_json::from_value(value).ok()))
}
/// [`ran_with`] in the caller's own type of the same serialized shape (the
/// dashboard contract's), so every read shows one shape. Only a database
/// failure is an error.
pub async fn ran_with_as<T: serde::de::DeserializeOwned>(
    conn: &mut PgConnection,
    attempt_id: Uuid,
) -> std::result::Result<Option<T>, sqlx::Error> {
    let summary: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT ran_with FROM attempt_ran_with WHERE attempt_id=$1")
            .bind(attempt_id)
            .fetch_optional(conn)
            .await?;
    Ok(summary.and_then(|value| serde_json::from_value(value).ok()))
}
/// The summaries of the attempts in `rows` (their `attempt_id` column), for
/// a read that lists attempts. A store without the summaries (a fixture from
/// before roles) and a failed read both show none: the summary is an
/// optional part of those reads.
pub async fn ran_with_rows(
    pool: &sqlx::PgPool,
    rows: &[sqlx::postgres::PgRow],
) -> std::collections::BTreeMap<Uuid, RanWith> {
    use sqlx::Row;
    let attempts: Vec<Uuid> = (rows.iter())
        .filter_map(|row| row.try_get::<Uuid, _>("attempt_id").ok())
        .collect();
    match pool.acquire().await {
        Ok(mut conn) => ran_with_many(&mut conn, &attempts)
            .await
            .unwrap_or_default(),
        Err(_) => Default::default(),
    }
}
/// The summaries of several attempts, keyed by attempt.
pub async fn ran_with_many(
    conn: &mut PgConnection,
    attempts: &[Uuid],
) -> Result<std::collections::BTreeMap<Uuid, RanWith>> {
    let rows: Vec<(Uuid, serde_json::Value)> =
        sqlx::query_as("SELECT attempt_id,ran_with FROM attempt_ran_with WHERE attempt_id=ANY($1)")
            .bind(attempts)
            .fetch_all(conn)
            .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(id, value)| Some((id, serde_json::from_value(value).ok()?)))
        .collect())
}
/// The attempt's record is being removed: its summary goes with it, and its
/// snapshot is released (and deleted when nothing else keeps it). Call it in
/// the transaction that deletes the attempt's record.
pub async fn forget(conn: &mut PgConnection, attempt_id: Uuid) -> Result<()> {
    sqlx::query("DELETE FROM attempt_ran_with WHERE attempt_id=$1")
        .bind(attempt_id)
        .execute(&mut *conn)
        .await?;
    store::unpin(conn, attempt_id).await
}
