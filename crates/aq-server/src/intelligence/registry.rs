use super::*;
use sqlx::{Postgres, Transaction};
async fn register_definition(
    tx: &mut Transaction<'_, Postgres>,
    item: config::CredentialDefinition,
) -> Result<()> {
    let config::CredentialDefinition {
        identity,
        definition,
        state_digest,
        revision,
    } = item;
    let existing=sqlx::query("SELECT definition_digest,latest_revision,private_state_digest FROM intelligence_credential_definitions WHERE identity=$1").bind(&identity).fetch_optional(&mut **tx).await.map_err(db)?;
    if let Some(row) = &existing {
        if row.get::<String, _>("definition_digest") != definition.as_str() {
            return Err(conflict("historical credential identity/version conflict"));
        }
        let old_revision = row.get::<i64, _>("latest_revision") as u64;
        let changed = row.get::<String, _>("private_state_digest") != state_digest.as_str();
        let compatible = revision > old_revision || (revision == old_revision && !changed);
        if !compatible {
            return Err(conflict(
                "historical credential revision cannot reset after removal/rotation",
            ));
        }
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM intelligence_credential_definitions")
        .fetch_one(&mut **tx)
        .await
        .map_err(db)?;
    if existing.is_none() && count >= 1024 {
        return Err(conflict("credential definition retention limit"));
    }
    sqlx::query("INSERT INTO intelligence_credential_definitions(identity,definition_digest,latest_revision,private_state_digest) VALUES($1,$2,$3,$4) ON CONFLICT(identity) DO UPDATE SET latest_revision=excluded.latest_revision,private_state_digest=excluded.private_state_digest").bind(identity).bind(definition.as_str()).bind(revision as i64).bind(state_digest.as_str()).execute(&mut **tx).await.map_err(db)?;
    Ok(())
}

/// Owner mutation installs immutable startup configuration; no filesystem or
/// provider I/O occurs here.
pub(super) async fn register(state: &AppState) -> Result<()> {
    let registry = loaded(state)?;
    let digest = registry.digest().map_err(|_| AppError::Internal)?;
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    sqlx::query("SELECT pg_advisory_xact_lock(712009)")
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    let previous=sqlx::query("SELECT revision,config_revision,config_digest,credentials,private_fingerprints FROM intelligence_registry WHERE singleton=TRUE FOR UPDATE").fetch_optional(&mut *tx).await.map_err(db)?;
    let revision = if let Some(row) = previous {
        if row.get::<String, _>("config_digest") == digest.as_str() {
            return Ok(());
        }
        if registry.config.revision <= row.get::<i64, _>("config_revision") as u64 {
            return Err(conflict(
                "intelligence configuration revision must increase; restart after rotating credentials",
            ));
        }
        let old: Vec<aq_selection::AuthorizedCredential> =
            serde_json::from_value(row.get("credentials")).map_err(|_| AppError::Internal)?;
        let fingerprints: Vec<Option<String>> =
            serde_json::from_value(row.get("private_fingerprints"))
                .map_err(|_| AppError::Internal)?;
        registry
            .verify_rotation(&old, &fingerprints)
            .map_err(conflict)?;
        (row.get::<i64, _>("revision") as u64)
            .checked_add(1)
            .filter(|r| *r <= i64::MAX as u64)
            .ok_or(AppError::Internal)?
    } else {
        1
    };
    for definition in registry.definitions() {
        let credential = definition.map_err(|_| AppError::Internal)?;
        register_definition(&mut tx, credential).await?;
    }
    sqlx::query("INSERT INTO intelligence_registry(singleton,revision,config_revision,config_digest,credentials,private_fingerprints) VALUES(TRUE,$1,$2,$3,$4,$5) ON CONFLICT(singleton) DO UPDATE SET revision=excluded.revision,config_revision=excluded.config_revision,config_digest=excluded.config_digest,credentials=excluded.credentials,private_fingerprints=excluded.private_fingerprints").bind(revision as i64).bind(registry.config.revision as i64).bind(digest.as_str()).bind(serde_json::to_value(&registry.credentials).map_err(|_|AppError::Internal)?).bind(serde_json::to_value(&registry.fingerprints).map_err(|_|AppError::Internal)?).execute(&mut *tx).await.map_err(db)?;
    tx.commit().await.map_err(db)?;
    Ok(())
}
pub(super) async fn check_current(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
) -> Result<u64> {
    let row = sqlx::query(
        "SELECT revision,config_digest FROM intelligence_registry WHERE singleton=TRUE FOR UPDATE",
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(db)?
    .ok_or_else(|| conflict("intelligence registry has not been installed"))?;
    if row.get::<String, _>("config_digest")
        != loaded(state)?
            .digest()
            .map_err(|_| AppError::Internal)?
            .as_str()
    {
        return Err(conflict("daemon intelligence configuration is stale"));
    }
    Ok(row.get::<i64, _>("revision") as u64)
}
pub(super) async fn quota(
    tx: &mut Transaction<'_, Postgres>,
    new_scope: bool,
    incoming: &Value,
) -> Result<()> {
    let scopes: i64 = sqlx::query_scalar("SELECT count(*) FROM intelligence_observations")
        .fetch_one(&mut **tx)
        .await
        .map_err(db)?;
    let row=sqlx::query("SELECT count(*) AS entries,COALESCE(sum(pg_column_size(response)),0)::BIGINT AS bytes FROM intelligence_operations").fetch_one(&mut **tx).await.map_err(db)?;
    let incoming_bytes: i64 = sqlx::query_scalar("SELECT pg_column_size($1::jsonb)::BIGINT")
        .bind(incoming)
        .fetch_one(&mut **tx)
        .await
        .map_err(db)?;
    let full = (new_scope && scopes >= 1024) || row.get::<i64, _>("entries") >= 10_000;
    let exceeds_bytes = row
        .get::<i64, _>("bytes")
        .checked_add(incoming_bytes)
        .is_none_or(|n| n > 16 * 1024 * 1024);
    if full || exceeds_bytes {
        return Err(conflict("intelligence observation retention limit"));
    }
    Ok(())
}

pub(super) async fn machine_quota(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    incoming: &Value,
) -> Result<()> {
    let bytes: i64 = sqlx::query_scalar("SELECT pg_column_size($1::jsonb)::BIGINT")
        .bind(incoming)
        .fetch_one(&mut **tx)
        .await
        .map_err(db)?;
    if bytes > 128 * 1024 {
        return Err(AppError::PayloadTooLarge(
            "inventory/failure snapshot bound".into(),
        ));
    }
    let row=sqlx::query("SELECT count(*) AS entries,COALESCE(sum(pg_column_size(response)),0)::BIGINT AS bytes FROM execution_receipts WHERE node_id=$1 AND kind IN ('intelligence_inventory','intelligence_failure')").bind(node).fetch_one(&mut **tx).await.map_err(db)?;
    if row.get::<i64, _>("entries") >= 4096
        || row
            .get::<i64, _>("bytes")
            .checked_add(bytes)
            .is_none_or(|n| n > 16 * 1024 * 1024)
    {
        return Err(conflict("node intelligence receipt retention limit"));
    }
    Ok(())
}
