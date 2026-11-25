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
        "SELECT {COLUMNS} FROM playbook_sources WHERE id=$1 AND deleted_at_ms IS NULL"
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
        "SELECT markdown,revision FROM playbook_sources WHERE id=$1 AND markdown IS NOT NULL
            AND deleted_at_ms IS NULL",
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
        "SELECT {COLUMNS},markdown,deleted_at_ms FROM playbook_sources WHERE id=$1 FOR UPDATE"
    ))
    .bind(&record.id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(revision) = next_revision(row.as_ref(), record, markdown)? else {
        return Ok(());
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

fn next_revision(
    row: Option<&PgRow>,
    record: &SourceRecord,
    markdown: Option<&str>,
) -> Result<Option<u64>> {
    let Some(row) = row else { return Ok(Some(1)) };
    let stored = decode(row)?;
    if stored.kind != record.kind {
        return Err(kind_taken(record));
    }
    let text: Option<String> = row.try_get("markdown")?;
    let deleted: Option<i64> = row.try_get("deleted_at_ms")?;
    // The old binding would stay live in the engine with nothing to
    // stop it: a source keeps its binding for as long as it exists.
    if stored.binding != record.binding && deleted.is_none() {
        return Err(Error::Conflict(format!(
            "{} is bound to {}; a source's binding cannot be changed",
            record.id, stored.binding
        )));
    }
    let unchanged = stored.same_as(record) && text.as_deref() == markdown;
    if unchanged && deleted.is_none() {
        return Ok(None);
    }
    Ok(Some(stored.revision + 1))
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
            revision=EXCLUDED.revision,updated_at_ms=EXCLUDED.updated_at_ms,deleted_at_ms=NULL
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

/// Every source, by id.
pub async fn list(pool: &PgPool) -> Result<Vec<SourceRecord>> {
    let rows = sqlx::query(&format!(
        "SELECT {COLUMNS} FROM playbook_sources WHERE deleted_at_ms IS NULL ORDER BY id"
    ))
    .fetch_all(pool)
    .await?;
    rows.iter().map(decode).collect()
}

/// Replaces a registered source's Markdown when its revision is still
/// `expected`, and returns the new revision.
pub async fn set_markdown(pool: &PgPool, id: &str, markdown: &str, expected: u64) -> Result<u64> {
    let record = (get(pool, id).await?)
        .ok_or_else(|| Error::NotFound(format!("There is no playbook source {id}")))?;
    if record.kind != SourceKind::Registered {
        return Err(Error::Conflict(format!(
            "{id} is a built-in source: its document is edited where the server keeps it"
        )));
    }
    let revision: Option<i64> = sqlx::query_scalar(
        "UPDATE playbook_sources SET markdown=$2,revision=revision+1,updated_at_ms=$4
         WHERE id=$1 AND kind='registered' AND deleted_at_ms IS NULL AND revision=$3
         RETURNING revision",
    )
    .bind(id)
    .bind(markdown)
    .bind(i64::try_from(expected).map_err(|_| Error::Invalid("expected_revision".into()))?)
    .bind(now_ms())
    .fetch_optional(pool)
    .await?;
    let revision = revision.ok_or_else(|| {
        Error::Conflict(format!(
            "{id} was changed by someone else; read it again before saving"
        ))
    })?;
    u64::try_from(revision).map_err(|_| Error::Corrupt)
}

/// Marks a registered source deleted: its runs stop, nothing of it is
/// active, and its run history stays. The built-in source is refused.
pub async fn remove(pool: &PgPool, id: &str) -> Result<()> {
    let record = (get(pool, id).await?)
        .ok_or_else(|| Error::NotFound(format!("There is no playbook source {id}")))?;
    if record.kind != SourceKind::Registered {
        return Err(Error::Conflict(format!(
            "{id} is a built-in source and cannot be deleted"
        )));
    }
    let now = now_ms();
    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE playbook_sources SET deleted_at_ms=$2 WHERE id=$1")
        .bind(id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE playbook_authoring_runs SET status='superseded',finished_at_ms=$2
         WHERE source_id=$1 AND status='running'",
    )
    .bind(id)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE playbook_authoring_runs SET active=FALSE WHERE source_id=$1 AND active")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
