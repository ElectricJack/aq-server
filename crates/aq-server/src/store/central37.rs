//! Frozen reservation-only storage. Startup verifies rather than repairs.
use super::{Result, StoreError};
use sqlx::{Postgres, Row, Transaction};

#[path = "central37_data.rs"]
mod data;
#[path = "central37_schema.rs"]
mod schema;

pub(super) const MIGRATION: &str = include_str!("../../migrations/0037_central_reservations.sql");
pub(super) fn corrupt() -> StoreError {
    StoreError::Corrupt(
        "central reservation storage/guards or retained proofs are inconsistent".into(),
    )
}
pub(super) fn function_body(name: &str, version: u32) -> Result<&'static str> {
    let latest = include_str!("../../migrations/0049_supervisor_reservation_consumption.sql");
    // bold-stone-51: agent-sent supervisor mail extends the source proof.
    let agent = include_str!("../../migrations/0084_agent_supervisor_mail.sql");
    let source = if version >= 84 && agent.contains(&format!("FUNCTION {name}(")) {
        agent
    } else if version >= 49 && latest.contains(&format!("FUNCTION {name}(")) {
        latest
    } else {
        MIGRATION
    };
    aq_profiled_state::sql_function_body(source, name).ok_or_else(corrupt)
}

pub(super) async fn central37_storage_guard(
    tx: &mut Transaction<'_, Postgres>,
    version: u32,
) -> Result<()> {
    schema::validate(tx, version).await?;
    data::validate(tx, version).await
}

async fn functions(tx: &mut Transaction<'_, Postgres>, version: u32) -> Result<()> {
    for (name, args, returns, language, volatility) in [
        ("central_global_mailbox_key", "", "text", "sql", "s"),
        (
            "central_global_mail_source_valid",
            "uuid",
            "boolean",
            "sql",
            "s",
        ),
        ("guard_central_global_mail", "", "trigger", "plpgsql", "v"),
        (
            "guard_central_reservation_actor",
            "",
            "trigger",
            "plpgsql",
            "v",
        ),
        (
            "guard_central_reservation_state",
            "",
            "trigger",
            "plpgsql",
            "v",
        ),
        (
            "guard_central_reservation_immutable",
            "",
            "trigger",
            "plpgsql",
            "v",
        ),
        (
            "check_central_reservation_consistency",
            "",
            "trigger",
            "plpgsql",
            "v",
        ),
    ]
    .into_iter()
    .chain(
        (version >= 49)
            .then_some([
                (
                    "central_supervisor_turn_complete",
                    "uuid",
                    "boolean",
                    "sql",
                    "s",
                ),
                (
                    "consume_central_supervisor_turn",
                    "",
                    "trigger",
                    "plpgsql",
                    "v",
                ),
            ])
            .into_iter()
            .flatten(),
    ) {
        let row = sqlx::query("SELECT p.prosrc,l.lanname,p.provolatile::text,p.prokind::text,p.prosecdef,p.proretset,p.proisstrict,p.proparallel::text,p.proconfig,format_type(p.prorettype,NULL) AS returns FROM pg_proc p JOIN pg_language l ON l.oid=p.prolang WHERE p.oid=to_regprocedure($1)")
            .bind(format!("{name}({args})")).fetch_optional(&mut **tx).await?;
        let Some(row) = row else {
            return Err(corrupt());
        };
        if row.get::<String, _>("prosrc") != function_body(name, version)?
            || row.get::<String, _>("lanname") != language
            || row.get::<String, _>("provolatile") != volatility
            || row.get::<String, _>("prokind") != "f"
            || row.get::<bool, _>("prosecdef")
            || row.get::<bool, _>("proretset")
            || row.get::<bool, _>("proisstrict")
            || row.get::<String, _>("proparallel") != "u"
            || row.get::<Option<Vec<String>>, _>("proconfig").is_some()
            || row.get::<String, _>("returns") != returns
        {
            return Err(corrupt());
        }
    }
    Ok(())
}
