//! Exact new41 leaf guards; prior38/39/40 catalogs retain their old profiles.
use super::{Result, StoreError};
use sqlx::{Postgres, Transaction};
fn corrupt() -> StoreError {
    StoreError::Corrupt("native compute41 guard drift".into())
}
const LOST: &str = include_str!("../../migrations/0102_lost_attempt_releases.sql");
pub(super) fn function_body(name: &str) -> Result<&'static str> {
    aq_profiled_state::sql_function_body(
        include_str!("../../migrations/0041_native_compute_occupancy.sql"),
        name,
    )
    .ok_or_else(corrupt)
}
pub(super) async fn validate(tx: &mut Transaction<'_, Postgres>, supported: u32) -> Result<()> {
    for (name, signature, language, volatility, result) in [
        (
            "profiled_compute_stopped_valid",
            "uuid,uuid",
            "sql",
            "s",
            "boolean",
        ),
        ("native_incarnation_guard", "", "plpgsql", "v", "trigger"),
    ] {
        let actual: Option<String> = sqlx::query_scalar("SELECT p.prosrc FROM pg_proc p JOIN pg_language l ON l.oid=p.prolang WHERE p.oid=to_regprocedure($1) AND NOT p.prosecdef AND p.proconfig IS NULL AND l.lanname=$2 AND p.provolatile::TEXT=$3 AND p.prorettype=to_regtype($4) AND p.prokind='f' AND NOT p.proretset AND NOT p.proisstrict AND p.proparallel='u'")
            .bind(format!("{name}({signature})")).bind(language).bind(volatility).bind(result)
            .fetch_optional(&mut **tx).await?;
        // Format 102 admits lost-attempt releases past the incarnation guard (D2 ruling C).
        let body = match (supported >= 102, name) {
            (true, "native_incarnation_guard") => {
                aq_profiled_state::sql_function_body(LOST, name).ok_or_else(corrupt)?
            }
            _ => function_body(name)?,
        };
        if actual.as_deref() != Some(body) {
            return Err(corrupt());
        }
    }
    let guarded: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_trigger WHERE tgrelid=to_regclass('execution_nodes') AND tgname='native_incarnation_guard' AND NOT tgisinternal AND tgenabled='O' AND tgtype=23 AND tgnargs=0 AND tgqual IS NULL AND tgfoid=to_regprocedure('native_incarnation_guard()'))")
        .fetch_one(&mut **tx).await?;
    if !guarded {
        return Err(corrupt());
    }
    Ok(())
}
