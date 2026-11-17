//! Which level (intelligence profile document, spec 0014 §5.2) internal work
//! uses (spec 0012 §5): one default and per-purpose overrides, saved with a
//! revision fence and replayable receipts. A choice names a level of the
//! active role snapshot.
use crate::{Error, Purpose, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Document {
    pub default_profile_id: Option<String>,
    pub overrides: BTreeMap<Purpose, String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SaveRequest {
    pub operation_id: Uuid,
    pub expected_revision: String,
    #[serde(flatten)]
    pub document: Document,
}

fn invalid(message: &str) -> Error {
    Error::Invalid(message.into())
}
fn conflict(message: &str) -> Error {
    Error::Conflict(message.into())
}
fn value<T: Serialize>(v: &T) -> Result<Value> {
    serde_json::to_value(v).map_err(|_| Error::Corrupt)
}

/// The ids of the levels (intelligence profile documents) of the active role
/// snapshot: what a purpose may name. Empty when no snapshot is active.
async fn levels(pool: &PgPool) -> Result<BTreeSet<String>> {
    let active = aq_role_settings::active_snapshot(pool)
        .await
        .map_err(|error| match error {
            aq_role_settings::Error::Database(e) => Error::Database(e),
            _ => Error::Corrupt,
        })?;
    Ok(active
        .iter()
        .flat_map(|a| a.snapshot.intelligence_profiles())
        .map(|p| p.id.clone())
        .collect())
}

/// A stored choice whose level no longer exists. `purpose` is `None` for the
/// default.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Missing {
    pub purpose: Option<Purpose>,
    pub profile_id: String,
}
/// The choices of `document` that name no level in `levels`: the default
/// first, then the overrides in purpose order.
pub fn missing(document: &Document, levels: &BTreeSet<String>) -> Vec<Missing> {
    let default = document.default_profile_id.iter().map(|id| (None, id));
    let overrides = document.overrides.iter().map(|(p, id)| (Some(*p), id));
    default
        .chain(overrides)
        .filter(|(_, id)| !levels.contains(*id))
        .map(|(purpose, id)| Missing {
            purpose,
            profile_id: id.clone(),
        })
        .collect()
}

fn decode(row: &sqlx::postgres::PgRow) -> Result<Document> {
    serde_json::from_value(row.get("document")).map_err(|_| Error::Corrupt)
}

/// The document with its revision, the purposes, and every stored choice
/// whose level no longer exists (`missing`; such a purpose runs on the
/// default, see [`level_for`]).
fn snapshot(revision: i64, levels: &BTreeSet<String>, document: &Document) -> Result<Value> {
    let mut out = value(document)?;
    out["revision"] = json!(revision.to_string());
    out["missing"] = value(&missing(document, levels))?;
    out["purposes"] = value(&Purpose::ALL)?;
    Ok(out)
}

pub async fn read(pool: &PgPool) -> Result<Value> {
    let row = sqlx::query("SELECT revision,document FROM internal_work_settings WHERE singleton")
        .fetch_one(pool)
        .await?;
    snapshot(row.get("revision"), &levels(pool).await?, &decode(&row)?)
}

pub async fn operation(pool: &PgPool, actor: Uuid, id: Uuid) -> Result<Value> {
    sqlx::query_scalar("SELECT response FROM internal_work_settings_operations WHERE actor_id=$1 AND operation_id=$2")
        .bind(actor).bind(id).fetch_optional(pool).await?
        .ok_or_else(|| Error::NotFound("Save has not been recorded".into()))
}

/// The level for `purpose` among `levels`: its override, else the default.
/// An override naming a level that no longer exists falls back to the
/// default. `Err` says why there is none: nothing is configured, or the
/// default names a level that no longer exists.
pub fn level_for(
    document: &Document,
    purpose: Purpose,
    levels: &BTreeSet<String>,
) -> std::result::Result<String, String> {
    if let Some(id) = document
        .overrides
        .get(&purpose)
        .filter(|id| levels.contains(*id))
    {
        return Ok(id.clone());
    }
    match &document.default_profile_id {
        Some(id) if levels.contains(id) => Ok(id.clone()),
        Some(id) => Err(format!(
            "the default internal work level {id:?} no longer exists"
        )),
        None => Err("no internal work profile configured".into()),
    }
}

/// The stored document.
pub async fn document(pool: &PgPool) -> Result<Document> {
    let document: Value =
        sqlx::query_scalar("SELECT document FROM internal_work_settings WHERE singleton")
            .fetch_one(pool)
            .await?;
    serde_json::from_value(document).map_err(|_| Error::Corrupt)
}

fn check_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 128 {
        return Err(invalid("Choose an existing Intelligence profile"));
    }
    Ok(())
}

pub async fn save(pool: &PgPool, actor: Uuid, req: SaveRequest) -> Result<Value> {
    let expected: i64 = req
        .expected_revision
        .parse()
        .map_err(|_| invalid("Invalid settings revision"))?;
    if expected < 0 || expected == i64::MAX || req.operation_id.is_nil() {
        return Err(invalid("Invalid save identity or revision"));
    }
    for id in req
        .document
        .default_profile_id
        .iter()
        .chain(req.document.overrides.values())
    {
        check_id(id)?;
    }
    let digest = aq_profiles::digest(&value(&req)?).map_err(|_| Error::Corrupt)?;
    // Read before the transaction: a level deleted after this check is
    // reported on read and its purpose falls back to the default.
    let ids = levels(pool).await?;
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout='1s'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL statement_timeout='5s'")
        .execute(&mut *tx)
        .await?;
    let row = sqlx::query(
        "SELECT revision,document FROM internal_work_settings WHERE singleton FOR UPDATE",
    )
    .fetch_one(&mut *tx)
    .await?;
    if let Some(previous) = sqlx::query("SELECT payload_digest,response FROM internal_work_settings_operations WHERE actor_id=$1 AND operation_id=$2")
        .bind(actor).bind(req.operation_id).fetch_optional(&mut *tx).await? {
        if previous.get::<String, _>("payload_digest") != digest {
            return Err(conflict("Save identity was reused with different changes"));
        }
        let response = previous.get("response");
        tx.commit().await?;
        return Ok(response);
    }
    if row.get::<i64, _>("revision") != expected {
        return Err(conflict(
            "Internal work settings changed. Your draft is preserved; reload before saving",
        ));
    }
    for id in req
        .document
        .default_profile_id
        .iter()
        .chain(req.document.overrides.values())
    {
        if !ids.contains(id) {
            return Err(invalid(&format!(
                "Choose an Intelligence profile that exists: there is no level {id:?}"
            )));
        }
    }
    let changed = req.document != decode(&row)?;
    let revision = if changed { expected + 1 } else { expected };
    sqlx::query("UPDATE internal_work_settings SET revision=$1,document=$2 WHERE singleton")
        .bind(revision)
        .bind(value(&req.document)?)
        .execute(&mut *tx)
        .await?;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM internal_work_settings_operations")
        .fetch_one(&mut *tx)
        .await?;
    if count >= 100_000 {
        return Err(conflict(
            "Internal work settings save history limit reached",
        ));
    }
    let mut response = snapshot(revision, &ids, &req.document)?;
    response["operation_id"] = json!(req.operation_id);
    response["disposition"] = json!(if changed { "saved" } else { "unchanged" });
    sqlx::query("INSERT INTO internal_work_settings_operations(actor_id,operation_id,payload_digest,response) VALUES($1,$2,$3,$4)")
        .bind(actor).bind(req.operation_id).bind(digest).bind(&response).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(response)
}
