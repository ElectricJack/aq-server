//! Exact retained-source sharing guard; original export/link guards stay intact.
use super::{Result, StoreError};
use sqlx::{Postgres, Transaction};
const MIGRATION: &str = include_str!("../../migrations/0053_replacement_retained_source.sql");
pub(super) async fn validate(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    let expected = MIGRATION
        .split_once("AS $$")
        .and_then(|(_, tail)| tail.split_once("$$;"))
        .map(|(body, _)| body);
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
