//! Apply owner settings after assembling the reported machine tree.
use aq_dashboard_contract::{HoldCode, HoldReason, Swarm};
use aq_machine_control::management;
use sqlx::PgPool;

pub async fn apply(pool: &PgPool, mut swarm: Swarm) -> Result<Swarm, sqlx::Error> {
    let mut conn = pool.acquire().await?;
    let settings = management::settings(&mut conn).await.map_err(db)?;
    for machine in &mut swarm.machines {
        let setting = settings
            .iter()
            .find(|s| s.kind == "machine" && s.id == machine.id);
        if let Some(label) = setting.and_then(|s| s.label.as_ref()) {
            machine.label = label.clone();
        }
        machine.nodes.retain(|n| {
            !settings
                .iter()
                .any(|s| s.kind == "node" && s.id == n.node.id.to_string() && s.removed)
        });
        for node in &mut machine.nodes {
            if let Some(setting) = settings
                .iter()
                .find(|s| s.kind == "node" && s.id == node.node.id.to_string())
            {
                if let Some(label) = &setting.label {
                    node.name = label.clone();
                }
                if let Some(max) = setting.max_agents {
                    node.slots.total = Some(node.slots.total.map_or(max, |v| v.min(max)));
                    node.slots.idle = node
                        .slots
                        .total
                        .map(|v| v.saturating_sub(node.slots.occupied));
                }
            }
            let capacity = management::capacity(&mut conn, node.node.id)
                .await
                .map_err(db)?;
            if capacity.full() {
                node.slots.idle = Some(0);
                node.holding.push(HoldReason {
                    code: HoldCode::AgentMaximumReached,
                    message: "Configured agent maximum reached".into(),
                    remedy: Some("Increase the maximum or wait for an agent to finish".into()),
                });
            }
        }
        let total: Option<u32> = machine
            .nodes
            .iter()
            .try_fold(0u32, |sum, n| n.slots.total.map(|v| sum.saturating_add(v)));
        machine.agent_max = setting.and_then(|s| s.max_agents).or(total);
    }
    swarm
        .machines
        .sort_by(|a, b| (&a.label, &a.id).cmp(&(&b.label, &b.id)));
    Ok(swarm)
}

fn db(error: aq_machine_control::Error) -> sqlx::Error {
    match error {
        aq_machine_control::Error::Db(error) => error,
        _ => sqlx::Error::Protocol("worker settings unavailable".into()),
    }
}
