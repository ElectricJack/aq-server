//! Sanitized configured-account inventory, not provider health or account quota.
//! Uses the immutable startup snapshot; never reads a key or calls a provider.
use crate::{AppState, authz::RequireOwner, error::AppError, intelligence::config::LoadedRegistry};
use axum::{Json, Router, extract::State, routing::get};
use serde::Serialize;
use std::collections::BTreeMap;

pub use aq_provider_quota::Account;
#[derive(Debug, Serialize)]
pub struct Accounts {
    pub schema_version: u32,
    pub contract: &'static str,
    pub source: &'static str,
    pub server_time_ms: u64,
    pub revision: Option<u64>,
    pub accounts: Vec<Account>,
}
pub(crate) fn project(registry: Option<&LoadedRegistry>, server_time_ms: u64) -> Accounts {
    let mut accounts = BTreeMap::<(&str, &str), Account>::new();
    if let Some(registry) = registry {
        // Startup validation bounds this snapshot to 32 credential profiles.
        for credential in &registry.credentials {
            let account = accounts
                .entry((&credential.provider, &credential.account_reference))
                .or_insert_with(|| Account {
                    provider: credential.provider.clone(),
                    account_reference: credential.account_reference.clone(),
                    enabled: false,
                    provisionable: false,
                    credential_profiles: 0,
                });
            account.enabled |= credential.enabled;
            account.provisionable |= credential.enabled && credential.provisionable;
            account.credential_profiles += 1;
        }
    }
    Accounts {
        schema_version: 1,
        contract: "human-provider-accounts-1",
        source: "startup_configuration",
        server_time_ms,
        revision: registry.map(|registry| registry.config.revision),
        accounts: accounts.into_values().collect(),
    }
}
async fn read(
    State(state): State<AppState>,
    RequireOwner(_): RequireOwner,
) -> Result<impl axum::response::IntoResponse, AppError> {
    let time = u64::try_from((state.clock)().timestamp_millis())
        .ok()
        .filter(|value| *value > 0)
        .ok_or(AppError::Internal)?;
    Ok((
        [("Cache-Control", "private, no-store")],
        Json(project(state.config.intelligence.as_deref(), time)),
    ))
}
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/human/provider-accounts", get(read))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intelligence::config::IntelligenceConfig;
    use aq_selection::{ArtifactRef, AuthorizedCredential, Text};

    fn credential(
        provider: &str,
        account: &str,
        enabled: bool,
        provisionable: bool,
    ) -> AuthorizedCredential {
        AuthorizedCredential {
            profile: ArtifactRef {
                id: Text::new("profile").unwrap(),
                version: Text::new("1").unwrap(),
                digest: aq_admission::Digest::of(b"public metadata"),
            },
            provider: provider.into(),
            account_reference: account.into(),
            endpoint_reference: "private-logical-endpoint-not-exported".into(),
            revision: 1,
            enabled,
            provisionable,
            material_owner: None,
            allowed_backends: aq_selection::default_allowed_backends(),
        }
    }
    #[test]
    fn absent_configuration_is_explicit_empty_inventory() {
        let page = project(None, 123);
        assert_eq!(page.revision, None);
        assert!(page.accounts.is_empty());
        assert_eq!(page.source, "startup_configuration");
    }
    #[test]
    fn accounts_are_bounded_deduplicated_and_never_claim_provider_authentication() {
        let registry = LoadedRegistry {
            config: IntelligenceConfig {
                revision: 7,
                ..Default::default()
            },
            credentials: vec![
                credential("openai", "team", true, false),
                credential("openai", "team", true, true),
                credential("anthropic", "other", false, true),
            ],
            fingerprints: vec![Some("private-fingerprint-not-exported".into())],
        };
        let page = project(Some(&registry), 456);
        assert_eq!(page.revision, Some(7));
        assert_eq!(page.accounts.len(), 2);
        assert_eq!(
            page.accounts[0],
            Account {
                provider: "anthropic".into(),
                account_reference: "other".into(),
                enabled: false,
                provisionable: false,
                credential_profiles: 1
            }
        );
        assert_eq!(
            page.accounts[1],
            Account {
                provider: "openai".into(),
                account_reference: "team".into(),
                enabled: true,
                provisionable: true,
                credential_profiles: 2
            }
        );
        let value = serde_json::to_value(page).unwrap();
        assert_eq!(value["accounts"][1].as_object().unwrap().len(), 5);
        let bytes = serde_json::to_string(&value).unwrap();
        for private in [
            "fingerprint",
            "endpoint",
            "secret_file",
            "authenticated",
            "ready",
            "quota",
            "percent",
        ] {
            assert!(!bytes.contains(private));
        }
    }
}
