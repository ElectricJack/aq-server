//! Leaf43 cancellation owners; older migration profiles keep their exact guards.
use super::{Result, StoreError};
use sqlx::{Postgres, Transaction};
fn corrupt() -> StoreError {
    StoreError::Corrupt("native cancellation43 guard drift".into())
}
pub(super) fn function_body(name: &str) -> Result<&'static str> {
    aq_profiled_state::sql_function_body(
        include_str!("../../migrations/0043_native_cancellation.sql"),
        name,
    )
    .ok_or_else(corrupt)
}
pub(super) async fn validate(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    for (name, signature, language, set) in [
        (
            "profiled_close_ungranted",
            "uuid,uuid,bigint,uuid,uuid,boolean",
            "plpgsql",
            true,
        ),
        (
            "profiled_close_expired_ungranted",
            "uuid,uuid,bigint,uuid,uuid",
            "sql",
            true,
        ),
        ("profiled_cancelled_settlement_guard", "", "plpgsql", false),
        ("profiled_cancelled_absence_guard", "", "plpgsql", false),
    ] {
        let actual: Option<String> = sqlx::query_scalar("SELECT p.prosrc FROM pg_proc p JOIN pg_language l ON l.oid=p.prolang WHERE p.oid=to_regprocedure($1) AND NOT p.prosecdef AND p.proconfig IS NULL AND l.lanname=$2 AND p.prokind='f' AND p.proretset=$3 AND NOT p.proisstrict AND p.proparallel='u'")
            .bind(format!("{name}({signature})")).bind(language).bind(set).fetch_optional(&mut **tx).await?;
        if actual.as_deref() != Some(function_body(name)?) {
            return Err(corrupt());
        }
    }
    for (table, trigger, function, kind) in [
        (
            "profiled_cancellations",
            "profiled_cancellations_immutable",
            "profiled_sealed_row_guard",
            27,
        ),
        (
            "profiled_cancel_operations",
            "profiled_cancel_operations_immutable",
            "profiled_sealed_row_guard",
            27,
        ),
        (
            "profiled_cancelled_settlements",
            "profiled_cancelled_settlements_immutable",
            "profiled_sealed_row_guard",
            27,
        ),
        (
            "profiled_cancelled_settlements",
            "profiled_cancelled_settlement_guard",
            "profiled_cancelled_settlement_guard",
            7,
        ),
        (
            "profiled_never_granted_closures",
            "profiled_cancelled_absence_guard",
            "profiled_cancelled_absence_guard",
            7,
        ),
    ] {
        let valid:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_trigger WHERE tgrelid=to_regclass($1) AND tgname=$2 AND NOT tgisinternal AND tgenabled='O' AND tgtype=$4 AND tgnargs=0 AND tgqual IS NULL AND tgfoid=to_regprocedure($3))")
            .bind(table).bind(trigger).bind(format!("{function}()")).bind(kind as i16).fetch_one(&mut **tx).await?;
        if !valid {
            return Err(corrupt());
        }
    }
    // Primary/foreign keys are the retained identity joins, including the
    // one-cancellation and one-proven-settlement-per-attempt constraints.
    let valid:bool=sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM (VALUES ('profiled_cancellations',1,1),('profiled_cancel_operations',1,1),('profiled_cancelled_settlements',1,2)) e(name,pk,fk) WHERE (SELECT count(*) FROM pg_constraint WHERE conrelid=to_regclass(e.name) AND contype='p' AND convalidated)<>e.pk OR (SELECT count(*) FROM pg_constraint WHERE conrelid=to_regclass(e.name) AND contype='f' AND convalidated)<>e.fk)")
        .fetch_one(&mut **tx).await?;
    if !valid {
        return Err(corrupt());
    }
    Ok(())
}
