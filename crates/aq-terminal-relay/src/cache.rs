//! Bounded, disposable terminal replay shared by authorized watches and peer uploads.
use crate::{
    Access, Ledger,
    api::ViewQuery,
    output::{Replay, View},
};
use aq_session_protocol::*;
use std::{collections::BTreeMap, sync::Mutex};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error(transparent)]
    Relay(crate::Error),
    #[error("terminal replay state unavailable")]
    Internal,
    #[error("terminal subscription expired")]
    Expired,
    #[error("central terminal authority unsupported")]
    Central,
    #[error("terminal epoch changed")]
    EpochChanged,
    #[error("terminal replay capacity exhausted")]
    Full,
    #[error("frame exceeds sealed terminal policy")]
    Oversized,
    #[error("terminal snapshot output quota exhausted")]
    SnapshotQuota,
}

#[derive(Clone)]
struct Rate {
    tokens: u64,
    remainder: u64,
    last: u64,
    capacity: u64,
}
impl Rate {
    fn new(capacity: u64, now: u64) -> Self {
        Self {
            tokens: capacity,
            remainder: 0,
            last: now,
            capacity,
        }
    }
    fn take(&mut self, bytes: usize, now: u64) -> bool {
        if now < self.last {
            return false;
        }
        let minted = (now - self.last) as u128 * self.capacity as u128 + self.remainder as u128;
        self.tokens = self.capacity.min(
            self.tokens
                .saturating_add((minted / 1000).min(u64::MAX as u128) as u64),
        );
        self.remainder = (minted % 1000) as u64;
        self.last = now;
        if bytes as u64 > self.tokens {
            return false;
        }
        self.tokens -= bytes as u64;
        true
    }
}
struct Buffer {
    terminal: TerminalIdentity,
    node: Uuid,
    policy: TerminalPolicy,
    until: u64,
    ring: Replay,
    rate: Rate,
}
#[derive(Default)]
struct Buffers {
    items: BTreeMap<Uuid, Buffer>,
    node_rate: BTreeMap<Uuid, Rate>,
}
impl Buffers {
    fn upload(
        &mut self,
        terminal: &TerminalIdentity,
        now: u64,
    ) -> Result<Option<(&mut Buffer, &mut Rate)>, CacheError> {
        let Some(buffer) = self.items.get(&terminal.terminal_id) else {
            return Ok(None);
        };
        if now >= buffer.until {
            self.items.remove(&terminal.terminal_id);
            return Ok(None);
        }
        let node_rate = self
            .node_rate
            .get_mut(&buffer.node)
            .ok_or(CacheError::Internal)?;
        let entry = self
            .items
            .get_mut(&terminal.terminal_id)
            .ok_or(CacheError::Internal)?;
        if entry.terminal != *terminal {
            return Err(CacheError::Expired);
        }
        Ok(Some((entry, node_rate)))
    }
}
pub struct Watch<'a> {
    ledger: &'a Ledger,
    access: Access,
    query: &'a ViewQuery,
    at: u64,
}
impl<'a> Watch<'a> {
    pub fn new(
        ledger: &'a Ledger,
        access: Access,
        query: &'a ViewQuery,
        at: u64,
    ) -> Result<Self, crate::Error> {
        ledger.authorize_watch(&access, query.subscription_id, at)?;
        Ok(Self {
            ledger,
            access,
            query,
            at,
        })
    }
}
impl Watch<'_> {
    pub fn access_expires_at(&self) -> u64 {
        self.access.valid_until_ms
    }

    pub fn expires_at(&self) -> Result<u64, CacheError> {
        self.ledger
            .summary()
            .subscriptions
            .into_iter()
            .find(|s| s.subscription_id == self.query.subscription_id)
            .map(|s| s.expires_at_ms)
            .ok_or(CacheError::Internal)
    }
}
#[derive(Default)]
pub struct ReplayCache {
    buffers: Mutex<Buffers>,
}
impl ReplayCache {
    pub fn reserve(&self, ledger: &Ledger, until: u64, now: u64) -> Result<(), CacheError> {
        let t = ledger.terminal();
        let policy = ledger.terminal_policy();
        let SessionIdentity::Remote { node_id, .. } = t.session else {
            return Err(CacheError::Central);
        };
        let mut b = self.buffers.lock().map_err(|_| CacheError::Internal)?;
        b.items.retain(|_, v| now < v.until);
        let active: std::collections::BTreeSet<_> = b.items.values().map(|v| v.node).collect();
        b.node_rate.retain(|id, _| active.contains(id));
        if let Some(entry) = b.items.get_mut(&t.terminal_id) {
            if entry.terminal != *t || entry.policy != *policy {
                return Err(CacheError::EpochChanged);
            }
            entry.until = entry.until.max(until);
            return Ok(());
        }
        if b.items.len() >= 128 || b.items.values().filter(|v| v.node == node_id).count() >= 32 {
            return Err(CacheError::Full);
        }
        b.node_rate
            .entry(node_id)
            .or_insert_with(|| Rate::new(2097152, now));
        b.items.insert(
            t.terminal_id,
            Buffer {
                terminal: t.clone(),
                node: node_id,
                policy: policy.clone(),
                until,
                ring: Replay::new_profiled(t.clone(), policy).map_err(|_| CacheError::Internal)?,
                rate: Rate::new(262144, now),
            },
        );
        Ok(())
    }
    pub fn view(&self, watch: &Watch<'_>) -> Result<Option<View>, CacheError> {
        let buffers = self.buffers.lock().map_err(|_| CacheError::Internal)?;
        buffers
            .items
            .get(&watch.ledger.terminal().terminal_id)
            .map(|v| {
                v.ring
                    .view(
                        watch.ledger,
                        &watch.access,
                        watch.query.subscription_id,
                        watch.query.after_sequence,
                        watch.at,
                    )
                    .map_err(CacheError::Relay)
            })
            .transpose()
    }
    pub fn frame(
        &self,
        peer: &aq_terminal_relay_protocol::Peer,
        frame: OutputFrame,
        now: u64,
    ) -> Result<Option<OutputGap>, CacheError> {
        let mut b = self.buffers.lock().map_err(|_| CacheError::Internal)?;
        // Output is disposable. A worker may finish an upload after its watch
        // expired (or before anyone watches). Authority is checked by upload_in;
        // absence of a replay consumer must not cause an endless upload retry.
        let Some((entry, node_rate)) = b.upload(&frame.terminal, now)? else {
            return Ok(None);
        };
        if frame.bytes.len() > entry.policy.max_frame_bytes as usize {
            return Err(CacheError::Oversized);
        }
        if !entry
            .ring
            .is_new_frame(peer, &frame)
            .map_err(CacheError::Relay)?
        {
            return Ok(None);
        }
        let mut sr = entry.rate.clone();
        let mut nr = node_rate.clone();
        if !sr.take(frame.bytes.len(), now) || !nr.take(frame.bytes.len(), now) {
            let gap = OutputGap {
                terminal: frame.terminal.clone(),
                first_missing: frame.sequence,
                last_missing: frame.sequence,
            };
            entry
                .ring
                .peer_gap(peer, &gap, now)
                .map_err(CacheError::Relay)?;
            return Ok(Some(gap));
        }
        entry
            .ring
            .frame(peer, frame, now)
            .map_err(CacheError::Relay)?;
        entry.rate = sr;
        *node_rate = nr;
        Ok(None)
    }
    pub fn snapshot(
        &self,
        peer: &aq_terminal_relay_protocol::Peer,
        snapshot: Snapshot,
        now: u64,
    ) -> Result<(), CacheError> {
        let mut b = self.buffers.lock().map_err(|_| CacheError::Internal)?;
        let Some((entry, node_rate)) = b.upload(&snapshot.terminal, now)? else {
            return Ok(());
        };
        if !entry
            .ring
            .is_new_snapshot(peer, &snapshot)
            .map_err(CacheError::Relay)?
        {
            return Ok(());
        }
        let mut sr = entry.rate.clone();
        let mut nr = node_rate.clone();
        if !sr.take(snapshot.state.len(), now) || !nr.take(snapshot.state.len(), now) {
            return Err(CacheError::SnapshotQuota);
        }
        entry
            .ring
            .snapshot(peer, snapshot, now)
            .map_err(CacheError::Relay)?;
        entry.rate = sr;
        *node_rate = nr;
        Ok(())
    }
    pub fn gap(
        &self,
        peer: &aq_terminal_relay_protocol::Peer,
        gap: &OutputGap,
        now: u64,
    ) -> Result<(), CacheError> {
        let mut b = self.buffers.lock().map_err(|_| CacheError::Internal)?;
        if b.items
            .get(&gap.terminal.terminal_id)
            .is_some_and(|entry| now >= entry.until)
        {
            b.items.remove(&gap.terminal.terminal_id);
            return Ok(());
        }
        if let Some(entry) = b.items.get_mut(&gap.terminal.terminal_id) {
            entry
                .ring
                .peer_gap(peer, gap, now)
                .map_err(CacheError::Relay)?;
        }
        Ok(())
    }
}
