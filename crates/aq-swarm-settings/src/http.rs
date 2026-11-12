//! Shared settings route assembly and request dispatch; authentication stays in the server.
use crate::{Result, providers::Provider};
use axum::{Router, extract::DefaultBodyLimit, routing::MethodRouter};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;
pub fn routes<S: Clone + Send + Sync + 'static>(
    read: MethodRouter<S>,
    save: MethodRouter<S>,
    operation: MethodRouter<S>,
) -> Router<S> {
    Router::new()
        .route("/api/settings/swarm", read.clone().merge(save.clone()))
        .route("/api/settings/swarm/limits", save.clone())
        .route("/api/settings/swarm/mode", save.clone())
        .route("/api/settings/swarm/operations/{id}", operation.clone())
        .route("/api/settings/providers", read.merge(save))
        .route("/api/settings/providers/operations/{id}", operation)
        .layer(DefaultBodyLimit::max(128 * 1024))
}
fn value<T: serde::Serialize>(data: T) -> Result<Value> {
    serde_json::to_value(data).map_err(|_| crate::Error::Internal)
}
pub async fn read(pool: &PgPool, available: &[Provider], path: &str) -> Result<Value> {
    if path.contains("/providers") {
        crate::providers::read(pool, available).await
    } else {
        value(crate::read(pool).await?)
    }
}
pub async fn save(
    pool: &PgPool,
    actor: Uuid,
    body: Value,
    available: &[Provider],
    path: &str,
) -> Result<Value> {
    let invalid = |_| crate::invalid("Invalid settings request");
    match path.rsplit('/').next().unwrap_or_default() {
        "providers" => {
            crate::providers::save(
                pool,
                actor,
                serde_json::from_value(body).map_err(invalid)?,
                available,
            )
            .await
        }
        "mode" => value(
            crate::set_mode(pool, actor, serde_json::from_value(body).map_err(invalid)?).await?,
        ),
        "limits" => value(
            crate::set_limits(pool, actor, serde_json::from_value(body).map_err(invalid)?).await?,
        ),
        _ => value(crate::save(pool, actor, serde_json::from_value(body).map_err(invalid)?).await?),
    }
}
pub async fn operation(pool: &PgPool, actor: Uuid, id: Uuid, path: &str) -> Result<Value> {
    if path.contains("/providers") {
        crate::providers::operation(pool, actor, id).await
    } else {
        value(crate::operation(pool, actor, id).await?)
    }
}
/// Aggregate safe account metadata without exporting credential identities or material paths.
pub fn catalog(credentials: &[aq_selection::AuthorizedCredential]) -> Vec<Provider> {
    let mut map =
        std::collections::BTreeMap::<&str, (std::collections::BTreeSet<String>, bool, bool)>::new();
    for c in credentials {
        let (id, account, enabled, present) = (
            c.provider.as_str(),
            c.account_reference.as_str(),
            c.enabled,
            c.provisionable,
        );
        let entry = map.entry(id).or_default();
        entry.0.insert(account.into());
        entry.1 |= enabled;
        entry.2 |= enabled && present;
    }
    map.into_iter()
        .map(|(id, (accounts, enabled, present))| Provider {
            id: id.into(),
            name: match id {
                "anthropic" => "Anthropic",
                "openai" => "OpenAI",
                "google" => "Google",
                "ollama" => "Ollama",
                _ => id,
            }
            .into(),
            accounts: accounts.into_iter().collect(),
            credential_status: if present {
                "present"
            } else if enabled {
                "missing"
            } else {
                "disabled"
            }
            .into(),
        })
        .collect()
}
