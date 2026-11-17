//! Authoritative labels for agent pills, from the role snapshot each attempt
//! launched from (spec 0014): the level's id and the name its document gives it.
//! The legacy model-name class remains a color hint, never admission metadata.
use aq_dashboard_contract::{RoutingProfile, Swarm};
use serde_json::Value;
use sqlx::{PgConnection, Row};
use std::collections::BTreeMap;
use uuid::Uuid;

/// The level `id` of a role snapshot: its id and the name its document gives it.
fn level(snapshot: &Value, id: &str) -> Option<RoutingProfile> {
    let profile = snapshot["intelligence"].get(id)?;
    let name = profile["name"].as_str().filter(|n| !n.trim().is_empty());
    Some(RoutingProfile {
        id: id.to_owned(),
        display_name: name.unwrap_or(id).to_owned(),
    })
}
/// Add labels in two bounded key reads. An attempt from before roles and
/// profiles, or one whose snapshot has rolled off, gets none; the active
/// snapshot, current project binding and model strings are never used.
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
    let rows=sqlx::query("SELECT l.attempt_id,d.decision#>>'{inputs,project,role_snapshot_digest}' AS snapshot,d.decision#>>'{inputs,project,intelligence_profile}' AS profile FROM profiled_launches l JOIN intelligence_decisions d USING(decision_id) WHERE l.attempt_id=ANY($1)")
        .bind(&attempts).fetch_all(&mut *conn).await?;
    let digests: Vec<String> = rows
        .iter()
        .filter_map(|r| r.get::<Option<String>, _>("snapshot"))
        .collect();
    // Spec 0014: an attempt launched from a role snapshot names its level by
    // id, and the snapshot it pinned holds that level's name.
    let stored: bool = sqlx::query_scalar("SELECT to_regclass('role_snapshots') IS NOT NULL")
        .fetch_one(&mut *conn)
        .await?;
    let snapshots: Vec<(String, Vec<u8>)> = if stored {
        sqlx::query_as("SELECT digest,snapshot FROM role_snapshots WHERE digest=ANY($1)")
            .bind(&digests)
            .fetch_all(&mut *conn)
            .await?
    } else {
        Vec::new()
    };
    let snapshots: BTreeMap<String, Value> = snapshots
        .into_iter()
        .filter_map(|(d, b)| Some((d, serde_json::from_slice(&b).ok()?)))
        .collect();
    let profiles: BTreeMap<Uuid, RoutingProfile> = rows
        .iter()
        .filter_map(|row| {
            let digest = row.get::<Option<String>, _>("snapshot")?;
            let profile = row.get::<Option<String>, _>("profile")?;
            let label = level(snapshots.get(&digest)?, &profile)?;
            Some((row.get("attempt_id"), label))
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
