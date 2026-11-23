use aq_admission::scoped_presence::InventoryReceipt;
use aq_dashboard_contract::InventoryRejection;
use serde::Serialize;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use uuid::Uuid;

#[derive(Clone, Default)]
pub(crate) struct Observation {
    pub(super) accepted: Option<Acceptance>,
    pub(crate) rejection: Option<InventoryRejection>,
}

#[derive(Clone, Serialize)]
pub(super) struct Acceptance {
    recorded_at_ms: u64,
    expires_at_ms: u64,
}

#[derive(Clone, Default)]
pub(crate) struct Diagnostics(Arc<Mutex<HashMap<Uuid, Observation>>>);

impl Diagnostics {
    pub(crate) fn get(&self, node: Uuid) -> Observation {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&node)
            .cloned()
            .unwrap_or_default()
    }

    fn update(&self, node: Uuid, update: impl FnOnce(&mut Observation)) {
        let mut nodes = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if nodes.len() >= 4096 && !nodes.contains_key(&node) {
            if let Some(evicted) = nodes.keys().next().copied() {
                nodes.remove(&evicted);
            }
        }
        update(nodes.entry(node).or_default());
    }

    pub(super) fn accept(&self, node: Uuid, receipt: &InventoryReceipt) {
        self.update(node, |record| {
            if record
                .accepted
                .as_ref()
                .is_none_or(|old| old.recorded_at_ms <= receipt.recorded_at_ms)
            {
                record.accepted = Some(Acceptance {
                    recorded_at_ms: receipt.recorded_at_ms,
                    expires_at_ms: receipt.expires_at_ms,
                });
            }
        });
    }

    pub(super) fn reject(&self, node: Uuid, refusal: InventoryRejection) {
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
}
