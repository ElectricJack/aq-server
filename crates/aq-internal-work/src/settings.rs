//! Which intelligence profile internal work uses (spec 0012 §5): one default
//! and per-purpose overrides, saved with a revision fence and replayable
//! receipts. The structure follows `aq-profile-settings`' `structure.rs`.
use crate::{Error, Purpose, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool, Row};
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
    pub expected_catalog_revision: String,
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

/// Catalog revision and the ids of the active profiles that may be chosen
/// (every profile except the automatic router).
async fn catalog(conn: &mut PgConnection) -> Result<(String, BTreeSet<String>)> {
    let (revision, catalog) = active_catalog(conn).await?;
    let ids = catalog
        .iter()
        .flat_map(|c| &c.artifact().catalog.intelligence);
    let ids = ids
        .filter(|p| p.automatic_routing != Some(true))
        .map(|p| p.id.clone())
        .collect();
    Ok((revision.to_string(), ids))
}

/// The catalog registry revision and the active catalog, if one is active.
pub async fn active_catalog(
    conn: &mut PgConnection,
) -> Result<(i64, Option<aq_profiles::ValidatedCatalog>)> {
    let row =
        sqlx::query("SELECT revision,active_digest FROM profile_catalog_registry WHERE singleton")
            .fetch_one(&mut *conn)
            .await?;
    let revision = row.get("revision");
    let Some(digest) = row.get::<Option<String>, _>("active_digest") else {
        return Ok((revision, None));
    };
    let bytes: Vec<u8> =
        sqlx::query_scalar("SELECT artifact FROM profile_catalog_artifacts WHERE digest=$1")
            .bind(&digest)
            .fetch_one(&mut *conn)
            .await?;
    let catalog =
        aq_profiles::ValidatedCatalog::from_artifact(&bytes).map_err(|_| Error::Corrupt)?;
    if catalog.digest() != digest {
        return Err(Error::Corrupt);
    }
    Ok((revision, Some(catalog)))
}

fn decode(row: &sqlx::postgres::PgRow) -> Result<Document> {
    serde_json::from_value(row.get("document")).map_err(|_| Error::Corrupt)
}

fn snapshot(revision: i64, catalog_revision: &str, document: &Document) -> Result<Value> {
    let mut out = value(document)?;
    out["revision"] = json!(revision.to_string());
    out["catalog_revision"] = json!(catalog_revision);
    out["purposes"] = value(&Purpose::ALL)?;
    Ok(out)
}

pub async fn read(pool: &PgPool) -> Result<Value> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await?;
    let (catalog_revision, _) = catalog(&mut tx).await?;
    let row = sqlx::query("SELECT revision,document FROM internal_work_settings WHERE singleton")
        .fetch_one(&mut *tx)
        .await?;
    let out = snapshot(row.get("revision"), &catalog_revision, &decode(&row)?)?;
    tx.commit().await?;
    Ok(out)
}

pub async fn operation(pool: &PgPool, actor: Uuid, id: Uuid) -> Result<Value> {
    sqlx::query_scalar("SELECT response FROM internal_work_settings_operations WHERE actor_id=$1 AND operation_id=$2")
        .bind(actor).bind(id).fetch_optional(pool).await?
        .ok_or_else(|| Error::NotFound("Save has not been recorded".into()))
}

/// The profile id for `purpose`: its override, else the default, else none.
pub async fn profile_for(pool: &PgPool, purpose: Purpose) -> Result<Option<String>> {
    let document: Value =
        sqlx::query_scalar("SELECT document FROM internal_work_settings WHERE singleton")
            .fetch_one(pool)
            .await?;
    let document: Document = serde_json::from_value(document).map_err(|_| Error::Corrupt)?;
    Ok(document
        .overrides
        .get(&purpose)
        .cloned()
        .or(document.default_profile_id))
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
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout='1s'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL statement_timeout='5s'")
        .execute(&mut *tx)
        .await?;
    // Catalog first, as catalog saves do.
    sqlx::query("SELECT revision FROM profile_catalog_registry WHERE singleton FOR SHARE")
        .fetch_one(&mut *tx)
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
    let (catalog_revision, ids) = catalog(&mut tx).await?;
    if row.get::<i64, _>("revision") != expected
        || req.expected_catalog_revision != catalog_revision
    {
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
            return Err(invalid(
                "Choose an Intelligence profile that exists and is not automatic routing",
            ));
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
    let mut response = snapshot(revision, &catalog_revision, &req.document)?;
    response["operation_id"] = json!(req.operation_id);
    response["disposition"] = json!(if changed { "saved" } else { "unchanged" });
    sqlx::query("INSERT INTO internal_work_settings_operations(actor_id,operation_id,payload_digest,response) VALUES($1,$2,$3,$4)")
        .bind(actor).bind(req.operation_id).bind(digest).bind(&response).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(response)
}
