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
        if !key.admits(&facts.binding) {
            return false;
        }
        if self.entries.contains_key(&key) {
            return true;
        }
        let Some(size) = Self::entry_size(&key, &facts) else {
            return false;
        };
        if !self.make_room(size) {
            return false;
        }
        self.bytes += size;
        self.order.push_back(key.clone());
        self.entries.insert(key, (facts, size));
        true
    }
    fn entry_size(key: &Key, facts: &Facts) -> Option<usize> {
        // Serialized facts, two key copies and bookkeeping approximate retained bytes.
        let facts_bytes = serde_json::to_vec(facts).ok()?.len();
        let key_bytes = serde_json::to_vec(key).ok()?.len();
        facts_bytes
            .checked_add(key_bytes.checked_mul(2)?)?
            .checked_add(512)
            .filter(|size| *size <= Self::MAX_ENTRY_BYTES)
    }
    fn make_room(&mut self, size: usize) -> bool {
        while self.entries.len() >= Self::MAX_ENTRIES || self.bytes + size > Self::MAX_BYTES {
            let Some(old) = self.order.pop_front() else {
                return false;
            };
            if let Some((_, bytes)) = self.entries.remove(&old) {
                self.bytes -= bytes;
            }
        }
        true
    }
    pub fn retained(&self) -> (usize, usize) {
        (self.entries.len(), self.bytes)
    }
}

impl Key {
    fn admits(&self, binding: &LaunchBinding) -> bool {
        binding.validate().is_ok()
            && self.attempt_id == binding.attempt_id
            && self.launch_manifest_digest == binding.launch_manifest_digest
            && self.execution_spec_digest == binding.execution_spec_digest.as_str()
            && self.valid_identifiers()
    }

    fn valid_identifiers(&self) -> bool {
        !self.decision_id.is_nil()
            && !self.source_observation_id.is_nil()
            && self.execution_spec_digest.len() == 64
            && self
                .execution_spec_digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }
}
