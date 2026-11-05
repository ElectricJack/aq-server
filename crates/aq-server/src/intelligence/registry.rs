use super::*;
use sqlx::{Postgres, Transaction};
/// One credential's digest contribution. Untagged tuple variants serialize as
/// plain JSON arrays, so `Legacy` is byte-identical to the pre-C5a1
/// `(credential, allowed_node_ids)` tuple: an unchanged default configuration
/// keeps its stored digest across the upgrade and needs no revision bump.
#[derive(serde::Serialize)]
#[serde(untagged)]
enum Placement<'a> {
    Legacy(&'a aq_selection::AuthorizedCredential, &'a [uuid::Uuid]),
    Kinded(
        &'a aq_selection::AuthorizedCredential,
        &'a [uuid::Uuid],
        aq_selection::CredentialKind,
    ),
}
fn configuration(registry: &config::LoadedRegistry) -> Result<aq_selection::Digest> {
    // The backend allowlist participates through `registry.credentials`, whose
    // canonical, skip-if-unrestricted field `load` derives; `c.credential` is
    // validated unrestricted, so it also serializes exactly as before.
    let placements: Vec<_> = registry
        .config
        .credentials
        .iter()
        .map(|c| match c.kind {
            aq_selection::CredentialKind::ApiKey => {
                Placement::Legacy(&c.credential, &c.allowed_node_ids)
            }
            kind => Placement::Kinded(&c.credential, &c.allowed_node_ids, kind),
        })
        .collect();
    aq_selection::digest(&(
        registry.config.revision,
        placements,
        &registry.config.qualified_harnesses,
        &registry.credentials,
        &registry.fingerprints,
    ))
    .map_err(|_| AppError::Internal)
}
/// Owner mutation installs immutable startup configuration; no filesystem or
/// provider I/O occurs here.
pub(super) async fn register(state: &AppState) -> Result<()> {
    let registry = loaded(state)?;
    let digest = configuration(registry)?;
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
        for (index, new) in registry.credentials.iter().enumerate() {
            if let Some((old_index, old)) = old.iter().enumerate().find(|(_, c)| {
                c.profile.id == new.profile.id && c.profile.version == new.profile.version
            }) {
                if old.profile != new.profile
                    || old.provider != new.provider
                    || old.account_reference != new.account_reference
                    || old.endpoint_reference != new.endpoint_reference
                {
                    return Err(conflict(
                        "credential identity/version cannot change definition",
                    ));
                }
                if new.revision < old.revision
                    || (new.revision == old.revision
                        && (fingerprints.get(old_index) != registry.fingerprints.get(index)
                            || old.enabled != new.enabled
                            || old.provisionable != new.provisionable))
                {
                    return Err(conflict(
                        "credential rotation/state change requires increasing credential revision",
                    ));
                }
            }
        }
        (row.get::<i64, _>("revision") as u64)
            .checked_add(1)
            .filter(|r| *r <= i64::MAX as u64)
            .ok_or(AppError::Internal)?
    } else {
        1
    };
    for (credential_index, credential) in registry.credentials.iter().enumerate() {
        let identity = format!(
            "{}@{}",
            credential.profile.id.as_str(),
            credential.profile.version.as_str()
        );
        let definition = aq_selection::digest(&(
            &credential.profile,
            &credential.provider,
            &credential.account_reference,
            &credential.endpoint_reference,
        ))
        .map_err(|_| AppError::Internal)?;
        let state_digest = aq_selection::digest(&(
            credential.enabled,
            credential.provisionable,
            &registry.fingerprints[credential_index],
        ))
        .map_err(|_| AppError::Internal)?;
        let existing=sqlx::query("SELECT definition_digest,latest_revision,private_state_digest FROM intelligence_credential_definitions WHERE identity=$1").bind(&identity).fetch_optional(&mut *tx).await.map_err(db)?;
        if let Some(row) = &existing {
            if row.get::<String, _>("definition_digest") != definition.as_str() {
                return Err(conflict("historical credential identity/version conflict"));
            }
            let old_revision = row.get::<i64, _>("latest_revision") as u64;
            if credential.revision < old_revision
                || (credential.revision == old_revision
                    && row.get::<String, _>("private_state_digest") != state_digest.as_str())
            {
                return Err(conflict(
                    "historical credential revision cannot reset after removal/rotation",
                ));
            }
        }
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM intelligence_credential_definitions")
                .fetch_one(&mut *tx)
                .await
                .map_err(db)?;
        if existing.is_none() && count >= 1024 {
            return Err(conflict("credential definition retention limit"));
        }
        sqlx::query("INSERT INTO intelligence_credential_definitions(identity,definition_digest,latest_revision,private_state_digest) VALUES($1,$2,$3,$4) ON CONFLICT(identity) DO UPDATE SET latest_revision=excluded.latest_revision,private_state_digest=excluded.private_state_digest").bind(identity).bind(definition.as_str()).bind(credential.revision as i64).bind(state_digest.as_str()).execute(&mut *tx).await.map_err(db)?;
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
    if row.get::<String, _>("config_digest") != configuration(loaded(state)?)?.as_str() {
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
    if (new_scope && scopes >= 1024)
        || row.get::<i64, _>("entries") >= 10_000
        || row
            .get::<i64, _>("bytes")
            .checked_add(incoming_bytes)
            .is_none_or(|n| n > 16 * 1024 * 1024)
    {
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

#[cfg(test)]
mod tests {
    use super::*;
    use aq_selection::{BackendKind, CredentialKind};

    /// A pre-C5a1 startup configuration: no `kind`, no `allowed_backends`.
    fn legacy_config() -> config::IntelligenceConfig {
        serde_json::from_value(serde_json::json!({
            "revision": 7,
            "credentials": [{
                "credential": {
                    "profile": {"id":"test", "version":"1", "digest":"a".repeat(64)},
                    "provider":"test", "account_reference":"acct", "endpoint_reference":"endpoint",
                    "revision":3, "enabled":true, "provisionable":false
                },
                "allowed_node_ids": ["00000000-0000-4000-8000-000000000001"],
                "secret_file": null
            }],
            "qualified_harnesses": []
        }))
        .unwrap()
    }
    fn digest(cfg: &config::IntelligenceConfig) -> aq_selection::Digest {
        configuration(&cfg.load().unwrap()).unwrap()
    }

    /// Pinned from base 86ca279 (pre-C5a1 `configuration`) on this exact
    /// input; see docs/implementation/C5a1.md. A change here forces every
    /// existing installation through a 409 until its revision is bumped.
    const LEGACY_DIGEST: &str = "35c9911673be2ce9dece9de54ea211f45be4b59f83f6b238e08a215cd0873ebd";

    #[test]
    fn credential_legacy_default_config_digest_is_unchanged() {
        let cfg = legacy_config();
        assert_eq!(digest(&cfg).as_str(), LEGACY_DIGEST);
        // Spelling the defaults explicitly, in any order, is the same policy.
        let mut explicit = cfg.clone();
        explicit.credentials[0].kind = CredentialKind::ApiKey;
        explicit.credentials[0].allowed_backends = vec![
            BackendKind::CloudflareContainers,
            BackendKind::DockerLocal,
            BackendKind::Boat,
        ];
        assert_eq!(digest(&explicit).as_str(), LEGACY_DIGEST);
    }

    #[test]
    fn credential_kind_and_backend_policy_change_registry_digest() {
        let cfg = legacy_config();
        let baseline = digest(&cfg);
        let mut changed = cfg.clone();
        changed.credentials[0].kind = CredentialKind::SubscriptionLogin;
        assert_ne!(digest(&changed), baseline);
        let mut narrowed = cfg.clone();
        narrowed.credentials[0].allowed_backends = vec![BackendKind::Boat];
        assert_ne!(digest(&narrowed), baseline);
        let mut denied = cfg;
        denied.credentials[0].allowed_backends = vec![];
        assert_ne!(digest(&denied), baseline);
        assert_ne!(digest(&denied), digest(&narrowed));
    }

    #[test]
    fn credential_narrowed_backend_order_does_not_change_digest() {
        let mut first = legacy_config();
        first.credentials[0].allowed_backends =
            vec![BackendKind::CloudflareContainers, BackendKind::DockerLocal];
        let mut second = legacy_config();
        second.credentials[0].allowed_backends =
            vec![BackendKind::DockerLocal, BackendKind::CloudflareContainers];
        assert_eq!(digest(&first), digest(&second));
        assert_eq!(
            first.load().unwrap().credentials[0].allowed_backends,
            vec![BackendKind::DockerLocal, BackendKind::CloudflareContainers]
        );
    }
}
