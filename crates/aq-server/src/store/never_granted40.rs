//! Exact leaf40 absence-owner functions; previous composed39 guards remain exact.
use super::{Result, StoreError};
use sqlx::{Postgres, Transaction};
fn corrupt() -> StoreError {
    StoreError::Corrupt("phase never-granted owner40 guard drift".into())
}
pub(super) fn function_body(name: &str) -> Result<&'static str> {
    include_str!("../../migrations/0040_phase_never_granted.sql")
        .split_once(&format!("FUNCTION {name}("))
        .and_then(|(_, tail)| tail.split_once("AS $$"))
        .and_then(|(_, tail)| tail.split_once("$$;"))
        .map(|(body, _)| body)
        .ok_or_else(corrupt)
}
pub(super) async fn validate(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    validate_composed(tx, false, false, 40).await
}
pub(super) async fn validate_compute_reuse(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    validate_composed(tx, true, false, 41).await
}
pub(super) async fn validate_native_cancellation(
    tx: &mut Transaction<'_, Postgres>,
    supported: u32,
) -> Result<()> {
    validate_composed(tx, true, true, supported).await
}
async fn validate_composed(
    tx: &mut Transaction<'_, Postgres>,
    compute_reuse: bool,
    cancellation: bool,
    supported: u32,
) -> Result<()> {
    let sql = include_str!("../../migrations/0040_phase_never_granted.sql");
    for (name, signature) in [
        ("profiled_no_execution_effects", "uuid"),
        (
            "profiled_close_expired_ungranted",
            "uuid,uuid,bigint,uuid,uuid",
        ),
        ("profiled_phase_never_granted_fence", ""),
        ("settlement_claim_guard", ""),
    ] {
        let replaced = super::replacement52::guard_overrides(supported)?
            .into_iter()
            .find(|(guard, _)| *guard == name);
        let tail = if let Some((_, body)) = replaced {
            body
        } else if cancellation && name == "profiled_close_expired_ungranted" {
            super::cancellation43::function_body(name)?
        } else if compute_reuse && name == "profiled_close_expired_ungranted" {
            super::compute41::function_body(name)?
        } else {
            sql.split_once(&format!("FUNCTION {name}("))
                .and_then(|(_, tail)| tail.split_once("AS $$"))
                .and_then(|(_, tail)| tail.split_once("$$;"))
                .map(|(body, _)| body)
                .ok_or_else(corrupt)?
        };
        let actual: Option<String> = sqlx::query_scalar("SELECT p.prosrc FROM pg_proc p JOIN pg_language l ON l.oid=p.prolang WHERE p.oid=to_regprocedure($1) AND NOT p.prosecdef AND p.proconfig IS NULL AND l.lanname=$2")
            .bind(format!("{name}({signature})"))
            .bind(if name == "profiled_no_execution_effects" || (cancellation && name == "profiled_close_expired_ungranted") { "sql" } else { "plpgsql" })
            .fetch_optional(&mut **tx).await?;
        if actual.as_deref() != Some(tail) {
            return Err(corrupt());
        }
    }
    let guarded:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_trigger WHERE tgrelid=to_regclass('profiled_phase_execution_grants') AND tgname='profiled_phase_never_granted_fence' AND NOT tgisinternal AND tgenabled='O' AND tgtype=7 AND tgfoid=to_regprocedure('profiled_phase_never_granted_fence()'))")
        .fetch_one(&mut **tx).await?;
    if !guarded {
        return Err(corrupt());
    }
    Ok(())
}
