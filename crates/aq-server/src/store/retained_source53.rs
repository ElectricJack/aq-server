//! Exact retained-source sharing guard; original export/link guards stay intact.
use super::{Result, StoreError};
use sqlx::{Postgres, Transaction};
const MIGRATION: &str = include_str!("../../migrations/0053_replacement_retained_source.sql");
/// Format 95 replaces the guard body (never-granted successors no longer block).
const MIGRATION95: &str =
    include_str!("../../migrations/0095_replacement_source_after_never_granted.sql");
fn expected_body(supported: u32) -> Option<&'static str> {
    if supported >= 95 {
        MIGRATION95
    } else {
        MIGRATION
    }
    .split_once("AS $$")
    .and_then(|(_, tail)| tail.split_once("$$;"))
    .map(|(body, _)| body)
}
pub(super) async fn validate(tx: &mut Transaction<'_, Postgres>, supported: u32) -> Result<()> {
    let expected = expected_body(supported);
    let actual: Option<String> = sqlx::query_scalar("SELECT p.prosrc FROM pg_proc p JOIN pg_language l ON l.oid=p.prolang WHERE p.oid=to_regprocedure('profiled_replacement_source_guard()') AND NOT p.prosecdef AND p.proconfig IS NULL AND l.lanname='plpgsql'")
        .fetch_optional(&mut **tx).await?;
    let complete: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_trigger WHERE tgrelid=to_regclass('profiled_launch_source_exports') AND tgname='profiled_replacement_source_guard' AND NOT tgisinternal AND tgenabled='O' AND tgtype=7 AND tgfoid=to_regprocedure('profiled_replacement_source_guard()')) AND NOT EXISTS(SELECT 1 FROM pg_constraint WHERE conrelid IN (to_regclass('profiled_launch_source_exports'),to_regclass('source_bases')) AND conname IN ('profiled_launch_source_exports_operation_id_key','source_bases_operation_id_key')) AND EXISTS(SELECT 1 FROM pg_index WHERE indexrelid=to_regclass('source_bases_legacy_operation') AND indisunique AND indisvalid AND pg_get_expr(indpred,indrelid)='(profiled_export_operation_id IS NULL)')")
        .fetch_one(&mut **tx).await?;
    if expected.is_none() || actual.as_deref() != expected || !complete {
        return Err(StoreError::Corrupt(
            "retained source sharing storage drift".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_body_follows_frozen_migration_boundaries() {
        let name = "profiled_replacement_source_guard";
        let mut expected = None;
        for (version, sql) in super::super::MIGRATIONS {
            if let Some(body) = aq_profiled_state::sql_function_body(sql, name) {
                expected = Some(body);
            }
            if *version >= 53 {
                assert!(expected.is_some(), "guard missing at format {version}");
                assert_eq!(expected_body(*version as u32), expected, "format {version}");
            }
        }
    }
}
