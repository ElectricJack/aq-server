//! Worker service state per node and machine control per key (spec 0007 phase 1).
use aq_dashboard_contract::{
    MachineDesired, OwnerAvailability, OwnerAvailabilityState, ServiceState,
};
use aq_machine_control::{self as mc, DesiredState};
use std::collections::BTreeMap;
use uuid::Uuid;

pub struct ServiceFact {
    pub state: ServiceState,
    pub since_ms: u64,
}
pub struct ControlFact {
    pub desired: MachineDesired,
    pub generation: u64,
    pub since_ms: u64,
    pub pending_handoffs: u32,
}

fn db(e: mc::Error) -> sqlx::Error {
    match e {
        mc::Error::Db(e) => e,
        _ => sqlx::Error::Protocol("machine control".into()),
    }
}

/// Every node that holds a key reports its machine's desired state; legacy
/// nodes are absent (unreported). Empty on a store before the machine tables.
pub async fn states(
    conn: &mut sqlx::PgConnection,
    nodes: &[Uuid],
) -> sqlx::Result<BTreeMap<Uuid, ServiceFact>> {
    if !mc::has_format(conn).await.map_err(db)? {
        return Ok(BTreeMap::new());
    }
    Ok(mc::node_services(conn, nodes)
        .await
        .map_err(db)?
        .into_iter()
        .map(|n| {
            let state = match n.desired {
                DesiredState::Running => ServiceState::Running,
                DesiredState::Draining => ServiceState::Draining,
                DesiredState::Stopped => ServiceState::Stopped,
            };
            (
                n.node_id,
                ServiceFact {
                    state,
                    since_ms: n.since_ms,
                },
            )
        })
        .collect())
}

/// One entry per machine key held by these nodes.
pub async fn controls(
    conn: &mut sqlx::PgConnection,
    nodes: &[Uuid],
) -> sqlx::Result<BTreeMap<String, ControlFact>> {
    if !mc::has_format(conn).await.map_err(db)? {
        return Ok(BTreeMap::new());
    }
    let services = mc::node_services(conn, nodes).await.map_err(db)?;
    let keys: Vec<String> = services.iter().map(|n| n.machine_key.clone()).collect();
    let pending = mc::pending_counts(conn, &keys).await.map_err(db)?;
    Ok(services
        .into_iter()
        .map(|n| {
            let desired = match n.desired {
                DesiredState::Running => MachineDesired::Running,
                DesiredState::Draining => MachineDesired::Draining,
                DesiredState::Stopped => MachineDesired::Stopped,
            };
            let fact = ControlFact {
                desired,
                generation: n.generation,
                since_ms: n.since_ms,
                pending_handoffs: pending.get(&n.machine_key).copied().unwrap_or(0),
            };
            (n.machine_key, fact)
        })
        .collect())
}

/// The owner's availability switch; `unset` at generation 1 before the machine tables.
pub async fn owner_availability(conn: &mut sqlx::PgConnection) -> sqlx::Result<OwnerAvailability> {
    let o = match mc::has_format(conn).await.map_err(db)? {
        true => Some(mc::owner_availability(conn).await.map_err(db)?),
        false => None,
    };
    let state = match o.as_ref().map(|o| o.state) {
        Some(mc::Availability::On) => OwnerAvailabilityState::On,
        Some(mc::Availability::Off) => OwnerAvailabilityState::Off,
        _ => OwnerAvailabilityState::Unset,
    };
    let since = o
        .as_ref()
        .and_then(|o| o.updated_at_ms)
        .map(crate::timestamp);
    Ok(OwnerAvailability {
        state,
        generation: o.map_or(1, |o| o.generation).to_string(),
        since: since
            .transpose()
            .map_err(|_| sqlx::Error::Protocol("time".into()))?,
    })
}
