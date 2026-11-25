//! Apply owner settings after assembling the reported machine tree.
use aq_dashboard_contract::{HoldCode, HoldReason, Swarm};
use aq_machine_control::management;
use sqlx::PgPool;

pub async fn apply(pool: &PgPool, mut swarm: Swarm) -> Result<Swarm, sqlx::Error> {
    let mut conn = pool.acquire().await?;
    let settings = management::settings(&mut conn).await.map_err(db)?;
    for machine in &mut swarm.machines {
        apply_machine(&mut conn, machine, &settings).await?;
    }
    swarm
        .machines
        .sort_by(|a, b| (&a.label, &a.id).cmp(&(&b.label, &b.id)));
    Ok(swarm)
}

async fn apply_machine(
    conn: &mut sqlx::PgConnection,
    machine: &mut aq_dashboard_contract::SwarmMachine,
    settings: &[management::Setting],
) -> sqlx::Result<()> {
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
        configure_node(node, settings);
        refresh_slots(conn, node).await?;
    }
    let total: Option<u32> = machine
        .nodes
        .iter()
        .try_fold(0u32, |sum, n| n.slots.total.map(|v| sum.saturating_add(v)));
    machine.agent_max = setting.and_then(|s| s.max_agents).or(total);
    Ok(())
}
fn configure_node(node: &mut aq_dashboard_contract::SwarmNode, settings: &[management::Setting]) {
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
}
async fn refresh_slots(
    conn: &mut sqlx::PgConnection,
    node: &mut aq_dashboard_contract::SwarmNode,
) -> sqlx::Result<()> {
    if aq_machine_control::slots::available(conn)
        .await
        .map_err(db)?
    {
        let slots = aq_machine_control::slots::view(conn, node.node.id)
            .await
            .map_err(db)?;
        node.slots.total = Some(slots.total);
        node.slots.occupied = slots.occupied;
        node.slots.idle = Some(slots.idle);
    }
    let capacity = management::capacity(conn, node.node.id).await.map_err(db)?;
    if capacity.full() {
        node.holding.push(HoldReason {
            code: HoldCode::AgentMaximumReached,
            message: "Configured agent maximum reached".into(),
            remedy: Some("Increase the maximum or wait for an agent to finish".into()),
        });
    }
    Ok(())
}
fn db(error: aq_machine_control::Error) -> sqlx::Error {
    match error {
        aq_machine_control::Error::Db(error) => error,
        _ => sqlx::Error::Protocol("worker settings unavailable".into()),
    }
}
