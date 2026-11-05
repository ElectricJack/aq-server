//! Disposable bounded replay and poll wakeups. No execution authority or SQL
//! connections are held by these buffers or during a terminal long-poll wait.
use crate::error::AppError;
use aq_session_protocol::*;
use aq_terminal_relay::{
    Access, Ledger,
    output::{Replay, View},
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, Weak},
};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;
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
struct PollState {
    slot: Arc<Semaphore>,
    output_slot: Arc<Semaphore>,
    wake: Notify,
    #[cfg(feature = "test-support")]
    waiting: std::sync::atomic::AtomicBool,
}
pub struct Poll {
    state: Arc<PollState>,
    _permit: OwnedSemaphorePermit,
    #[cfg(feature = "test-support")]
    output: bool,
}
impl Poll {
    pub async fn wait(&self, hold: u32) {
        #[cfg(feature = "test-support")]
        if !self.output {
            self.state
                .waiting
                .store(true, std::sync::atomic::Ordering::Release);
        }
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(hold as u64),
            self.state.wake.notified(),
        )
        .await;
        #[cfg(feature = "test-support")]
        if !self.output {
            self.state
                .waiting
                .store(false, std::sync::atomic::Ordering::Release);
        }
    }
}
impl Drop for Poll {
    fn drop(&mut self) {
        #[cfg(feature = "test-support")]
        if !self.output {
            self.state
                .waiting
                .store(false, std::sync::atomic::Ordering::Release);
        }
    }
}
type Polls = BTreeMap<(Uuid, Option<Uuid>), Weak<PollState>>;

#[derive(Default)]
pub struct Runtime {
    buffers: Mutex<Buffers>,
    polls: Mutex<Polls>,
}
fn fail() -> AppError {
    AppError::Internal
}
impl Runtime {
    pub fn reserve(
        &self,
        t: &TerminalIdentity,
        policy: &TerminalPolicy,
        until: u64,
        now: u64,
    ) -> Result<(), AppError> {
        let SessionIdentity::Remote { node_id, .. } = t.session else {
            return Err(AppError::Conflict(
                "central terminal authority unsupported".into(),
            ));
        };
        let mut b = self.buffers.lock().map_err(|_| fail())?;
        b.items.retain(|_, v| now < v.until);
        let active: std::collections::BTreeSet<_> = b.items.values().map(|v| v.node).collect();
        b.node_rate.retain(|id, _| active.contains(id));
        if let Some(entry) = b.items.get_mut(&t.terminal_id) {
            if entry.terminal != *t || entry.policy != *policy {
                return Err(AppError::Conflict("terminal epoch changed".into()));
            }
            entry.until = entry.until.max(until);
            return Ok(());
        }
        if b.items.len() >= 128 || b.items.values().filter(|v| v.node == node_id).count() >= 32 {
            return Err(AppError::IdentityUnavailable(
                "terminal replay capacity exhausted".into(),
            ));
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
                ring: Replay::new_profiled(t.clone(), policy).map_err(|_| fail())?,
                rate: Rate::new(262144, now),
            },
        );
        Ok(())
    }
    pub fn view(
        &self,
        l: &Ledger,
        a: &Access,
        subscription: Uuid,
        after: u64,
        now: u64,
    ) -> Result<Option<View>, AppError> {
        let b = self.buffers.lock().map_err(|_| fail())?;
        b.items
            .get(&l.terminal().terminal_id)
            .map(|v| {
                v.ring
                    .view(l, a, subscription, after, now)
                    .map_err(super::persistence::relay)
            })
            .transpose()
    }
    pub fn frame(
        &self,
        peer: &aq_terminal_relay_protocol::Peer,
        frame: OutputFrame,
        now: u64,
    ) -> Result<Option<OutputGap>, AppError> {
        let mut b = self.buffers.lock().map_err(|_| fail())?;
        // Output is disposable. A worker may finish an upload after its watch
        // expired (or before anyone watches). Authority is checked by upload_in;
        // absence of a replay consumer must not cause an endless upload retry.
        let Some(buffer) = b.items.get(&frame.terminal.terminal_id) else {
            return Ok(None);
        };
        if now >= buffer.until {
            b.items.remove(&frame.terminal.terminal_id);
            return Ok(None);
        }
        let node = buffer.node;
        let mut nr = b.node_rate.get(&node).cloned().ok_or_else(fail)?;
        let entry = b
            .items
            .get_mut(&frame.terminal.terminal_id)
            .ok_or_else(fail)?;
        if entry.terminal != frame.terminal || now >= entry.until {
            return Err(AppError::Conflict("terminal subscription expired".into()));
        }
        if frame.bytes.len() > entry.policy.max_frame_bytes as usize {
            return Err(AppError::BadRequest(
                "frame exceeds sealed terminal policy".into(),
            ));
        }
        if !entry
            .ring
            .is_new_frame(peer, &frame)
            .map_err(super::persistence::relay)?
        {
            return Ok(None);
        }
        let mut sr = entry.rate.clone();
        if !sr.take(frame.bytes.len(), now) || !nr.take(frame.bytes.len(), now) {
            let gap = OutputGap {
                terminal: frame.terminal.clone(),
                first_missing: frame.sequence,
                last_missing: frame.sequence,
            };
            entry
                .ring
                .peer_gap(peer, &gap, now)
                .map_err(super::persistence::relay)?;
            return Ok(Some(gap));
        }
        entry
            .ring
            .frame(peer, frame, now)
            .map_err(super::persistence::relay)?;
        entry.rate = sr;
        b.node_rate.insert(node, nr);
        Ok(None)
    }
    pub fn snapshot(
        &self,
        peer: &aq_terminal_relay_protocol::Peer,
        snapshot: Snapshot,
        now: u64,
    ) -> Result<(), AppError> {
        let mut b = self.buffers.lock().map_err(|_| fail())?;
        let Some(buffer) = b.items.get(&snapshot.terminal.terminal_id) else {
            return Ok(());
        };
        if now >= buffer.until {
            b.items.remove(&snapshot.terminal.terminal_id);
            return Ok(());
        }
        let node = buffer.node;
        let mut nr = b.node_rate.get(&node).cloned().ok_or_else(fail)?;
        let entry = b
            .items
            .get_mut(&snapshot.terminal.terminal_id)
            .ok_or_else(fail)?;
        if entry.terminal != snapshot.terminal || now >= entry.until {
            return Err(AppError::Conflict("terminal subscription expired".into()));
        }
        if !entry
            .ring
            .is_new_snapshot(peer, &snapshot)
            .map_err(super::persistence::relay)?
        {
            return Ok(());
        }
        let mut sr = entry.rate.clone();
        if !sr.take(snapshot.state.len(), now) || !nr.take(snapshot.state.len(), now) {
            return Err(AppError::IdentityUnavailable(
                "terminal snapshot output quota exhausted".into(),
            ));
        }
        entry
            .ring
            .snapshot(peer, snapshot, now)
            .map_err(super::persistence::relay)?;
        entry.rate = sr;
        b.node_rate.insert(node, nr);
        Ok(())
    }
    pub fn gap(
        &self,
        peer: &aq_terminal_relay_protocol::Peer,
        gap: &OutputGap,
        now: u64,
    ) -> Result<(), AppError> {
        let mut b = self.buffers.lock().map_err(|_| fail())?;
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
                .map_err(super::persistence::relay)?;
        }
        Ok(())
    }
    pub fn poll(&self, node: Uuid) -> Result<Poll, AppError> {
        self.poll_channel(node, None)
    }
    pub fn output_poll(&self, node: Uuid) -> Result<Poll, AppError> {
        self.output_poll_channel(node, None)
    }
    pub fn output_poll_channel(&self, node: Uuid, attempt: Option<Uuid>) -> Result<Poll, AppError> {
        self.poll_lane(node, attempt, true)
    }
    pub fn poll_channel(&self, node: Uuid, attempt: Option<Uuid>) -> Result<Poll, AppError> {
        self.poll_lane(node, attempt, false)
    }
    fn poll_lane(&self, node: Uuid, attempt: Option<Uuid>, output: bool) -> Result<Poll, AppError> {
        let key = (node, attempt);
        let mut polls = self.polls.lock().map_err(|_| fail())?;
        polls.retain(|_, v| v.strong_count() > 0);
        let state = match polls.get(&key).and_then(Weak::upgrade) {
            Some(v) => v,
            None => {
                if polls.len() >= 32 {
                    return Err(AppError::IdentityUnavailable(
                        "terminal poll capacity exhausted".into(),
                    ));
                }
                let v = Arc::new(PollState {
                    slot: Arc::new(Semaphore::new(1)),
                    output_slot: Arc::new(Semaphore::new(1)),
                    wake: Notify::new(),
                    #[cfg(feature = "test-support")]
                    waiting: Default::default(),
                });
                polls.insert(key, Arc::downgrade(&v));
                v
            }
        };
        // Control may long-poll while output delivery progresses independently.
        // Each lane admits one request, under the shared node/attempt channel cap.
        let slot = if output {
            &state.output_slot
        } else {
            &state.slot
        };
        let permit = slot.clone().try_acquire_owned().map_err(|_| {
            AppError::Conflict(format!(
                "one terminal {} poll per channel",
                if output { "output" } else { "control" }
            ))
        })?;
        Ok(Poll {
            state,
            _permit: permit,
            #[cfg(feature = "test-support")]
            output,
        })
    }
    #[cfg(feature = "test-support")]
    pub fn poll_waiting_for_testing(&self, node: Uuid) -> bool {
        self.polls
            .lock()
            .ok()
            .and_then(|p| p.get(&(node, None)).and_then(Weak::upgrade))
            .is_some_and(|p| p.waiting.load(std::sync::atomic::Ordering::Acquire))
    }
    pub fn wake(&self, node: Uuid) {
        if let Ok(polls) = self.polls.lock() {
            for ((owner, _), value) in polls.iter() {
                if *owner == node
                    && let Some(state) = value.upgrade()
                {
                    state.wake.notify_one();
                }
            }
        }
    }
}

/// Bounded derived scheduling cursor; authoritative state is always reloaded and
/// authenticated under the common node/attempt locks. No input is emitted here.
pub async fn maintenance(state: &crate::AppState, after: Uuid) -> Result<Uuid, AppError> {
    use super::persistence::*;
    use crate::execution::authority::{self, Policy};
    let state = state.terminal_view();
    let config = configured(&state)?;
    let owner = super::persistence::begin(&state).await?;
    owner.commit().await.map_err(|_| AppError::Internal)?;
    let bodies:Vec<String>=sqlx::query_scalar("SELECT identity::text FROM terminal_sessions WHERE terminal_id>$1 AND(runtime_id<>$2 OR pending_bytes>0) ORDER BY terminal_id LIMIT 16").bind(after).bind(config.runtime_id).fetch_all(&state.store.pool).await.map_err(|_|AppError::Internal)?;
    if bodies.is_empty() {
        return Ok(Uuid::nil());
    }
    let mut cursor = after;
    for body in bodies {
        let t: TerminalIdentity = serde_json::from_str(&body).map_err(|_| AppError::Internal)?;
        cursor = t.terminal_id;
        let result = async {
            let mut tx = super::persistence::begin(&state).await?;
            let view =
                authority::load_locked(&state, &mut tx, &t.session, Policy::TerminalHistorical)
                    .await?;
            let loaded = load(&state, &mut tx, &t, &view).await?;
            let recovering = loaded.runtime_id != config.runtime_id;
            let plan = if recovering {
                Ledger::recover(&loaded.ledger.encode().map_err(relay)?)
            } else {
                loaded.ledger.expire(now(&state))
            }
            .map_err(relay)?;
            if recovering || plan.preview().ledger_revision != loaded.ledger.revision() {
                let mut journal = SqlJournal {
                    state: &state,
                    tx: Some(tx),
                    operation: None,
                    peer_contact: false,
                    recovering,
                };
                plan.commit_async(&mut journal).await.map_err(relay)?;
            } else {
                tx.commit().await.map_err(|_| AppError::Internal)?;
            }
            Ok::<(), AppError>(())
        }
        .await;
        if result.is_err() {
            // Fixed message only: no ciphertext, plaintext, key material or serde detail.
            tracing::warn!(terminal_id=%t.terminal_id,"terminal maintenance refused stored state");
        }
    }
    Ok(cursor)
}
pub async fn run(state: crate::AppState, mut stop: tokio::sync::watch::Receiver<bool>) {
    if state.config.terminals.is_none() {
        return;
    }
    let mut cursor = Uuid::nil();
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(250));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            changed=stop.changed()=>{if changed.is_err()||*stop.borrow(){break;}},
            _=interval.tick()=>{
                match maintenance(&state,cursor).await {Ok(next)=>cursor=next,Err(AppError::IdentityUnavailable(_))=>break,Err(_)=>tracing::warn!("terminal maintenance unavailable")}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_lanes_share_the_node_cap_and_release_on_drop() {
        let runtime = Runtime::default();
        let nodes: Vec<_> = (0..33).map(|_| Uuid::new_v4()).collect();
        let mut controls: Vec<_> = nodes[..32]
            .iter()
            .map(|id| runtime.poll(*id).unwrap())
            .collect();
        let mut outputs: Vec<_> = nodes[..32]
            .iter()
            .map(|id| runtime.output_poll(*id).unwrap())
            .collect();
        assert!(matches!(runtime.poll(nodes[0]), Err(AppError::Conflict(_))));
        assert!(matches!(
            runtime.output_poll(nodes[0]),
            Err(AppError::Conflict(_))
        ));
        assert!(matches!(
            runtime.output_poll(nodes[32]),
            Err(AppError::IdentityUnavailable(_))
        ));
        drop(controls.pop());
        // An output permit alone still owns that node's capacity.
        assert!(runtime.poll(nodes[32]).is_err());
        drop(outputs.pop());
        let _next = runtime.poll(nodes[32]).unwrap();
        let _next_output = runtime.output_poll(nodes[32]).unwrap();
    }
}

#[cfg(test)]
mod delegated_poll_tests {
    use super::*;
    #[tokio::test]
    async fn delegated_polls_are_exclusive_per_attempt_and_share_node_wakeup() {
        let runtime = Runtime::default();
        let node = Uuid::new_v4();
        let attempt = Uuid::new_v4();
        let first = runtime.poll_channel(node, Some(attempt)).unwrap();
        let second = runtime.poll_channel(node, Some(Uuid::new_v4())).unwrap();
        let output = runtime.output_poll_channel(node, Some(attempt)).unwrap();
        assert!(runtime.output_poll_channel(node, Some(attempt)).is_err());
        assert!(runtime.poll_channel(node, Some(attempt)).is_err());
        assert!(runtime.poll(node).is_ok());
        assert!(runtime.output_poll(node).is_ok());
        runtime.wake(node);
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            first.state.wake.notified(),
        )
        .await
        .unwrap();
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            second.state.wake.notified(),
        )
        .await
        .unwrap();
        // Both live channels retain their own exclusive slots after a wake.
        assert_eq!(first.state.slot.available_permits(), 0);
        assert_eq!(second.state.slot.available_permits(), 0);
        assert_eq!(output.state.output_slot.available_permits(), 0);
        drop(first);
        assert!(runtime.poll_channel(node, Some(attempt)).is_ok());
        assert!(runtime.output_poll_channel(node, Some(attempt)).is_err());
        drop(output);
        assert!(runtime.output_poll_channel(node, Some(attempt)).is_ok());
    }
}
