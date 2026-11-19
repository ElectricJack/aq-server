//! Cache only compact, fully verified immutable facts. Current grants, roles,
//! credential health and provider availability never enter this structure.
use aq_session_protocol::{Digest, LaunchBinding, Validate};
use aq_worker_protocol::{GraphBinding, SourceBinding};
use serde::Serialize;
use std::collections::{BTreeMap, VecDeque};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Key {
    pub attempt_id: Uuid,
    pub decision_id: Uuid,
    pub source_observation_id: Uuid,
    pub execution_spec_digest: String,
    pub launch_manifest_digest: Digest,
    pub selection_digest: Digest,
}
#[derive(Debug, Clone, Serialize)]
pub struct Facts {
    pub binding: LaunchBinding,
    pub project_id: Uuid,
    pub task_id: Uuid,
    pub graph: GraphBinding,
    pub terminal_policy: aq_session_protocol::TerminalPolicy,
    pub source: SourceBinding,
}
/// FIFO keeps both retention work and accounted bytes bounded. Oversized facts
/// are verified on a cold path but never cached. The immutable-row SQL guards
/// and a fresh exact key lookup are mandatory prerequisites for a hit.
#[derive(Default)]
pub struct VerifiedCache {
    entries: BTreeMap<Key, (Facts, usize)>,
    order: VecDeque<Key>,
    bytes: usize,
    hits: u64,
    misses: u64,
}
impl VerifiedCache {
    pub const MAX_ENTRIES: usize = 1024;
    pub const MAX_BYTES: usize = 16 * 1024 * 1024;
    pub const MAX_ENTRY_BYTES: usize = 65_536;
    pub fn get(&mut self, key: &Key) -> Option<Facts> {
        let result = self.entries.get(key).map(|(facts, _)| facts.clone());
        if result.is_some() {
            self.hits = self.hits.saturating_add(1);
        } else {
            self.misses = self.misses.saturating_add(1);
        }
        result
    }
    pub fn stats(&self) -> (usize, usize, u64, u64) {
        (self.entries.len(), self.bytes, self.hits, self.misses)
    }
    pub fn insert_verified(&mut self, key: Key, facts: Facts) -> bool {
        if facts.binding.validate().is_err()
            || key.attempt_id != facts.binding.attempt_id
            || key.launch_manifest_digest != facts.binding.launch_manifest_digest
            || key.execution_spec_digest != facts.binding.execution_spec_digest.as_str()
            || key.decision_id.is_nil()
            || key.source_observation_id.is_nil()
            || key.execution_spec_digest.len() != 64
            || !key
                .execution_spec_digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return false;
        }
        if self.entries.contains_key(&key) {
            return true;
        }
        let Ok(encoded) = serde_json::to_vec(&facts) else {
            return false;
        };
        // Declared logical byte budget: serialized facts, two serialized key
        // copies and a fixed bookkeeping allowance. This is not measured heap
        // allocation/RSS; allocator and per-request clone overhead are extra.
        let Ok(key_bytes) = serde_json::to_vec(&key) else {
            return false;
        };
        let Some(size) = encoded
            .len()
            .checked_add(key_bytes.len() * 2)
            .and_then(|n| n.checked_add(512))
        else {
            return false;
        };
        if size > Self::MAX_ENTRY_BYTES {
            return false;
        }
        while self.entries.len() >= Self::MAX_ENTRIES || self.bytes + size > Self::MAX_BYTES {
            let Some(old) = self.order.pop_front() else {
                return false;
            };
            if let Some((_, bytes)) = self.entries.remove(&old) {
                self.bytes -= bytes;
            }
        }
        self.bytes += size;
        self.order.push_back(key.clone());
        self.entries.insert(key, (facts, size));
        true
    }
    pub fn retained(&self) -> (usize, usize) {
        (self.entries.len(), self.bytes)
    }
}
