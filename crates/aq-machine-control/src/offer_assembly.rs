//! Process-local work bounds. These are not execution grants or task ownership.
use std::{collections::BTreeSet, sync::Mutex};
use uuid::Uuid;

pub struct Gate {
    maximum: usize,
    active: Mutex<BTreeSet<(Uuid, u32)>>,
}
impl Gate {
    pub fn new(maximum: usize) -> Self {
        Self {
            maximum,
            active: Mutex::new(BTreeSet::new()),
        }
    }
    pub fn enter(&self, node: Uuid) -> Option<Permit<'_>> {
        self.enter_slot(node, 1)
    }
    pub fn enter_slot(&self, node: Uuid, slot: u32) -> Option<Permit<'_>> {
        let mut active = self.active.lock().ok()?;
        if active.len() >= self.maximum || !active.insert((node, slot)) {
            return None;
        }
        Some(Permit {
            gate: self,
            node,
            slot,
        })
    }
}
/// The server's assemblers: one per node slot, `offer_assemblies` in all.
pub fn enter(node: Uuid, slot: u32) -> Option<Permit<'static>> {
    static ASSEMBLERS: std::sync::OnceLock<Gate> = std::sync::OnceLock::new();
    let sized = || Gate::new(crate::limits::current().offer_assemblies);
    ASSEMBLERS.get_or_init(sized).enter_slot(node, slot)
}
/// One assembler per task in this server: the slots of a node, and several
/// nodes, poll at the same time and would otherwise all assemble the task at
/// the head of the queue, contend for its one source export and lose at
/// publication. A slot that finds the task taken goes on with the next task.
pub fn claim_task(task: Uuid) -> Option<TaskClaim> {
    // The claim is built only for a new entry, after the set's lock is released:
    // dropping a claim takes that lock and removes the entry.
    let inserted = TASKS.lock().ok()?.insert(task);
    inserted.then(|| TaskClaim(task))
}
static TASKS: Mutex<BTreeSet<Uuid>> = Mutex::new(BTreeSet::new());
pub struct TaskClaim(Uuid);
impl Drop for TaskClaim {
    fn drop(&mut self) {
        if let Ok(mut tasks) = TASKS.lock() {
            tasks.remove(&self.0);
        }
    }
}
pub struct Permit<'a> {
    gate: &'a Gate,
    node: Uuid,
    slot: u32,
}
impl Drop for Permit<'_> {
    fn drop(&mut self) {
        if let Ok(mut active) = self.gate.active.lock() {
            active.remove(&(self.node, self.slot));
        }
    }
}
/// Leave enough validity for the next poll; this never extends a report.
pub fn deliverable(snapshot: &aq_selection::CapabilitySnapshot, now: u64) -> bool {
    window(snapshot.observed_at_ms, snapshot.expires_at_ms, now)
}
fn window(observed: u64, expires: u64, now: u64) -> bool {
    observed <= now
        && now < expires
        && expires - now >= expires.saturating_sub(observed).min(30000) / 2
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn late_assembly_waits_for_a_fresh_report_without_extending_validity() {
        assert!(window(1000, 31000, 1000));
        assert!(window(1000, 31000, 16000));
        assert!(!window(1000, 31000, 16001));
        assert!(!window(1000, 31000, 31000));
        assert!(!window(1000, 31000, 999));
        assert!(window(1000, 2000, 1400));
        assert!(!window(1000, 2000, 1600));
    }
    #[test]
    fn a_task_has_one_assembler_until_its_claim_is_dropped() {
        let (a, b) = (Uuid::from_u128(71), Uuid::from_u128(72));
        let first = claim_task(a).unwrap();
        assert!(claim_task(a).is_none());
        let _other = claim_task(b).unwrap();
        drop(first);
        assert!(claim_task(a).is_some());
    }
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
