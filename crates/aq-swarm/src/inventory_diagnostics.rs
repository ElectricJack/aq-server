//! The latest accepted and rejected capability report per worker node, held in
//! memory by this server process for diagnosis (lost on restart by design).
use aq_dashboard_contract::{InventoryRejection, Swarm};
use serde::Serialize;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use uuid::Uuid;

/// Nodes remembered at most; a new node past this evicts an arbitrary one.
const MAX_NODES: usize = 4096;

#[derive(Clone, Default)]
pub struct Observation {
    pub accepted: Option<Acceptance>,
    pub rejection: Option<InventoryRejection>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Acceptance {
    pub recorded_at_ms: u64,
    pub expires_at_ms: u64,
}

#[derive(Clone, Default)]
pub struct Diagnostics(Arc<Mutex<HashMap<Uuid, Observation>>>);

impl Diagnostics {
    pub fn get(&self, node: Uuid) -> Observation {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&node)
            .cloned()
            .unwrap_or_default()
    }

    fn update(&self, node: Uuid, update: impl FnOnce(&mut Observation)) {
        let mut nodes = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if nodes.len() >= MAX_NODES
            && !nodes.contains_key(&node)
            && let Some(evicted) = nodes.keys().next().copied()
        {
            nodes.remove(&evicted);
        }
        update(nodes.entry(node).or_default());
    }

    pub fn accept(&self, node: Uuid, accepted: Acceptance) {
        self.update(node, |record| {
            if record
                .accepted
                .as_ref()
                .is_none_or(|old| old.recorded_at_ms <= accepted.recorded_at_ms)
            {
                record.accepted = Some(accepted);
            }
        });
    }

    pub fn reject(&self, node: Uuid, refusal: InventoryRejection) {
        self.update(node, |record| {
            if record
                .rejection
                .as_ref()
                .is_none_or(|old| old.observed_at_ms <= refusal.observed_at_ms)
            {
                record.rejection = Some(refusal);
            }
        });
    }

    /// Sets each node's latest rejection on an assembled swarm read.
    pub fn annotate(&self, swarm: &mut Swarm) {
        for node in swarm.machines.iter_mut().flat_map(|m| m.nodes.iter_mut()) {
            node.inventory_rejection = self.get(node.node.id).rejection;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refusal(at: u64, stage: &str) -> InventoryRejection {
        InventoryRejection {
            request_id: Uuid::nil(),
            observed_at_ms: at,
            stage: stage.into(),
            status: 409,
            code: "conflict".into(),
        }
    }

    #[test]
    fn keeps_the_newest_report_of_each_kind_and_bounds_the_nodes() {
        let d = Diagnostics::default();
        let node = Uuid::from_u128(1);
        assert!(d.get(node).accepted.is_none() && d.get(node).rejection.is_none());
        d.reject(node, refusal(20, "commit_inventory"));
        d.reject(node, refusal(10, "validate"));
        assert_eq!(d.get(node).rejection.unwrap().stage, "commit_inventory");
        let at = |recorded_at_ms| Acceptance {
            recorded_at_ms,
            expires_at_ms: recorded_at_ms + 5,
        };
        d.accept(node, at(30));
        d.accept(node, at(25));
        assert_eq!(d.get(node).accepted, Some(at(30)));
        assert!(
            d.get(node).rejection.is_some(),
            "an acceptance keeps the rejection"
        );
        for n in 2..=MAX_NODES as u128 + 1 {
            d.reject(Uuid::from_u128(n), refusal(1, "validate"));
        }
        assert_eq!(d.0.lock().unwrap().len(), MAX_NODES);
    }
}
