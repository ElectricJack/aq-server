//! The `playbook_sources` table: the built-in record is written by the
//! server at start, a registered one by its owner or a supervisor.
use crate::{Error, Result, SourceKind, SourceRecord, now_ms};
use aq_internal_work::Purpose;
use serde_json::Value;
use sqlx::{PgPool, Postgres, Row, Transaction, postgres::PgRow, types::Json};
use std::collections::BTreeSet;

const COLUMNS: &str =
    "id,kind,binding,scope,purpose,judgment_purpose,commands,events,capabilities,facts,revision";
const BINDING_KEY: &str = "playbook_sources_binding_key";

fn decode(row: &PgRow) -> Result<SourceRecord> {
    let kind: String = row.try_get("kind")?;
    let purpose: String = row.try_get("judgment_purpose")?;
    let revision: i64 = row.try_get("revision")?;
    let names = |column: &str| -> Result<BTreeSet<String>> {
        Ok(row.try_get::<Json<BTreeSet<String>>, _>(column)?.0)
    };
    Ok(SourceRecord {
        id: row.try_get("id")?,
        kind: SourceKind::parse(&kind).ok_or(Error::Corrupt)?,
        binding: row.try_get("binding")?,
        scope: row.try_get("scope")?,
        purpose: row.try_get("purpose")?,
        judgment_purpose: serde_json::from_value(Value::String(purpose))
            .map_err(|_| Error::Corrupt)?,
        commands: names("commands")?,
        events: names("events")?,
        capabilities: names("capabilities")?,
        facts: row.try_get::<Json<Vec<String>>, _>("facts")?.0,
        revision: u64::try_from(revision).map_err(|_| Error::Corrupt)?,
    })
}

fn purpose_text(purpose: Purpose) -> String {
    let value = serde_json::to_value(purpose).unwrap_or_default();
    value.as_str().unwrap_or_default().to_owned()
}

/// The source `id`, if there is one.
pub async fn get(pool: &PgPool, id: &str) -> Result<Option<SourceRecord>> {
    let row = sqlx::query(&format!(
        "SELECT {COLUMNS} FROM playbook_sources WHERE id=$1"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await?;
    row.as_ref().map(decode).transpose()
}

/// A registered source's stored Markdown and its record revision. `None` for
/// an unknown source and for the built-in one, whose Markdown the host keeps.
pub async fn stored_markdown(pool: &PgPool, id: &str) -> Result<Option<(String, u64)>> {
    let row: Option<(String, i64)> = sqlx::query_as(
        "SELECT markdown,revision FROM playbook_sources WHERE id=$1 AND markdown IS NOT NULL",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    row.map(|(text, revision)| Ok((text, u64::try_from(revision).map_err(|_| Error::Corrupt)?)))
        .transpose()
}

/// Writes the built-in `record` (the server calls it at start). Once it is
/// stored, the same record changes nothing; a different one (a newer server)
/// replaces it and takes the next revision.
pub async fn ensure_builtin(pool: &PgPool, record: &SourceRecord) -> Result<()> {
    if record.kind != SourceKind::Builtin {
        return Err(Error::Invalid(format!(
            "{} is not a built-in source",
            record.id
        )));
    }
    write(pool, record, None).await
}

/// Registers `record` with its Markdown, or, for a source already registered
/// under that id, replaces both and takes the next revision when either
/// changed. A binding another source uses, or the id of a built-in source, is
/// a conflict.
pub async fn register(pool: &PgPool, record: &SourceRecord, markdown: &str) -> Result<()> {
    if record.kind != SourceKind::Registered {
        return Err(Error::Invalid(format!(
            "{} is not a registered source",
            record.id
        )));
    }
    write(pool, record, Some(markdown)).await
}

async fn write(pool: &PgPool, record: &SourceRecord, markdown: Option<&str>) -> Result<()> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query(&format!(
        "SELECT {COLUMNS},markdown FROM playbook_sources WHERE id=$1 FOR UPDATE"
    ))
    .bind(&record.id)
    .fetch_optional(&mut *tx)
    .await?;
    let revision = match row {
        None => 1,
        Some(row) => {
            let stored = decode(&row)?;
            if stored.kind != record.kind {
                return Err(kind_taken(record));
            }
            let text: Option<String> = row.try_get("markdown")?;
            if stored.same_as(record) && text.as_deref() == markdown {
                return Ok(());
            }
            stored.revision + 1
        }
    };
    let used: Option<String> =
        sqlx::query_scalar("SELECT id FROM playbook_sources WHERE binding=$1 AND id<>$2")
            .bind(&record.binding)
            .bind(&record.id)
            .fetch_optional(&mut *tx)
            .await?;
    if let Some(other) = used {
        return Err(binding_used(&record.binding, &other));
    }
    upsert(&mut tx, record, markdown, revision).await?;
    tx.commit().await?;
    Ok(())
}

async fn upsert(
    tx: &mut Transaction<'_, Postgres>,
    record: &SourceRecord,
    markdown: Option<&str>,
    revision: u64,
) -> Result<()> {
    let written = sqlx::query(
        "INSERT INTO playbook_sources(id,kind,binding,scope,purpose,judgment_purpose,commands,
            events,capabilities,facts,markdown,revision,updated_at_ms)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)
         ON CONFLICT(id) DO UPDATE SET binding=EXCLUDED.binding,scope=EXCLUDED.scope,
            purpose=EXCLUDED.purpose,judgment_purpose=EXCLUDED.judgment_purpose,
            commands=EXCLUDED.commands,events=EXCLUDED.events,
            capabilities=EXCLUDED.capabilities,facts=EXCLUDED.facts,markdown=EXCLUDED.markdown,
            revision=EXCLUDED.revision,updated_at_ms=EXCLUDED.updated_at_ms
         WHERE playbook_sources.kind=EXCLUDED.kind",
    )
    .bind(&record.id)
    .bind(record.kind.as_str())
    .bind(&record.binding)
    .bind(&record.scope)
    .bind(&record.purpose)
    .bind(purpose_text(record.judgment_purpose))
    .bind(Json(&record.commands))
    .bind(Json(&record.events))
    .bind(Json(&record.capabilities))
    .bind(Json(&record.facts))
    .bind(markdown)
    .bind(i64::try_from(revision).map_err(|_| Error::Corrupt)?)
    .bind(now_ms())
    .execute(&mut **tx)
    .await;
    match written {
        // Another source took the binding after the check above.
        Err(sqlx::Error::Database(e)) if e.constraint() == Some(BINDING_KEY) => {
            Err(binding_used(&record.binding, "another source"))
        }
        Err(e) => Err(e.into()),
        // A source of the other kind took the id after the read above.
        Ok(done) if done.rows_affected() == 0 => Err(kind_taken(record)),
        Ok(_) => Ok(()),
    }
}

fn kind_taken(record: &SourceRecord) -> Error {
    let other = match record.kind {
        SourceKind::Builtin => "a registered",
        SourceKind::Registered => "a built-in",
    };
    Error::Conflict(format!("{} is already {other} source", record.id))
}

fn binding_used(binding: &str, by: &str) -> Error {
    Error::Conflict(format!("The binding {binding} is already used by {by}"))
}
