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
    /// What the owner must do before this provider's agents can start again, when the
    /// server knows (a saved Codex login older than the owner's limit).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attention: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SaveRequest {
    pub operation_id: Uuid,
    pub expected_revision: String,
    pub expected_available_revision: String,
    pub providers: Vec<String>,
    /// Absent: unchanged. `null`: no limit. Days since a stored Codex login's last
    /// refresh after which the server refuses to lease it.
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub codex_login_max_age_days: Option<Option<i32>>,
}
fn present<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Option<Option<i32>>, D::Error> {
    Deserialize::deserialize(d).map(Some)
}
/// The owner's Codex login age limit in days; None is no limit. Schemas before
/// the column (prefix fixtures) have none.
pub async fn codex_login_max_age_days(
    conn: &mut PgConnection,
) -> std::result::Result<Option<i64>, sqlx::Error> {
    let exists: bool = sqlx::query_scalar("SELECT to_regclass('provider_settings') IS NOT NULL")
        .fetch_one(&mut *conn)
        .await?;
    if !exists {
        return Ok(None);
    }
    sqlx::query_scalar("SELECT (to_jsonb(p)->>'codex_login_max_age_days')::BIGINT FROM provider_settings p WHERE singleton")
        .fetch_one(conn)
        .await
}
fn snapshot(
    revision: i64,
    stored: Option<Vec<String>>,
    max_age: Option<i32>,
    available: &[Provider],
) -> Value {
    let providers = stored.unwrap_or_else(|| {
        available
            .iter()
            .filter(|p| p.credential_status != "not_configured")
            .map(|p| p.id.clone())
            .collect()
    });
    json!({"revision":revision.to_string(),"providers":providers,"codex_login_max_age_days":max_age,"available":available,"available_revision":aq_profiles::digest(&json!(available)).expect("serializable provider metadata")})
}
pub async fn read(pool: &PgPool, available: &[Provider]) -> Result<Value> {
    let row = sqlx::query(
        "SELECT revision,providers,codex_login_max_age_days FROM provider_settings WHERE singleton",
    )
    .fetch_one(pool)
    .await?;
    let stored = row
        .get::<Option<Value>, _>("providers")
        .map(serde_json::from_value)
        .transpose()
        .map_err(|_| Error::Internal)?;
    Ok(snapshot(
        row.get("revision"),
        stored,
        row.get("codex_login_max_age_days"),
        available,
    ))
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
    let expected = request.expected_revision()?;
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
        sqlx::query("SELECT revision,providers,codex_login_max_age_days FROM provider_settings WHERE singleton FOR UPDATE")
            .fetch_one(&mut *tx)
            .await?;
    if let Some(receipt) = sqlx::query("SELECT payload_digest,response FROM provider_settings_operations WHERE actor_id=$1 AND operation_id=$2")
        .bind(actor).bind(request.operation_id).fetch_optional(&mut *tx).await? {
        if receipt.get::<String, _>("payload_digest") != digest {
            return Err(crate::conflict("Save identity was reused with different changes"));
        }
        return Ok(receipt.get("response"));
    }
    let (revision, max_age, response) = request.apply(&row, expected, available)?;
    check_receipt_capacity(&mut tx, &response).await?;
    sqlx::query("UPDATE provider_settings SET revision=$1,providers=$2,codex_login_max_age_days=$3 WHERE singleton")
        .bind(revision)
        .bind(json!(request.providers))
        .bind(max_age)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO provider_settings_operations(actor_id,operation_id,payload_digest,response) VALUES($1,$2,$3,$4)")
        .bind(actor).bind(request.operation_id).bind(digest).bind(&response).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(response)
}
impl SaveRequest {
    fn expected_revision(&self) -> Result<i64> {
        let expected = self
            .expected_revision
            .parse::<i64>()
            .ok()
            .filter(|n| *n >= 0 && *n < i64::MAX)
            .ok_or_else(|| crate::invalid("Invalid provider settings revision"))?;
        if self.operation_id.is_nil() || !self.valid_providers() {
            return Err(crate::invalid("Choose each provider at most once"));
        }
        if let Some(Some(days)) = self.codex_login_max_age_days
            && !(1..=3650).contains(&days)
        {
            return Err(crate::invalid(
                "The Codex login age limit is 1 to 3650 days, or empty for none",
            ));
        }
        Ok(expected)
    }
    fn valid_providers(&self) -> bool {
        let within_bounds = self.providers.len() <= 32
            && self
                .providers
                .iter()
                .all(|id| !id.is_empty() && id.len() <= 512);
        let unique = self.providers.iter().collect::<BTreeSet<_>>().len() == self.providers.len();
        within_bounds && unique
    }
    fn apply(
        &self,
        row: &sqlx::postgres::PgRow,
        expected: i64,
        available: &[Provider],
    ) -> Result<(i64, Option<i32>, Value)> {
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
        let old_age: Option<i32> = row.get("codex_login_max_age_days");
        let max_age = self.codex_login_max_age_days.unwrap_or(old_age);
        let previous = snapshot(expected, previous, old_age, available);
        if previous["available_revision"] != self.expected_available_revision {
            return Err(crate::conflict(
                "Server provider configuration changed. Reload before saving",
            ));
        }
        let old: Vec<String> =
            serde_json::from_value(previous["providers"].clone()).map_err(|_| Error::Internal)?;
        if self
            .providers
            .iter()
            .any(|id| !available.iter().any(|p| &p.id == id) && !old.contains(id))
        {
            return Err(crate::invalid("Choose a provider from the available list"));
        }
        let changed = initial
            || max_age != old_age
            || old.iter().collect::<BTreeSet<_>>()
                != self.providers.iter().collect::<BTreeSet<_>>();
        let revision = expected + i64::from(changed);
        let mut response = snapshot(revision, Some(self.providers.clone()), max_age, available);
        response["operation_id"] = json!(self.operation_id);
        Ok((revision, max_age, response))
    }
}
async fn check_receipt_capacity(conn: &mut PgConnection, response: &Value) -> Result<()> {
    let usage = sqlx::query("SELECT count(*) AS entries, COALESCE(sum(pg_column_size(response)),0)::BIGINT AS bytes FROM provider_settings_operations")
        .fetch_one(&mut *conn).await?;
    let bytes: i64 = sqlx::query_scalar("SELECT pg_column_size($1::jsonb)::BIGINT")
        .bind(response)
        .fetch_one(&mut *conn)
        .await?;
    if usage.get::<i64, _>("entries") >= 10000
        || usage.get::<i64, _>("bytes").saturating_add(bytes) > 16 * 1024 * 1024
    {
        return Err(crate::conflict("Provider save receipt limit reached"));
    }
    Ok(())
}
async fn membership(conn: &mut PgConnection) -> std::result::Result<Option<Value>, sqlx::Error> {
    let exists: bool = sqlx::query_scalar("SELECT to_regclass('provider_settings') IS NOT NULL")
        .fetch_one(&mut *conn)
        .await?;
    if !exists {
        return Ok(None);
    }
    let providers: Option<Value> =
        sqlx::query_scalar("SELECT providers FROM provider_settings WHERE singleton FOR SHARE")
            .fetch_one(&mut *conn)
            .await?;
    Ok(providers)
}
/// Membership is fenced under the caller's admission transaction. Prefix fixtures stay compatible.
pub async fn allows(
    conn: &mut PgConnection,
    provider: &str,
) -> std::result::Result<bool, sqlx::Error> {
    let providers = membership(conn).await?;
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
        keep_frozen_credentials(credentials, frozen);
        return Ok(());
    }
    let providers = membership(conn).await?;
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

fn keep_frozen_credentials(
    credentials: &mut [aq_selection::AuthorizedCredential],
    frozen: &[aq_selection::AuthorizedCredential],
) {
    for credential in credentials {
        credential.enabled &= frozen
            .iter()
            .find(|old| old.profile == credential.profile)
            .is_some_and(|old| old.enabled);
    }
}
