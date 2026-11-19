//! Owner-managed provider membership. Credentials remain owned by startup configuration.
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool, Row};
use std::collections::BTreeSet;
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub accounts: Vec<String>,
    pub credential_status: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SaveRequest {
    pub operation_id: Uuid,
    pub expected_revision: String,
    pub expected_available_revision: String,
    pub providers: Vec<String>,
}
fn snapshot(revision: i64, stored: Option<Vec<String>>, available: &[Provider]) -> Value {
    let providers = stored.unwrap_or_else(|| {
        available
            .iter()
            .filter(|p| p.credential_status != "not_configured")
            .map(|p| p.id.clone())
            .collect()
    });
    json!({"revision":revision.to_string(),"providers":providers,"available":available,"available_revision":aq_profiles::digest(&json!(available)).expect("serializable provider metadata")})
}
pub async fn read(pool: &PgPool, available: &[Provider]) -> Result<Value> {
    let row = sqlx::query("SELECT revision,providers FROM provider_settings WHERE singleton")
        .fetch_one(pool)
        .await?;
    let stored = row
        .get::<Option<Value>, _>("providers")
        .map(serde_json::from_value)
        .transpose()
        .map_err(|_| Error::Internal)?;
    Ok(snapshot(row.get("revision"), stored, available))
}
pub async fn operation(pool: &PgPool, actor: Uuid, id: Uuid) -> Result<Value> {
    sqlx::query_scalar(
        "SELECT response FROM provider_settings_operations WHERE actor_id=$1 AND operation_id=$2",
    )
    .bind(actor)
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| Error::NotFound("Save has not been recorded".into()))
}
pub async fn save(
    pool: &PgPool,
    actor: Uuid,
    request: SaveRequest,
    available: &[Provider],
) -> Result<Value> {
    let expected = request
        .expected_revision
        .parse::<i64>()
        .ok()
        .filter(|n| *n >= 0 && *n < i64::MAX)
        .ok_or_else(|| crate::invalid("Invalid provider settings revision"))?;
    if request.operation_id.is_nil()
        || request.providers.len() > 32
        || request
            .providers
            .iter()
            .any(|id| id.is_empty() || id.len() > 512)
        || request.providers.iter().collect::<BTreeSet<_>>().len() != request.providers.len()
    {
        return Err(crate::invalid("Choose each provider at most once"));
    }
    let digest = aq_profiles::digest(&serde_json::to_value(&request).map_err(|_| Error::Internal)?)
        .map_err(|_| Error::Internal)?;
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout='1s'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL statement_timeout='5s'")
        .execute(&mut *tx)
        .await?;
    // Same order as admission: swarm, then provider membership. Running agents are untouched.
    crate::lock(&mut tx).await?;
    let row =
        sqlx::query("SELECT revision,providers FROM provider_settings WHERE singleton FOR UPDATE")
            .fetch_one(&mut *tx)
            .await?;
    if let Some(receipt) = sqlx::query("SELECT payload_digest,response FROM provider_settings_operations WHERE actor_id=$1 AND operation_id=$2")
        .bind(actor).bind(request.operation_id).fetch_optional(&mut *tx).await? {
        if receipt.get::<String, _>("payload_digest") != digest {
            return Err(crate::conflict("Save identity was reused with different changes"));
        }
        return Ok(receipt.get("response"));
    }
    if row.get::<i64, _>("revision") != expected {
        return Err(crate::conflict(
            "Providers changed. Your draft is preserved; reload before saving",
        ));
    }
    let initial = row.get::<Option<Value>, _>("providers").is_none();
    let previous = row
        .get::<Option<Value>, _>("providers")
        .map(serde_json::from_value)
        .transpose()
        .map_err(|_| Error::Internal)?;
    let previous = snapshot(expected, previous, available);
    if previous["available_revision"] != request.expected_available_revision {
        return Err(crate::conflict(
            "Server provider configuration changed. Reload before saving",
        ));
    }
    let old: Vec<String> =
        serde_json::from_value(previous["providers"].clone()).map_err(|_| Error::Internal)?;
    if request
        .providers
        .iter()
        .any(|id| !available.iter().any(|p| &p.id == id) && !old.contains(id))
    {
        return Err(crate::invalid("Choose a provider from the available list"));
    }
    let changed = initial
        || old.iter().collect::<BTreeSet<_>>() != request.providers.iter().collect::<BTreeSet<_>>();
    let revision = expected + i64::from(changed);
    let mut response = snapshot(revision, Some(request.providers.clone()), available);
    response["operation_id"] = json!(request.operation_id);
    let usage = sqlx::query("SELECT count(*) AS entries, COALESCE(sum(pg_column_size(response)),0)::BIGINT AS bytes FROM provider_settings_operations")
        .fetch_one(&mut *tx).await?;
    let bytes: i64 = sqlx::query_scalar("SELECT pg_column_size($1::jsonb)::BIGINT")
        .bind(&response)
        .fetch_one(&mut *tx)
        .await?;
    if usage.get::<i64, _>("entries") >= 10000
        || usage.get::<i64, _>("bytes").saturating_add(bytes) > 16 * 1024 * 1024
    {
        return Err(crate::conflict("Provider save receipt limit reached"));
    }
    sqlx::query("UPDATE provider_settings SET revision=$1,providers=$2 WHERE singleton")
        .bind(revision)
        .bind(json!(request.providers))
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO provider_settings_operations(actor_id,operation_id,payload_digest,response) VALUES($1,$2,$3,$4)")
        .bind(actor).bind(request.operation_id).bind(digest).bind(&response).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(response)
}
/// Membership is fenced under the caller's admission transaction. Prefix fixtures stay compatible.
pub async fn allows(
    conn: &mut PgConnection,
    provider: &str,
) -> std::result::Result<bool, sqlx::Error> {
    let exists: bool = sqlx::query_scalar("SELECT to_regclass('provider_settings') IS NOT NULL")
        .fetch_one(&mut *conn)
        .await?;
    if !exists {
        return Ok(true);
    }
    let providers: Option<Value> =
        sqlx::query_scalar("SELECT providers FROM provider_settings WHERE singleton FOR SHARE")
            .fetch_one(&mut *conn)
            .await?;
    Ok(providers.is_none_or(|ids| {
        ids.as_array()
            .is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(provider)))
    }))
}

/// Membership affects provider selection, independently of task intelligence routing.
/// Already reserved attempts retain their frozen membership while startup revocation still wins.
pub async fn mask(
    conn: &mut PgConnection,
    credentials: &mut [aq_selection::AuthorizedCredential],
    frozen: Option<&[aq_selection::AuthorizedCredential]>,
) -> std::result::Result<(), sqlx::Error> {
    if let Some(frozen) = frozen {
        for credential in credentials {
            credential.enabled &= frozen
                .iter()
                .find(|old| old.profile == credential.profile)
                .is_some_and(|old| old.enabled);
        }
        return Ok(());
    }
    let exists: bool = sqlx::query_scalar("SELECT to_regclass('provider_settings') IS NOT NULL")
        .fetch_one(&mut *conn)
        .await?;
    if !exists {
        return Ok(());
    }
    let providers: Option<Value> =
        sqlx::query_scalar("SELECT providers FROM provider_settings WHERE singleton FOR SHARE")
            .fetch_one(&mut *conn)
            .await?;
    if let Some(providers) = providers {
        for credential in credentials {
            credential.enabled &= providers.as_array().is_some_and(|ids| {
                ids.iter()
                    .any(|id| id.as_str() == Some(&credential.provider))
            });
        }
    }
    Ok(())
}
