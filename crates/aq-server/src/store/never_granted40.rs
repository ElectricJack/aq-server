//! Exact leaf40 absence-owner functions; previous composed39 guards remain exact.
use super::{Result, StoreError};
use sqlx::{Postgres, Transaction};
fn corrupt() -> StoreError {
    StoreError::Corrupt("phase never-granted owner40 guard drift".into())
}
pub(super) fn function_body(name: &str) -> Result<&'static str> {
    aq_profiled_state::sql_function_body(
        include_str!("../../migrations/0040_phase_never_granted.sql"),
        name,
    )
    .ok_or_else(corrupt)
}
pub(super) async fn validate(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    validate_composed(tx, 40).await
}
pub(super) async fn validate_compute_reuse(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    validate_composed(tx, 41).await
}
pub(super) async fn validate_native_cancellation(
    tx: &mut Transaction<'_, Postgres>,
    supported: u32,
) -> Result<()> {
    validate_composed(tx, supported).await
}
async fn validate_composed(tx: &mut Transaction<'_, Postgres>, supported: u32) -> Result<()> {
    let overrides = super::replacement52::guard_overrides(supported)?;
    for (name, signature) in [
        ("profiled_no_execution_effects", "uuid"),
        (
            "profiled_close_expired_ungranted",
            "uuid,uuid,bigint,uuid,uuid",
        ),
        ("profiled_phase_never_granted_fence", ""),
        ("settlement_claim_guard", ""),
    ] {
        let tail = if let Some((_, body)) = overrides.iter().find(|(guard, _)| *guard == name) {
            *body
        } else {
            expected_body(name, supported)?
        };
        let language = if name == "profiled_no_execution_effects"
            || (supported >= 43 && name == "profiled_close_expired_ungranted")
        {
            "sql"
        } else {
            "plpgsql"
        };
        let actual: Option<String> = sqlx::query_scalar("SELECT p.prosrc FROM pg_proc p JOIN pg_language l ON l.oid=p.prolang WHERE p.oid=to_regprocedure($1) AND NOT p.prosecdef AND p.proconfig IS NULL AND l.lanname=$2")
            .bind(format!("{name}({signature})"))
            .bind(language)
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

fn expected_body(name: &str, supported: u32) -> Result<&'static str> {
    if name == "profiled_close_expired_ungranted" {
        if supported >= 43 {
            return super::cancellation43::function_body(name);
        }
        if supported >= 41 {
            return super::compute41::function_body(name);
        }
    }
    function_body(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expiry_guard_body_follows_frozen_migration_boundaries() {
        let name = "profiled_close_expired_ungranted";
        let mut expected = None;
        for (version, sql) in super::super::MIGRATIONS {
            if let Some(body) = aq_profiled_state::sql_function_body(sql, name) {
                expected = Some(body);
            }
            if *version >= 40 {
                assert_eq!(
                    Some(expected_body(name, *version as u32).unwrap()),
                    expected,
                    "format {version}"
                );
            }
        }
    }
}
