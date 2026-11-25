//! Disposable bounded replay. Peer authentication and scope authorization are
//! explicit inputs; no terminal renderer/barrier capture is qualified here.
use crate::{Access, Error, Ledger, Result, draft::Peer};
use aq_session_protocol::*;
use std::collections::{BTreeMap, VecDeque};
use uuid::Uuid;
const MAX_FRAMES: usize = 1024;
const MAX_GAPS: usize = 64;
pub struct Replay {
    terminal: TerminalIdentity,
    frames: VecDeque<OutputFrame>,
    bytes: usize,
    limit: usize,
    frame_limit: usize,
    high: u64,
    snapshot: Option<Snapshot>,
    snapshot_ids: BTreeMap<Uuid, Digest>,
    gaps: Vec<OutputGap>,
    observed_at_ms: u64,
    disconnected: bool,
}
pub struct View {
    pub snapshot: Option<Snapshot>,
    pub frames: Vec<OutputFrame>,
    pub gaps: Vec<OutputGap>,
    pub age_ms: u64,
    pub disconnected: bool,
    pub truncated: bool,
}
impl Replay {
    pub fn new(terminal: TerminalIdentity, limit: usize) -> Result<Self> {
        terminal.validate().map_err(|_| Error::Invalid)?;
        if limit == 0 || limit > MAX_REPLAY_BYTES {
            return Err(Error::Invalid);
        }
        Ok(Self {
            terminal,
            frames: VecDeque::new(),
            bytes: 0,
            limit,
            frame_limit: MAX_FRAME_BYTES,
            high: 0,
            snapshot: None,
            snapshot_ids: BTreeMap::new(),
            gaps: vec![],
            observed_at_ms: 0,
            disconnected: true,
        })
    }
    pub fn new_profiled(terminal: TerminalIdentity, policy: &TerminalPolicy) -> Result<Self> {
        crate::valid_policy(policy)?;
        let mut replay = Self::new(terminal, policy.replay_bytes as usize)?;
        replay.frame_limit = policy.max_frame_bytes as usize;
        Ok(replay)
    }
    fn peer(&self, p: &Peer, t: &TerminalIdentity) -> Result<()> {
        if *t != self.terminal || !p.matches(t) {
            Err(Error::Stale)
        } else {
            Ok(())
        }
    }
    fn gap(&mut self, first: u64, last: u64) {
        if first == 0 || last < first {
            return;
        }
        self.gaps.push(OutputGap {
            terminal: self.terminal.clone(),
            first_missing: first,
            last_missing: last,
        });
        self.gaps.sort_by_key(|v| v.first_missing);
        let mut merged: Vec<OutputGap> = vec![];
        for gap in self.gaps.drain(..) {
            if let Some(old) = merged.last_mut()
                && gap.first_missing <= old.last_missing.saturating_add(1)
            {
                old.last_missing = old.last_missing.max(gap.last_missing);
            } else {
                merged.push(gap)
            }
        }
        while merged.len() > MAX_GAPS {
            let first = merged.remove(0);
            merged[0].first_missing = first.first_missing;
        }
        self.gaps = merged;
    }
    fn trim(&mut self) {
        let state = self.snapshot.as_ref().map_or(0, |s| s.state.len());
        while self.bytes + state > self.limit || self.frames.len() > MAX_FRAMES {
            let Some(old) = self.frames.pop_front() else {
                break;
            };
            self.bytes -= old.bytes.len();
            self.gap(old.sequence, old.sequence);
        }
    }
    /// Validate replay identity before charging disposable output quotas.
    /// Snapshot-covered frames are intentionally retired by the barrier.
    pub fn is_new_frame(&self, p: &Peer, f: &OutputFrame) -> Result<bool> {
        f.validate().map_err(|_| Error::Invalid)?;
        if f.bytes.len() > self.frame_limit {
            return Err(Error::Invalid);
        }
        self.peer(p, &f.terminal)?;
        if let Some(old) = self.frames.iter().find(|v| v.sequence == f.sequence) {
            return if old == f {
                Ok(false)
            } else {
                Err(Error::Conflict)
            };
        }
        if self
            .snapshot
            .as_ref()
            .is_some_and(|s| f.sequence <= s.through_sequence)
        {
            return Ok(false);
        }
        if f.sequence <= self.high {
            return Err(Error::Stale);
        }
        if f.sequence == u64::MAX {
            return Err(Error::Capacity);
        }
        Ok(true)
    }
    pub fn frame(&mut self, p: &Peer, f: OutputFrame, now: u64) -> Result<bool> {
        if !self.is_new_frame(p, &f)? {
            return Ok(false);
        }
        if f.sequence > self.high + 1 {
            self.gap(self.high + 1, f.sequence - 1);
        }
        self.high = f.sequence;
        self.bytes += f.bytes.len();
        self.frames.push_back(f);
        self.observed_at_ms = now;
        self.disconnected = false;
        self.trim();
        Ok(true)
    }
    pub fn is_new_snapshot(&self, p: &Peer, s: &Snapshot) -> Result<bool> {
        s.validate().map_err(|_| Error::Invalid)?;
        self.peer(p, &s.terminal)?;
        let d = canonical_digest(s).map_err(|_| Error::Invalid)?;
        if let Some(old) = self.snapshot_ids.get(&s.snapshot_id) {
            return if *old == d {
                Ok(false)
            } else {
                Err(Error::Conflict)
            };
        }
        if self.snapshot_ids.len() >= 1024 || s.state.len() > self.limit {
            return Err(Error::Capacity);
        }
        if s.through_sequence == u64::MAX {
            return Err(Error::Capacity);
        }
        if self
            .snapshot
            .as_ref()
            .is_some_and(|old| s.through_sequence < old.through_sequence)
        {
            return Err(Error::Stale);
        }
        Ok(true)
    }
    pub fn snapshot(&mut self, p: &Peer, s: Snapshot, now: u64) -> Result<bool> {
        if !self.is_new_snapshot(p, &s)? {
            return Ok(false);
        }
        let d = canonical_digest(&s).map_err(|_| Error::Invalid)?;
        let barrier = s.through_sequence;
        self.snapshot_ids.insert(s.snapshot_id, d);
        self.frames.retain(|f| f.sequence > barrier);
        self.bytes = self.frames.iter().map(|f| f.bytes.len()).sum();
        self.gaps.retain(|g| g.last_missing > barrier);
        for g in &mut self.gaps {
            g.first_missing = g.first_missing.max(barrier + 1);
        }
        self.high = self.high.max(barrier);
        self.snapshot = Some(s);
        self.observed_at_ms = now;
        self.disconnected = false;
        self.trim();
        Ok(true)
    }
    pub fn peer_gap(&mut self, p: &Peer, g: &OutputGap, now: u64) -> Result<()> {
        g.validate().map_err(|_| Error::Invalid)?;
        self.peer(p, &g.terminal)?;
        if g.last_missing == u64::MAX {
            return Err(Error::Capacity);
        }
        let start = g
            .first_missing
            .max(self.snapshot.as_ref().map_or(1, |s| s.through_sequence + 1));
        self.gap(start, g.last_missing);
        self.high = self.high.max(g.last_missing);
        self.observed_at_ms = now;
        Ok(())
    }
    pub fn disconnect(&mut self) {
        self.disconnected = true;
    }
    pub fn view(
        &self,
        ledger: &Ledger,
        a: &Access,
        subscription: Uuid,
        after: u64,
        now: u64,
    ) -> Result<View> {
        if ledger.terminal() != &self.terminal {
            return Err(Error::Stale);
        }
        ledger.authorize_watch(a, subscription, now)?;
        Ok(self.view_after(after, now))
    }

    fn view_after(&self, after: u64, now: u64) -> View {
        let barrier = self
            .snapshot
            .as_ref()
            .map_or(after, |s| s.through_sequence.max(after));
        let gaps: Vec<_> = self
            .gaps
            .iter()
            .filter(|g| g.last_missing > barrier)
            .cloned()
            .collect();
        View {
            snapshot: self.snapshot.clone(),
            frames: self
                .frames
                .iter()
                .filter(|f| f.sequence > barrier)
                .cloned()
                .collect(),
            truncated: !gaps.is_empty() || self.snapshot.is_none(),
            gaps,
            age_ms: now.saturating_sub(self.observed_at_ms),
            disconnected: self.disconnected,
        }
    }
    pub fn retained_bytes(&self) -> usize {
        self.bytes + self.snapshot.as_ref().map_or(0, |s| s.state.len())
    }
}
/// Pure reservation model. Actual executors/HTTP/pools need integration tests.
pub struct Capacity {
    terminal: Slots,
    execution: Slots,
}

struct Slots {
    used: usize,
    limit: usize,
}

impl Slots {
    fn new(limit: usize) -> Result<Self> {
        if limit == 0 {
            return Err(Error::Invalid);
        }
        Ok(Self { used: 0, limit })
    }
}
#[derive(Clone, Copy)]
pub enum Class {
    Terminal,
    Execution,
}

impl Capacity {
    pub fn new(terminal_limit: usize, execution_limit: usize) -> Result<Self> {
        Ok(Self {
            terminal: Slots::new(terminal_limit)?,
            execution: Slots::new(execution_limit)?,
        })
    }

    fn slots(&mut self, class: Class) -> &mut Slots {
        match class {
            Class::Terminal => &mut self.terminal,
            Class::Execution => &mut self.execution,
        }
    }

    pub fn acquire(&mut self, class: Class) -> Result<()> {
        let slots = self.slots(class);
        if slots.used >= slots.limit {
            return Err(Error::Capacity);
        }
        slots.used += 1;
        Ok(())
    }

    pub fn release(&mut self, class: Class) -> Result<()> {
        let slots = self.slots(class);
        slots.used = slots.used.checked_sub(1).ok_or(Error::Transition)?;
        Ok(())
    }
}
impl std::fmt::Debug for View {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplayView")
            .field("frames", &self.frames.len())
            .field("gaps", &self.gaps.len())
            .field("disconnected", &self.disconnected)
            .field("truncated", &self.truncated)
            .finish_non_exhaustive()
    }
}
