//! The phase 1 seam (spec 0007): worker service state per node.
use aq_dashboard_contract::ServiceState;
use std::collections::BTreeMap;
use uuid::Uuid;

pub struct ServiceFact {
    pub state: ServiceState,
    pub since_ms: u64,
}

/// Phase 1 (spec 0007) fills this from the worker service state.
pub async fn states(
    _conn: &mut sqlx::PgConnection,
    _nodes: &[Uuid],
) -> sqlx::Result<BTreeMap<Uuid, ServiceFact>> {
    Ok(BTreeMap::new())
}
