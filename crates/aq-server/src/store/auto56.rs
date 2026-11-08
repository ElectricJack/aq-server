//! Exact D3 format-59 auto-settlement owner and the two widened holds_task
//! release guards; prior catalogs substitute these bodies only at format >= 59.
use super::{Result, StoreError};
use sqlx::{Postgres, Transaction};
fn corrupt() -> StoreError {
    StoreError::Corrupt("auto settlement56 guard drift".into())
}
pub(super) fn function_body(name: &str) -> Result<&'static str> {
    aq_profiled_state::sql_function_body(
        include_str!("../../migrations/0059_auto_settlement.sql"),
        name,
    )
    .ok_or_else(corrupt)
}
/// Later formats replace these guards without changing earlier attestations.
pub(super) fn guard_overrides(supported: u32) -> Result<Vec<(&'static str, &'static str)>> {
    let mut guards = [
        "profiled_historical_snapshot_guard",
        "settlement_claim_guard",
    ]
    .into_iter()
    .map(|name| {
        if supported >= 65 && name == "profiled_historical_snapshot_guard" {
            completion65_body(name).map(|body| (name, body))
        } else {
            function_body(name).map(|body| (name, body))
        }
    })
    .collect::<Result<Vec<_>>>()?;
    if supported >= 65 {
        guards.push((
            "settlement_completion_guard",
            completion65_body("settlement_completion_guard")?,
        ));
    }
    Ok(guards)
}
fn completion65_body(name: &str) -> Result<&'static str> {
    aq_profiled_state::sql_function_body(
        include_str!("../../migrations/0065_settlement_completion_projection.sql"),
        name,
    )
    .ok_or_else(corrupt)
}
pub(super) async fn validate(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    let actual: Option<String> = sqlx::query_scalar("SELECT p.prosrc FROM pg_proc p JOIN pg_language l ON l.oid=p.prolang WHERE p.oid=to_regprocedure('profiled_auto_settlement_owner()') AND NOT p.prosecdef AND p.proconfig IS NULL AND l.lanname='plpgsql' AND p.prorettype=to_regtype('trigger')")
        .fetch_optional(&mut **tx).await?;
    if actual.as_deref() != Some(function_body("profiled_auto_settlement_owner")?) {
        return Err(corrupt());
    }
    let guarded: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_trigger WHERE tgrelid=to_regclass('profiled_auto_settlements') AND tgname='profiled_auto_settlement_owner' AND NOT tgisinternal AND tgenabled='O' AND tgtype=31 AND tgnargs=0 AND tgqual IS NULL AND tgfoid=to_regprocedure('profiled_auto_settlement_owner()')) AND to_regclass('profiled_check_evidence') IS NOT NULL AND to_regclass('profiled_delivery_evidence') IS NOT NULL")
        .fetch_one(&mut **tx).await?;
    if !guarded {
        return Err(corrupt());
    }
    Ok(())
}
