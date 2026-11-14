//! Authoritative labels for agent pills, from each attempt's pinned catalog.
//! The legacy model-name class remains a color hint, never admission metadata.
use aq_dashboard_contract::{RoutingProfile, Swarm};
use serde_json::Value;
use sqlx::{PgConnection, Row};
use std::collections::BTreeMap;
use uuid::Uuid;

fn label(catalog: &Value, reference: &str) -> Option<RoutingProfile> {
    let profile = catalog["catalog"]["intelligence"]
        .as_array()?
        .iter()
        .find(|p| {
            p["id"]
                .as_str()
                .is_some_and(|id| format!("{id}@{}", p["version"]) == reference)
        })?;
    let id = profile["id"].as_str()?.to_owned();
    let display_name = if profile["automatic_routing"] == true {
        "Automatic routing".into()
    } else {
        profile["name"]
            .as_str()
            .filter(|n| !n.trim().is_empty())
            .unwrap_or(&id)
            .to_owned()
    };
    Some(RoutingProfile { id, display_name })
}
/// Add labels in two bounded key reads. Missing historical identity stays absent;
/// the active catalog, current project binding and model strings are never used.
pub async fn apply(conn: &mut PgConnection, swarm: &mut Swarm) -> Result<(), sqlx::Error> {
    let attempts: Vec<Uuid> = swarm
        .machines
        .iter()
        .flat_map(|m| &m.nodes)
        .flat_map(|n| &n.agents)
        .map(|a| a.attempt_id)
        .collect();
    if attempts.is_empty() {
        return Ok(());
    }
    let rows=sqlx::query("SELECT l.attempt_id,d.decision#>>'{inputs,project,catalog_digest}' AS catalog,d.decision#>>'{inputs,project,intelligence_profile}' AS profile FROM profiled_launches l JOIN intelligence_decisions d USING(decision_id) WHERE l.attempt_id=ANY($1)")
        .bind(&attempts).fetch_all(&mut *conn).await?;
    let digests: Vec<String> = rows
        .iter()
        .filter_map(|r| r.get::<Option<String>, _>("catalog"))
        .collect();
    let artifacts: Vec<(String, Vec<u8>)> = sqlx::query_as(
        "SELECT digest,artifact FROM profile_catalog_artifacts WHERE digest=ANY($1)",
    )
    .bind(&digests)
    .fetch_all(conn)
    .await?;
    let catalogs: BTreeMap<String, Value> = artifacts
        .into_iter()
        .filter_map(|(d, b)| Some((d, serde_json::from_slice(&b).ok()?)))
        .collect();
    let profiles: BTreeMap<Uuid, RoutingProfile> = rows
        .iter()
        .filter_map(|row| {
            let digest = row.get::<Option<String>, _>("catalog")?;
            let profile = row.get::<Option<String>, _>("profile")?;
            Some((
                row.get("attempt_id"),
                label(catalogs.get(&digest)?, &profile)?,
            ))
        })
        .collect();
    for agent in swarm
        .machines
        .iter_mut()
        .flat_map(|m| &mut m.nodes)
        .flat_map(|n| &mut n.agents)
    {
        agent.routing_profile = profiles.get(&agent.attempt_id).cloned();
    }
    Ok(())
}
