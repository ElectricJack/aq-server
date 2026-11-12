//! Presentation metadata and inert routing source. Neither participates in runtime selection.
use crate::{Error, Result, conflict, invalid, value};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool, Row};
use std::collections::BTreeSet;
use uuid::Uuid;

pub const MAX_ROUTING_BYTES: usize = 128 * 1024;
const PARENTS: [&str; 4] = ["simple", "standard", "complex", "deep"];
const EFFORTS: [&str; 4] = ["low", "medium", "high", "max"];
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Parent {
    pub id: String,
    pub description: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Assignment {
    pub profile_id: String,
    pub parent_id: String,
    pub effort: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Document {
    pub routing_markdown: String,
    pub parents: Vec<Parent>,
    pub assignments: Vec<Assignment>,
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
fn validate(doc: &Document) -> Result<()> {
    let ids: BTreeSet<_> = doc.parents.iter().map(|p| p.id.as_str()).collect();
    if doc.routing_markdown.len() > MAX_ROUTING_BYTES
        || doc.parents.len() != 4
        || ids != PARENTS.into_iter().collect()
        || doc.parents.iter().any(|p| p.description.len() > 2000)
        || doc.assignments.len() > 4096
    {
        return Err(invalid(
            "Enter four parent descriptions and at most 128 KiB of routing Markdown",
        ));
    }
    let mut profiles = BTreeSet::new();
    let mut slots = BTreeSet::new();
    for a in &doc.assignments {
        if !PARENTS.contains(&a.parent_id.as_str())
            || !EFFORTS.contains(&a.effort.as_str())
            || a.profile_id.is_empty()
            || a.profile_id.len() > 128
            || !profiles.insert(&a.profile_id)
            || !slots.insert((&a.parent_id, &a.effort))
        {
            return Err(invalid(
                "Each profile needs one valid parent and effort assignment",
            ));
        }
    }
    Ok(())
}
/// An exact allowlist of historical IDs, never a guess from display names.
fn seeded(ids: &[String]) -> Vec<Assignment> {
    let mut out = Vec::new();
    for (old, parent) in [
        ("simple", "simple"),
        ("fast", "simple"),
        ("standard", "standard"),
        ("complex", "complex"),
        ("deep", "deep"),
    ] {
        for effort in EFFORTS {
            let id = format!("{old}-{effort}");
            if ids.contains(&id)
                && !out
                    .iter()
                    .any(|a: &Assignment| a.parent_id == parent && a.effort == effort)
            {
                out.push(Assignment {
                    profile_id: id,
                    parent_id: parent.into(),
                    effort: effort.into(),
                });
            }
        }
    }
    out
}
async fn catalog(conn: &mut PgConnection) -> Result<(String, Vec<String>)> {
    let row =
        sqlx::query("SELECT revision,active_digest FROM profile_catalog_registry WHERE singleton")
            .fetch_one(&mut *conn)
            .await?;
    let mut ids = Vec::new();
    if let Some(digest) = row.get::<Option<String>, _>("active_digest") {
        let bytes: Vec<u8> =
            sqlx::query_scalar("SELECT artifact FROM profile_catalog_artifacts WHERE digest=$1")
                .bind(&digest)
                .fetch_one(&mut *conn)
                .await?;
        let catalog =
            aq_profiles::ValidatedCatalog::from_artifact(&bytes).map_err(|_| Error::Internal)?;
        if catalog.digest() != digest {
            return Err(Error::Internal);
        }
        ids = catalog
            .artifact()
            .catalog
            .intelligence
            .iter()
            .filter(|p| p.automatic_routing != Some(true))
            .map(|p| p.id.clone())
            .collect();
    }
    Ok((row.get::<i64, _>("revision").to_string(), ids))
}
fn decode(row: &sqlx::postgres::PgRow, ids: &[String]) -> Result<Document> {
    let mut document: Document =
        serde_json::from_value(row.get("document")).map_err(|_| Error::Internal)?;
    if row.get::<i64, _>("revision") == 0 {
        document.assignments = seeded(ids);
    }
    Ok(document)
}
fn snapshot(revision: i64, catalog_revision: &str, document: &Document) -> Result<Value> {
    let mut out = value(document)?;
    out["revision"] = json!(revision.to_string());
    out["catalog_revision"] = json!(catalog_revision);
    Ok(out)
}
pub async fn read(pool: &PgPool) -> Result<Value> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await?;
    let (catalog_revision, ids) = catalog(&mut tx).await?;
    let row = sqlx::query("SELECT revision,document FROM intelligence_structure WHERE singleton")
        .fetch_one(&mut *tx)
        .await?;
    let out = snapshot(row.get("revision"), &catalog_revision, &decode(&row, &ids)?)?;
    tx.commit().await?;
    Ok(out)
}
pub async fn operation(pool: &PgPool, actor: Uuid, id: Uuid) -> Result<Value> {
    sqlx::query_scalar("SELECT response FROM intelligence_structure_operations WHERE actor_id=$1 AND operation_id=$2")
        .bind(actor).bind(id).fetch_optional(pool).await?
        .ok_or_else(|| Error::NotFound("Save has not been recorded".into()))
}
pub async fn save(pool: &PgPool, actor: Uuid, req: SaveRequest) -> Result<Value> {
    validate(&req.document)?;
    let expected: i64 = req
        .expected_revision
        .parse()
        .map_err(|_| invalid("Invalid structure revision"))?;
    if expected < 0 || expected == i64::MAX || req.operation_id.is_nil() {
        return Err(invalid("Invalid save identity or revision"));
    }
    let digest = aq_profiles::digest(&value(&req)?).map_err(|_| Error::Internal)?;
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout='1s'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL statement_timeout='5s'")
        .execute(&mut *tx)
        .await?;
    // Catalog first: consistent with catalog saves; no runtime/profile writes occur here.
    sqlx::query("SELECT revision FROM profile_catalog_registry WHERE singleton FOR SHARE")
        .fetch_one(&mut *tx)
        .await?;
    let row = sqlx::query(
        "SELECT revision,document FROM intelligence_structure WHERE singleton FOR UPDATE",
    )
    .fetch_one(&mut *tx)
    .await?;
    if let Some(previous) = sqlx::query("SELECT payload_digest,response FROM intelligence_structure_operations WHERE actor_id=$1 AND operation_id=$2")
        .bind(actor).bind(req.operation_id).fetch_optional(&mut *tx).await? {
        if previous.get::<String, _>("payload_digest") != digest { return Err(conflict("Save identity was reused with different changes")); }
        let response = previous.get("response");
        tx.commit().await?;
        return Ok(response);
    }
    let (catalog_revision, ids) = catalog(&mut tx).await?;
    if row.get::<i64, _>("revision") != expected
        || req.expected_catalog_revision != catalog_revision
    {
        return Err(conflict(
            "Intelligence settings changed. Your draft is preserved; reload before saving",
        ));
    }
    let previous = decode(&row, &ids)?;
    for assignment in &req.document.assignments {
        if !ids.contains(&assignment.profile_id)
            && !previous
                .assignments
                .iter()
                .any(|a| a.profile_id == assignment.profile_id)
        {
            return Err(invalid("Assign only an existing Intelligence profile"));
        }
    }
    let document = value(&req.document)?;
    let changed = document != value(&previous)?;
    // Persist the initial exact-ID migration even if the visible document is unchanged.
    let revision = if changed || expected == 0 {
        expected + 1
    } else {
        expected
    };
    sqlx::query("UPDATE intelligence_structure SET revision=$1,document=$2 WHERE singleton")
        .bind(revision)
        .bind(&document)
        .execute(&mut *tx)
        .await?;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM intelligence_structure_operations")
        .fetch_one(&mut *tx)
        .await?;
    if count >= 100_000 {
        return Err(conflict("Intelligence metadata save history limit reached"));
    }
    let mut response = snapshot(revision, &catalog_revision, &req.document)?;
    response["operation_id"] = json!(req.operation_id);
    response["disposition"] = json!(if changed || expected == 0 {
        "saved"
    } else {
        "unchanged"
    });
    sqlx::query("INSERT INTO intelligence_structure_operations(actor_id,operation_id,payload_digest,response) VALUES($1,$2,$3,$4)")
        .bind(actor).bind(req.operation_id).bind(digest).bind(&response).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(response)
}
