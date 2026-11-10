//! Process-local work bounds. These are not execution grants or task ownership.
use std::{collections::BTreeSet, sync::Mutex};
use uuid::Uuid;

pub struct Gate {
    maximum: usize,
    active: Mutex<BTreeSet<Uuid>>,
}
impl Gate {
    pub fn new(maximum: usize) -> Self {
        Self {
            maximum,
            active: Mutex::new(BTreeSet::new()),
        }
    }
    pub fn enter(&self, node: Uuid) -> Option<Permit<'_>> {
        let mut active = self.active.lock().ok()?;
        if active.len() >= self.maximum || !active.insert(node) {
            return None;
        }
        Some(Permit { gate: self, node })
    }
}
pub struct Permit<'a> {
    gate: &'a Gate,
    node: Uuid,
}
impl Drop for Permit<'_> {
    fn drop(&mut self) {
        if let Ok(mut active) = self.gate.active.lock() {
            active.remove(&self.node);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_one_assembler_per_node_and_bounded_total_until_owner_drops() {
        let gate = Gate::new(2);
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        let c = Uuid::from_u128(3);
        let first = gate.enter(a).unwrap();
        assert!(gate.enter(a).is_none());
        let second = gate.enter(b).unwrap();
        assert!(gate.enter(c).is_none());
        drop(first);
        let third = gate.enter(c).unwrap();
        assert!(gate.enter(a).is_none());
        drop(second);
        drop(third);
        assert!(gate.enter(a).is_some());
    }
}
