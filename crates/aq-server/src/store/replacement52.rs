//! Exact format-52 replacement-release and fair-schedule storage.
use super::{Result, StoreError};
use sqlx::{Postgres, Transaction};
const MIGRATION: &str = include_str!("../../migrations/0052_profiled_replacement_fairness.sql");
/// Claim guards that format 52 replaces; earlier attestations defer to these.
const GUARDS: [&str; 2] = [
    "profiled_historical_snapshot_guard",
    "settlement_claim_guard",
];
fn corrupt() -> StoreError {
    StoreError::Corrupt("replacement52 release/schedule storage drift".into())
}
pub(super) fn function_body(name: &str) -> Result<&'static str> {
    aq_profiled_state::sql_function_body(MIGRATION, name).ok_or_else(corrupt)
}
pub(super) fn guard_overrides(supported: u32) -> Result<Vec<(&'static str, &'static str)>> {
    if supported < 52 {
        return Ok(Vec::new());
    }
    if supported >= 66 {
        return super::stopped66::guard_overrides(supported);
    }
    if supported >= 59 {
        return super::auto56::guard_overrides(supported);
    }
    GUARDS
        .into_iter()
        .map(|name| function_body(name).map(|body| (name, body)))
        .collect()
}
pub(super) async fn validate(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    let actual: Option<String> = sqlx::query_scalar("SELECT p.prosrc FROM pg_proc p JOIN pg_language l ON l.oid=p.prolang WHERE p.oid=to_regprocedure('profiled_replacement_release_guard()') AND NOT p.prosecdef AND p.proconfig IS NULL AND l.lanname='plpgsql'")
        .fetch_optional(&mut **tx)
        .await?;
    if actual.as_deref() != Some(function_body("profiled_replacement_release_guard")?) {
        return Err(corrupt());
    }
    let complete: bool = sqlx::query_scalar("SELECT to_regclass('profiled_replacement_releases') IS NOT NULL AND to_regclass('profiled_project_schedule') IS NOT NULL AND EXISTS(SELECT 1 FROM pg_trigger WHERE tgrelid=to_regclass('profiled_replacement_releases') AND tgname='profiled_replacement_release_guard' AND NOT tgisinternal AND tgenabled='O' AND tgtype=31 AND tgfoid=to_regprocedure('profiled_replacement_release_guard()'))")
        .fetch_one(&mut **tx)
        .await?;
    if !complete {
        return Err(corrupt());
    }
    Ok(())
}
