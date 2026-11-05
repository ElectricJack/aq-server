//! Schema-66 disposition storage and the two widened release guards.
use super::{Result, StoreError};
use sqlx::{Postgres, Transaction};
fn corrupt() -> StoreError {
    StoreError::Corrupt("stopped disposition66 storage drift".into())
}
fn function_body(name: &str) -> Result<&'static str> {
    include_str!("../../migrations/0066_stopped_dispositions.sql")
        .split_once(&format!("FUNCTION {name}("))
        .and_then(|(_, tail)| tail.split_once("AS $$"))
        .and_then(|(_, tail)| tail.split_once("$$;"))
        .map(|(body, _)| body)
        .ok_or_else(corrupt)
}
pub(super) fn guard_overrides(supported: u32) -> Result<Vec<(&'static str, &'static str)>> {
    let mut guards = super::auto56::guard_overrides(supported)?;
    for name in [
        "profiled_historical_snapshot_guard",
        "settlement_claim_guard",
    ] {
        guards.retain(|(existing, _)| *existing != name);
        guards.push((name, function_body(name)?));
    }
    Ok(guards)
}
pub(super) async fn validate(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    for name in [
        "profiled_stopped_disposition_guard",
        "profiled_stopped_disposition_committed",
    ] {
        let body: Option<String> = sqlx::query_scalar("SELECT p.prosrc FROM pg_proc p JOIN pg_language l ON l.oid=p.prolang WHERE p.oid=to_regprocedure($1) AND NOT p.prosecdef AND p.proconfig IS NULL AND l.lanname='plpgsql' AND p.prorettype=to_regtype('trigger')")
            .bind(format!("{name}()")).fetch_optional(&mut **tx).await?;
        if body.as_deref() != Some(function_body(name)?) {
            return Err(corrupt());
        }
        let guarded: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_trigger WHERE tgrelid=to_regclass('profiled_stopped_dispositions') AND tgname=$1 AND NOT tgisinternal AND tgenabled='O' AND tgtype=$2 AND tgdeferrable=$3 AND tginitdeferred=$3 AND tgnargs=0 AND tgqual IS NULL AND tgfoid=to_regprocedure($4))")
            .bind(name).bind(if name.ends_with("committed") {5i16} else {31i16})
            .bind(name.ends_with("committed")).bind(format!("{name}()"))
            .fetch_one(&mut **tx).await?;
        if !guarded {
            return Err(corrupt());
        }
    }
    Ok(())
}
