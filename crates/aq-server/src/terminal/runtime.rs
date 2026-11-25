//! Disposable bounded replay and poll wakeups. No execution authority or SQL
//! connections are held by these buffers or during a terminal long-poll wait.
use crate::error::AppError;
use aq_session_protocol::*;
use aq_terminal_relay::{
    Access, Ledger,
    api::ViewQuery,
    cache::{CacheError, ReplayCache, Watch},
    output::View,
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, Weak},
};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;
struct PollState {
    slot: Arc<Semaphore>,
    wake: Notify,
    #[cfg(feature = "test-support")]
    waiting: std::sync::atomic::AtomicBool,
}
pub struct Poll {
    state: Arc<PollState>,
    _permit: OwnedSemaphorePermit,
}
impl Poll {
    pub async fn wait(&self, hold: u32) {
        #[cfg(feature = "test-support")]
        self.state
            .waiting
            .store(true, std::sync::atomic::Ordering::Release);
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(hold as u64),
            self.state.wake.notified(),
        )
        .await;
        #[cfg(feature = "test-support")]
        self.state
            .waiting
            .store(false, std::sync::atomic::Ordering::Release);
    }
}
impl Drop for Poll {
    fn drop(&mut self) {
        #[cfg(feature = "test-support")]
        self.state
            .waiting
            .store(false, std::sync::atomic::Ordering::Release);
    }
}
type Polls = BTreeMap<(Uuid, Option<Uuid>), Weak<PollState>>;

#[derive(Default)]
pub struct Runtime {
    replay: ReplayCache,
    polls: Mutex<Polls>,
}
fn fail() -> AppError {
    AppError::Internal
}
impl super::persistence::LoadedLedger {
    pub(super) fn watch<'a>(
        &'a self,
        mut access: Access,
        query: &'a ViewQuery,
        at: u64,
    ) -> Result<Watch<'a>, AppError> {
        access.session_revision = self.authority_revision;
        Watch::new(&self.ledger, access, query, at).map_err(super::persistence::relay)
    }
}
impl From<CacheError> for AppError {
    fn from(error: CacheError) -> Self {
        match error {
            CacheError::Relay(error) => super::persistence::relay(error),
            CacheError::Internal => Self::Internal,
            CacheError::Expired | CacheError::Central | CacheError::EpochChanged => {
                Self::Conflict(error.to_string())
            }
            CacheError::Full | CacheError::SnapshotQuota => {
                Self::IdentityUnavailable(error.to_string())
            }
            CacheError::Oversized => Self::BadRequest(error.to_string()),
        }
    }
}
impl Runtime {
    pub fn reserve(&self, ledger: &Ledger, until: u64, now: u64) -> Result<(), AppError> {
        Ok(self.replay.reserve(ledger, until, now)?)
    }
    pub(super) fn view(&self, watch: &Watch<'_>) -> Result<Option<View>, AppError> {
        Ok(self.replay.view(watch)?)
    }
    pub fn frame(
        &self,
        peer: &aq_terminal_relay_protocol::Peer,
        frame: OutputFrame,
        now: u64,
    ) -> Result<Option<OutputGap>, AppError> {
        Ok(self.replay.frame(peer, frame, now)?)
    }
    pub fn snapshot(
        &self,
        peer: &aq_terminal_relay_protocol::Peer,
        snapshot: Snapshot,
        now: u64,
    ) -> Result<(), AppError> {
        Ok(self.replay.snapshot(peer, snapshot, now)?)
    }
    pub fn gap(
        &self,
        peer: &aq_terminal_relay_protocol::Peer,
        gap: &OutputGap,
        now: u64,
    ) -> Result<(), AppError> {
        Ok(self.replay.gap(peer, gap, now)?)
    }
    pub fn poll(&self, node: Uuid) -> Result<Poll, AppError> {
        self.poll_channel(node, None)
    }
    pub fn poll_channel(&self, node: Uuid, attempt: Option<Uuid>) -> Result<Poll, AppError> {
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
                    wake: Notify::new(),
                    #[cfg(feature = "test-support")]
                    waiting: Default::default(),
                });
                polls.insert(key, Arc::downgrade(&v));
                v
            }
        };
        // One control long-poll per channel; output polls take no slot (they only read).
        let permit = state
            .slot
            .clone()
            .try_acquire_owned()
            .map_err(|_| AppError::Conflict("one terminal control poll per channel".into()))?;
        Ok(Poll {
            state,
            _permit: permit,
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
    fn control_polls_share_the_channel_cap_and_release_on_drop() {
        let runtime = Runtime::default();
        let nodes: Vec<_> = (0..33).map(|_| Uuid::new_v4()).collect();
        let mut controls: Vec<_> = nodes[..32]
            .iter()
            .map(|id| runtime.poll(*id).unwrap())
            .collect();
        assert!(matches!(runtime.poll(nodes[0]), Err(AppError::Conflict(_))));
        assert!(matches!(
            runtime.poll(nodes[32]),
            Err(AppError::IdentityUnavailable(_))
        ));
        drop(controls.pop());
        let _next = runtime.poll(nodes[32]).unwrap();
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
        assert!(runtime.poll_channel(node, Some(attempt)).is_err());
        assert!(runtime.poll(node).is_ok());
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
        drop(first);
        assert!(runtime.poll_channel(node, Some(attempt)).is_ok());
    }
}
