//! Pure bounded relay planning only. No SQL, authentication, node transport or PTY.
//! Effects become available only after the embedding Journal acknowledges CAS.
pub mod api;
pub mod draft;
pub mod epoch;
pub mod output;
use aq_session_api as human;
use aq_session_protocol::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;
pub const MAX_OPERATIONS: usize = 1024;
pub const MAX_LEDGER_WIRE_BYTES: usize = 1048576;
pub const MAX_VIEWERS: usize = 4;
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("invalid relay contract")]
    Invalid,
    #[error("input-deadline rule: deadline_ms must be 1 to 2000 ms after issued_at_ms")]
    InputDeadline,
    #[error("scope not authorized")]
    Forbidden,
    #[error("stale session/controller/peer/revision")]
    Stale,
    #[error("operation ID reused with changed content")]
    Conflict,
    #[error("authority or command expired")]
    Expired,
    #[error("bounded capacity exhausted")]
    Capacity,
    #[error("controller fence is not yet effective")]
    PendingFence,
    #[error("input delivery unknown; explicit fresh-view resolution required")]
    Unknown,
    #[error("invalid state transition")]
    Transition,
    #[error("durable CAS failed")]
    Persistence,
    #[error("input mode requires separately negotiated capability")]
    Unsupported,
}
impl From<aq_terminal_relay_protocol::Error> for Error {
    fn from(e: aq_terminal_relay_protocol::Error) -> Self {
        match e {
            aq_terminal_relay_protocol::Error::Invalid => Self::Invalid,
            aq_terminal_relay_protocol::Error::Stale => Self::Stale,
        }
    }
}
pub type Result<T> = std::result::Result<T, Error>;
fn valid<T: Validate>(v: &T) -> Result<()> {
    v.validate().map_err(|_| Error::Invalid)
}
fn digest<T: Serialize>(v: &T) -> Result<Digest> {
    let mut value = serde_json::to_value(v).map_err(|_| Error::Invalid)?;
    value.sort_all_objects();
    Ok(Digest::of(
        &serde_json::to_vec(&value).map_err(|_| Error::Invalid)?,
    ))
}
fn nonnil(id: Uuid) -> Result<()> {
    if id.is_nil() {
        Err(Error::Invalid)
    } else {
        Ok(())
    }
}
/// Trusted adapter result, NEVER deserialized from a human body. Production must
/// obtain this under the same session/authorization locks as the committed CAS.
#[derive(Clone, Debug)]
pub struct Access {
    pub principal: Text,
    pub terminal: TerminalIdentity,
    pub session_revision: u64,
    pub watch: bool,
    pub control: bool,
    pub valid_until_ms: u64,
    pub execution_active: bool,
    pub execution_expires_at_ms: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Seen {
    principal: Text,
    digest: Digest,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Controller {
    lease: ControllerLease,
    fence_id: Uuid,
    acknowledged: bool,
    activate_after_ms: Option<u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    principal: Text,
    request_digest: Digest,
    command_digest: Digest,
    command: Option<InputCommand>,
    disposition: InputDisposition,
    dispatched: bool,
    deadline_ms: u64,
    byte_count: usize,
    admission_revision: u64,
    epoch: Option<Uuid>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EpochState {
    permit: epoch::Permit,
    highwater: u64,
    retired_through: u64,
    receipts: BTreeMap<u64, Uuid>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CaptureState {
    request: draft::Capture,
    snapshot: Option<Snapshot>,
    snapshot_digest: Option<Digest>,
    snapshot_id: Option<Uuid>,
    invalidated: bool,
}
/// Versioned durable *candidate*. Restore only from the trusted journal adapter;
/// production at-rest protection and transactional authority joins remain seams.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ledger {
    format: u32,
    terminal: TerminalIdentity,
    terminal_policy: TerminalPolicy,
    session_revision: u64,
    revision: u64,
    execution_expires_at_ms: u64,
    generation: u64,
    controller: Option<Controller>,
    last_fence: Option<draft::Fence>,
    last_permit_expires_at_ms: Option<u64>,
    expiry_margin_ms: Option<u64>,
    seen: BTreeMap<Uuid, Seen>,
    inputs: BTreeMap<Uuid, Input>,
    unknown: BTreeSet<Uuid>,
    captures: BTreeMap<Uuid, CaptureState>,
    viewers: BTreeMap<Uuid, WatchSubscription>,
    epoch_enabled: bool,
    epoch: Option<EpochState>,
    resize_sequence: u64,
    resize_digests: BTreeMap<Uuid, (Digest, u64, Option<bool>)>,
}
/// No dispatch effects are exposed by a Plan until a journal has acknowledged
/// ledger+outbox persistence atomically. Lost commit acknowledgement is not retry
/// permission; reload and recover the durable row before deciding anything.
pub struct Plan {
    expected: u64,
    next: Ledger,
    effects: Vec<draft::Effect>,
}
pub struct Committed {
    pub ledger: Ledger,
    pub effects: Vec<draft::Effect>,
}
pub trait Journal {
    /// Must CAS the authoritative row and outbox in ONE transaction, checking
    /// current registration/authorization/execution fences under its locks.
    /// Input outbox effects are ALREADY CLAIMED one-shot attempts: never replay
    /// them after lost commit/delivery acknowledgement. Recovery must retire
    /// their outbox entries atomically with Unknown/Expired ledger state.
    fn commit(
        &mut self,
        terminal: &TerminalIdentity,
        expected_revision: u64,
        next: &[u8],
        outbox: &[draft::Effect],
    ) -> Result<()>;
}
/// Async storage integration with the same durable ledger+outbox contract. The
/// future must acknowledge its actual transaction COMMIT, not merely an in-memory
/// staging object, before returning success.
pub trait AsyncJournal {
    fn commit<'a>(
        &'a mut self,
        terminal: &'a TerminalIdentity,
        expected_revision: u64,
        next: &'a [u8],
        outbox: &'a [draft::Effect],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>>;
}
impl Plan {
    /// Compose canonical transitions without exposing effects before one actual
    /// journal commit. All intermediate revisions remain inside this CAS plan.
    pub fn and_then(mut self, next: impl FnOnce(&Ledger) -> Result<Plan>) -> Result<Self> {
        let following = next(&self.next)?;
        if following.expected != self.next.revision || following.next.terminal != self.next.terminal
        {
            return Err(Error::Conflict);
        }
        if self.effects.len() + following.effects.len() > 64 {
            return Err(Error::Capacity);
        }
        self.next = following.next;
        self.effects.extend(following.effects);
        Ok(self)
    }
    pub fn preview(&self) -> LedgerSummary {
        self.next.summary()
    }
    pub async fn commit_async(self, journal: &mut impl AsyncJournal) -> Result<Committed> {
        let bytes = self.next.encode()?;
        journal
            .commit(&self.next.terminal, self.expected, &bytes, &self.effects)
            .await?;
        Ok(Committed {
            ledger: self.next,
            effects: self.effects,
        })
    }
    pub fn commit(self, journal: &mut impl Journal) -> Result<Committed> {
        let bytes = self.next.encode()?;
        journal.commit(&self.next.terminal, self.expected, &bytes, &self.effects)?;
        Ok(Committed {
            ledger: self.next,
            effects: self.effects,
        })
    }
}
impl Ledger {
    pub fn new(
        terminal: TerminalIdentity,
        session_revision: u64,
        execution_expires_at_ms: u64,
        expiry_margin_ms: Option<u64>,
    ) -> Result<Self> {
        valid(&terminal)?;
        if session_revision == 0
            || execution_expires_at_ms == 0
            || expiry_margin_ms.is_some_and(|v| v > 5000)
        {
            return Err(Error::Invalid);
        }
        Ok(Self {
            format: 1,
            terminal,
            terminal_policy: TerminalPolicy {
                host: TerminalHost::Tmux,
                input_enabled: true,
                max_input_bytes: MAX_INPUT_BYTES as u32,
                max_pending_input_bytes: MAX_PENDING_INPUT_BYTES as u32,
                max_frame_bytes: MAX_FRAME_BYTES as u32,
                replay_bytes: MAX_REPLAY_BYTES as u32,
            },
            session_revision,
            revision: 0,
            execution_expires_at_ms,
            generation: 0,
            controller: None,
            last_fence: None,
            last_permit_expires_at_ms: None,
            expiry_margin_ms,
            seen: BTreeMap::new(),
            inputs: BTreeMap::new(),
            unknown: BTreeSet::new(),
            captures: BTreeMap::new(),
            viewers: BTreeMap::new(),
            epoch_enabled: false,
            epoch: None,
            resize_sequence: 0,
            resize_digests: BTreeMap::new(),
        })
    }
    /// Production adapters must supply the policy from the verified sealed
    /// manifest. Protocol-max constructors are pure contract conveniences only.
    pub fn new_profiled(
        terminal: TerminalIdentity,
        session_revision: u64,
        execution_ceiling_ms: u64,
        margin: Option<u64>,
        policy: TerminalPolicy,
        epoch_enabled: bool,
    ) -> Result<Self> {
        valid_policy(&policy)?;
        if epoch_enabled && !policy.input_enabled {
            return Err(Error::Forbidden);
        }
        let mut ledger = Self::new(terminal, session_revision, execution_ceiling_ms, margin)?;
        ledger.terminal_policy = policy;
        ledger.epoch_enabled = epoch_enabled;
        ledger.check()?;
        Ok(ledger)
    }
    pub fn terminal_policy(&self) -> &TerminalPolicy {
        &self.terminal_policy
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn terminal(&self) -> &TerminalIdentity {
        &self.terminal
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn paused(&self) -> bool {
        !self.unknown.is_empty()
    }
    pub fn status(&self) -> draft::ControlStatus {
        draft::ControlStatus {
            terminal: self.terminal.clone(),
            session_revision: self.session_revision,
            generation: self.generation,
            lease: self.controller.as_ref().map(|v| v.lease.clone()),
            fence_acknowledged: self.controller.as_ref().is_some_and(|v| v.acknowledged),
            input_paused: self.paused(),
        }
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        let bytes = serde_json::to_vec(self).map_err(|_| Error::Invalid)?;
        if bytes.len() > MAX_LEDGER_WIRE_BYTES {
            return Err(Error::Capacity);
        }
        Ok(bytes)
    }
    fn auth(
        &self,
        a: &Access,
        t: &TerminalIdentity,
        revision: u64,
        control: bool,
        now: u64,
    ) -> Result<()> {
        valid(t)?;
        if *t != self.terminal
            || a.terminal != self.terminal
            || revision != self.session_revision
            || a.session_revision != self.session_revision
        {
            return Err(Error::Stale);
        }
        if (control && (!a.control || !self.terminal_policy.input_enabled))
            || (!control && !a.watch)
        {
            return Err(Error::Forbidden);
        }
        if now >= a.valid_until_ms {
            return Err(Error::Expired);
        }
        Ok(())
    }
    fn peer(&self, p: &draft::Peer) -> Result<()> {
        valid(&self.terminal)?;
        p.validate().map_err(|_| Error::Stale)?;
        if !p.matches(&self.terminal) {
            return Err(Error::Stale);
        }
        Ok(())
    }
    fn seen(&self, id: Uuid, a: &Access, value: &impl Serialize) -> Result<bool> {
        nonnil(id)?;
        let d = digest(value)?;
        if let Some(old) = self.seen.get(&id) {
            return if old.principal == a.principal && old.digest == d {
                Ok(true)
            } else {
                Err(Error::Conflict)
            };
        }
        if self.seen.len() >= MAX_OPERATIONS {
            return Err(Error::Capacity);
        }
        Ok(false)
    }
    fn remember(&mut self, id: Uuid, a: &Access, value: &impl Serialize) -> Result<()> {
        self.seen.insert(
            id,
            Seen {
                principal: a.principal.clone(),
                digest: digest(value)?,
            },
        );
        Ok(())
    }
    fn plan(&self, mut next: Self, effects: Vec<draft::Effect>) -> Result<Plan> {
        let unchanged = effects.is_empty() && self.encode()? == next.encode()?;
        next.revision = if unchanged {
            self.revision
        } else {
            self.revision.checked_add(1).ok_or(Error::Capacity)?
        };
        if effects.len() > 64 {
            return Err(Error::Capacity);
        }
        for effect in &effects {
            effect.validate().map_err(|_| Error::Invalid)?;
        }
        next.check()?;
        next.encode()?;
        Ok(Plan {
            expected: self.revision,
            next,
            effects,
        })
    }
    fn retire_controller_inputs(&mut self) {
        for (id, input) in &mut self.inputs {
            if input.command.take().is_some() {
                if input.dispatched {
                    input.disposition = InputDisposition::DeliveryUnknown;
                    self.unknown.insert(*id);
                } else {
                    input.disposition = InputDisposition::Rejected;
                }
            }
        }
    }
    fn active(
        &self,
        a: &Access,
        lease_id: Uuid,
        generation: u64,
        now: u64,
    ) -> Result<&ControllerLease> {
        if !a.execution_active
            || now >= a.execution_expires_at_ms
            || now >= self.execution_expires_at_ms
        {
            return Err(Error::Expired);
        }
        let c = self.controller.as_ref().ok_or(Error::Stale)?;
        if c.lease.principal_id != a.principal
            || c.lease.lease_id != lease_id
            || generation != self.generation
        {
            return Err(Error::Stale);
        }
        if now < c.lease.issued_at_ms || now >= c.lease.expires_at_ms {
            return Err(Error::Expired);
        }
        if !c.acknowledged && c.activate_after_ms.is_none_or(|v| now < v) {
            return Err(Error::PendingFence);
        }
        Ok(&c.lease)
    }
    pub fn acquire(
        &self,
        a: &Access,
        r: &human::AcquireControlRequest,
        fence_id: Uuid,
        lease_id: Uuid,
        now: u64,
    ) -> Result<Plan> {
        self.auth(a, &r.terminal, r.expected_session_revision, true, now)?;
        if self.seen(r.operation_id, a, r)? {
            return self.plan(self.clone(), vec![]);
        }
        nonnil(fence_id)?;
        nonnil(lease_id)?;
        if r.expected_controller_generation != self.generation {
            return Err(Error::Stale);
        }
        if !a.execution_active || now >= a.execution_expires_at_ms {
            return Err(Error::Expired);
        }
        let expires = r
            .expires_at_ms
            .min(now.checked_add(15000).ok_or(Error::Invalid)?)
            .min(self.execution_expires_at_ms)
            .min(a.execution_expires_at_ms)
            .min(a.valid_until_ms);
        if expires <= now {
            return Err(Error::Expired);
        }
        let generation = self.generation.checked_add(1).ok_or(Error::Capacity)?;
        let lease = ControllerLease {
            terminal: self.terminal.clone(),
            principal_id: a.principal.clone(),
            lease_id,
            generation,
            issued_at_ms: now,
            expires_at_ms: expires,
            execution_expires_at_ms: self.execution_expires_at_ms.min(a.execution_expires_at_ms),
        };
        valid(&lease)?;
        let activate_after_ms = self
            .last_permit_expires_at_ms
            .and_then(|v| self.expiry_margin_ms.and_then(|m| v.checked_add(m)));
        let fence = draft::Fence {
            terminal: self.terminal.clone(),
            fence_id,
            generation,
            controller: Some(lease.clone()),
        };
        let mut n = self.clone();
        n.retire_controller_inputs();
        n.generation = generation;
        n.epoch = None;
        n.resize_sequence = 0;
        n.controller = Some(Controller {
            lease,
            fence_id,
            acknowledged: false,
            activate_after_ms,
        });
        n.last_fence = Some(fence.clone());
        n.last_permit_expires_at_ms =
            Some(self.last_permit_expires_at_ms.unwrap_or(0).max(expires));
        n.remember(r.operation_id, a, r)?;
        self.plan(n, vec![draft::Effect::Fence(fence)])
    }
    pub fn acknowledge_fence(&self, p: &draft::Peer, ack: &draft::FenceAck) -> Result<Plan> {
        self.peer(p)?;
        let fence = self.last_fence.as_ref().ok_or(Error::Stale)?;
        if ack.terminal != self.terminal
            || ack.fence_id != fence.fence_id
            || ack.generation != self.generation
        {
            return Err(Error::Stale);
        }
        let mut n = self.clone();
        if let Some(c) = &mut n.controller {
            c.acknowledged = true;
        }
        self.plan(n, vec![])
    }
    pub fn release(
        &self,
        a: &Access,
        r: &draft::ReleaseControl,
        fence_id: Uuid,
        now: u64,
    ) -> Result<Plan> {
        self.auth(a, &r.terminal, r.expected_session_revision, true, now)?;
        if self.seen(r.operation_id, a, r)? {
            return self.plan(self.clone(), vec![]);
        }
        self.active(a, r.lease_id, r.expected_controller_generation, now)?;
        nonnil(fence_id)?;
        let mut n = self.clone();
        n.generation = n.generation.checked_add(1).ok_or(Error::Capacity)?;
        n.controller = None;
        n.retire_controller_inputs();
        n.epoch = None;
        n.remember(r.operation_id, a, r)?;
        let fence = draft::Fence {
            terminal: self.terminal.clone(),
            fence_id,
            generation: n.generation,
            controller: None,
        };
        n.last_fence = Some(fence.clone());
        self.plan(n, vec![draft::Effect::Fence(fence)])
    }
    pub fn revoke(&self, fence_id: Uuid) -> Result<Plan> {
        nonnil(fence_id)?;
        let mut n = self.clone();
        n.generation = n.generation.checked_add(1).ok_or(Error::Capacity)?;
        n.controller = None;
        n.retire_controller_inputs();
        n.epoch = None;
        let fence = draft::Fence {
            terminal: self.terminal.clone(),
            fence_id,
            generation: n.generation,
            controller: None,
        };
        n.last_fence = Some(fence.clone());
        self.plan(n, vec![draft::Effect::Fence(fence)])
    }
    pub fn input(&self, a: &Access, r: &human::InputRequest, now: u64) -> Result<Plan> {
        if self.epoch_enabled {
            return Err(Error::Unsupported);
        }
        self.admit_input(a, r, now)
    }
    fn admit_input(&self, a: &Access, r: &human::InputRequest, now: u64) -> Result<Plan> {
        if r.bytes.is_empty() || r.bytes.len() > self.terminal_policy.max_input_bytes as usize {
            return Err(Error::Invalid);
        }
        self.auth(a, &r.terminal, r.expected_session_revision, true, now)?;
        if self.seen(r.operation_id, a, r)? {
            return self.plan(self.clone(), vec![]);
        }
        let lease = self.active(a, r.lease_id, r.expected_controller_generation, now)?;
        if self.paused() {
            return Err(Error::Unknown);
        }
        if self.inputs.len() >= MAX_OPERATIONS {
            return Err(Error::Capacity);
        }
        if r.expires_at_ms > a.execution_expires_at_ms || r.expires_at_ms > a.valid_until_ms {
            return Err(Error::Expired);
        }
        if r.expires_at_ms
            .checked_sub(now)
            .is_none_or(|ttl| !(1..=2000).contains(&ttl))
        {
            return Err(Error::InputDeadline);
        }
        let command = InputCommand {
            operation_id: r.operation_id,
            lease: lease.clone(),
            issued_at_ms: now,
            deadline_ms: r.expires_at_ms,
            bytes: r.bytes.clone(),
        };
        valid(&command)?;
        let pending: usize = self
            .inputs
            .values()
            .filter(|v| v.command.is_some())
            .map(|v| v.byte_count)
            .sum();
        if pending
            .checked_add(r.bytes.len())
            .is_none_or(|v| v > self.terminal_policy.max_pending_input_bytes as usize)
        {
            return Err(Error::Capacity);
        }
        let mut n = self.clone();
        n.remember(r.operation_id, a, r)?;
        n.inputs.insert(
            r.operation_id,
            Input {
                principal: a.principal.clone(),
                request_digest: digest(r)?,
                command_digest: digest(&command)?,
                command: Some(command),
                disposition: InputDisposition::Queued,
                dispatched: false,
                deadline_ms: r.expires_at_ms,
                byte_count: r.bytes.len(),
                admission_revision: self.revision.checked_add(1).ok_or(Error::Capacity)?,
                epoch: None,
            },
        );
        self.plan(n, vec![])
    }
    pub fn input_receipt(
        &self,
        a: &Access,
        r: &human::InputRequest,
        now: u64,
    ) -> Result<human::InputReceipt> {
        if r.bytes.is_empty() || r.bytes.len() > self.terminal_policy.max_input_bytes as usize {
            return Err(Error::Invalid);
        }
        self.auth(a, &r.terminal, r.expected_session_revision, true, now)?;
        let input = self.inputs.get(&r.operation_id).ok_or(Error::Stale)?;
        if input.principal != a.principal || input.request_digest != digest(r)? {
            return Err(Error::Conflict);
        }
        Ok(human::InputReceipt {
            operation_id: r.operation_id,
            terminal: self.terminal.clone(),
            disposition: input.disposition,
            input_paused: self.paused(),
            observed_at_ms: now,
        })
    }
    pub fn dispatch_input(&self, a: &Access, id: Uuid, now: u64) -> Result<Plan> {
        if self.epoch_enabled {
            return Err(Error::Unsupported);
        }
        self.dispatch_raw(a, id, now)
    }
    fn dispatch_raw(&self, a: &Access, id: Uuid, now: u64) -> Result<Plan> {
        if self.paused() {
            return Err(Error::Unknown);
        }
        if self
            .inputs
            .values()
            .any(|v| v.dispatched && v.command.is_some())
        {
            return Err(Error::Capacity);
        }
        let input = self.inputs.get(&id).ok_or(Error::Stale)?;
        let c = input.command.as_ref().ok_or(Error::Transition)?;
        self.auth(a, &c.lease.terminal, self.session_revision, true, now)?;
        self.active(a, c.lease.lease_id, c.lease.generation, now)?;
        if now >= input.deadline_ms
            || input.deadline_ms > a.execution_expires_at_ms
            || input.deadline_ms > a.valid_until_ms
        {
            return Err(Error::Expired);
        }
        let first = self
            .inputs
            .iter()
            .filter(|(_, v)| {
                !v.dispatched
                    && v.disposition == InputDisposition::Queued
                    && v.command.as_ref().is_some_and(|old| {
                        old.lease.lease_id == c.lease.lease_id
                            && old.lease.generation == c.lease.generation
                    })
            })
            .min_by_key(|(_, v)| v.admission_revision)
            .map(|(id, _)| *id);
        if first != Some(id) {
            return Err(Error::Transition);
        }
        if input.disposition != InputDisposition::Queued || input.dispatched {
            return Err(Error::Transition);
        }
        let mut n = self.clone();
        n.inputs.get_mut(&id).unwrap().dispatched = true;
        self.plan(n, vec![draft::Effect::Input(c.clone())])
    }
    pub fn observe_input(
        &self,
        p: &draft::Peer,
        r: &aq_session_protocol::InputReceipt,
    ) -> Result<Plan> {
        self.peer(p)?;
        valid(r)?;
        if r.terminal != self.terminal {
            return Err(Error::Stale);
        }
        let old = self.inputs.get(&r.operation_id).ok_or(Error::Stale)?;
        if old.command_digest != r.command_digest {
            return Err(Error::Conflict);
        }
        if old.disposition == r.disposition {
            return self.plan(self.clone(), vec![]);
        }
        if !old.dispatched || old.command.is_none() || r.disposition == InputDisposition::Queued {
            return Err(Error::Transition);
        }
        let rank = |d| match d {
            InputDisposition::Queued => 0,
            InputDisposition::Received => 1,
            InputDisposition::Injecting => 2,
            _ => 3,
        };
        if rank(r.disposition) < rank(old.disposition) {
            return Err(Error::Transition);
        }
        let mut n = self.clone();
        let input = n.inputs.get_mut(&r.operation_id).unwrap();
        input.disposition = r.disposition;
        if matches!(
            r.disposition,
            InputDisposition::Applied
                | InputDisposition::Expired
                | InputDisposition::Rejected
                | InputDisposition::DeliveryUnknown
        ) {
            input.command = None;
        }
        if r.disposition == InputDisposition::DeliveryUnknown {
            n.unknown.insert(r.operation_id);
        }
        self.plan(n, vec![])
    }
    pub fn expire(&self, now: u64) -> Result<Plan> {
        let mut n = self.clone();
        for (id, input) in &mut n.inputs {
            if input.command.is_some() && now >= input.deadline_ms {
                input.command = None;
                input.disposition = if input.dispatched {
                    n.unknown.insert(*id);
                    InputDisposition::DeliveryUnknown
                } else {
                    InputDisposition::Expired
                };
            }
        }
        n.viewers.retain(|_, v| now < v.expires_at_ms);
        self.plan(n, vec![])
    }
    /// Restart/offline recovery never dispatches a recorded input automatically.
    /// Every previously dispatched nonfinal command becomes unknown; other queued
    /// commands expire immediately even if their old wall deadline is in future.
    pub fn recover(bytes: &[u8]) -> Result<Plan> {
        if bytes.len() > MAX_LEDGER_WIRE_BYTES {
            return Err(Error::Capacity);
        }
        let old: Self = serde_json::from_slice(bytes).map_err(|_| Error::Invalid)?;
        old.check()?;
        let mut n = old.clone();
        n.viewers.clear();
        for capture in n.captures.values_mut() {
            capture.snapshot = None;
            capture.invalidated = true;
        }
        for (id, input) in &mut n.inputs {
            if input.command.take().is_some() {
                input.disposition = if input.dispatched {
                    n.unknown.insert(*id);
                    InputDisposition::DeliveryUnknown
                } else {
                    InputDisposition::Expired
                };
            }
        }
        if let Some(c) = &mut n.controller {
            c.acknowledged = false;
            c.activate_after_ms = None;
        }
        old.plan(n, vec![])
    }
    fn check(&self) -> Result<()> {
        valid_policy(&self.terminal_policy)?;
        if !self.terminal_policy.input_enabled
            && (self.controller.is_some() || !self.inputs.is_empty() || self.epoch_enabled)
        {
            return Err(Error::Invalid);
        }

        valid(&self.terminal)?;
        if self.format != 1
            || self.session_revision == 0
            || self.seen.len() > MAX_OPERATIONS
            || self.inputs.len() > MAX_OPERATIONS
            || self.captures.len() > MAX_OPERATIONS
            || self.viewers.len() > MAX_VIEWERS
            || self.resize_digests.len() > MAX_OPERATIONS
            || self.unknown.len() > self.inputs.len()
        {
            return Err(Error::Invalid);
        }
        if self.execution_expires_at_ms == 0
            || self.expiry_margin_ms.is_some_and(|v| v > 5000)
            || self.seen.keys().any(Uuid::is_nil)
            || self.captures.keys().any(Uuid::is_nil)
        {
            return Err(Error::Invalid);
        }
        if let Some(f) = &self.last_fence
            && (f.generation != self.generation
                || f.terminal != self.terminal
                || f.fence_id.is_nil())
        {
            return Err(Error::Invalid);
        }
        if let Some(c) = &self.controller {
            valid(&c.lease)?;
            if c.lease.terminal != self.terminal
                || c.lease.generation != self.generation
                || c.lease.execution_expires_at_ms > self.execution_expires_at_ms
            {
                return Err(Error::Invalid);
            }
        }
        if let Some(e) = &self.epoch {
            e.permit.validate()?;
            if !self.epoch_enabled
                || e.permit.terminal != self.terminal
                || e.permit.controller_generation != self.generation
                || self.controller.as_ref().is_none_or(|c| {
                    c.lease.lease_id != e.permit.lease_id
                        || e.permit.expires_at_ms > c.lease.expires_at_ms
                })
                || e.receipts.len() > epoch::RECEIPT_WINDOW
                || e.highwater < e.retired_through
                || e.highwater - e.retired_through != e.receipts.len() as u64
            {
                return Err(Error::Invalid);
            }
            for (sequence, id) in &e.receipts {
                if *sequence <= e.retired_through
                    || *sequence > e.highwater
                    || epoch::operation_id(&e.permit, *sequence)? != *id
                    || self
                        .inputs
                        .get(id)
                        .is_none_or(|v| v.epoch != Some(e.permit.epoch_id))
                {
                    return Err(Error::Invalid);
                }
            }
        }
        let mut pending = 0usize;
        for (id, input) in &self.inputs {
            let seen = self.seen.get(id).ok_or(Error::Invalid)?;
            if id.is_nil()
                || seen.principal != input.principal
                || seen.digest != input.request_digest
            {
                return Err(Error::Invalid);
            };
            if input.admission_revision == 0 || input.admission_revision > self.revision {
                return Err(Error::Invalid);
            }
            if !(1..=self.terminal_policy.max_input_bytes as usize).contains(&input.byte_count) {
                return Err(Error::Invalid);
            }
            match input.disposition {
                InputDisposition::Queued => {
                    if input.command.is_none() {
                        return Err(Error::Invalid);
                    }
                }
                InputDisposition::Received | InputDisposition::Injecting => {
                    if !input.dispatched || input.command.is_none() {
                        return Err(Error::Invalid);
                    }
                }
                InputDisposition::DeliveryUnknown | InputDisposition::Applied => {
                    if !input.dispatched || input.command.is_some() {
                        return Err(Error::Invalid);
                    }
                }
                InputDisposition::Rejected => {
                    if input.command.is_some() {
                        return Err(Error::Invalid);
                    }
                }
                InputDisposition::Expired => {
                    if input.command.is_some() {
                        return Err(Error::Invalid);
                    }
                }
            }
            if input.epoch.is_some() && !self.epoch_enabled {
                return Err(Error::Invalid);
            }
            if let Some(c) = &input.command {
                valid(c)?;
                if c.operation_id != *id
                    || c.lease.terminal != self.terminal
                    || digest(c)? != input.command_digest
                    || c.bytes.len() != input.byte_count
                    || c.deadline_ms != input.deadline_ms
                {
                    return Err(Error::Invalid);
                }
                pending = pending.checked_add(c.bytes.len()).ok_or(Error::Capacity)?;
            }
        }
        for (id, capture) in &self.captures {
            if *id != capture.request.capture_request_id
                || capture.request.terminal != self.terminal
                || capture.request.after_ledger_revision > self.revision
            {
                return Err(Error::Invalid);
            }
            if let Some(snapshot) = &capture.snapshot {
                valid(snapshot)?;
                if snapshot.capture_request_id != *id
                    || capture.snapshot_id != Some(snapshot.snapshot_id)
                    || capture.snapshot_digest.as_ref() != Some(&digest(snapshot)?)
                {
                    return Err(Error::Invalid);
                }
            }
        }
        if pending > self.terminal_policy.max_pending_input_bytes as usize
            || self.unknown.iter().any(|id| {
                self.inputs
                    .get(id)
                    .is_none_or(|v| v.disposition != InputDisposition::DeliveryUnknown)
            })
        {
            return Err(Error::Invalid);
        }
        Ok(())
    }
}
impl Ledger {
    /// Renewal is an explicit replacement grant with a higher generation. It
    /// never extends the old issued permit or authorizes execution renewal.
    pub fn renew(
        &self,
        a: &Access,
        r: &draft::RenewControl,
        fence_id: Uuid,
        new_lease_id: Uuid,
        now: u64,
    ) -> Result<Plan> {
        self.auth(a, &r.terminal, r.expected_session_revision, true, now)?;
        if self.seen(r.operation_id, a, r)? {
            return self.plan(self.clone(), vec![]);
        }
        self.active(a, r.lease_id, r.expected_controller_generation, now)?;
        if new_lease_id == r.lease_id {
            return Err(Error::Invalid);
        }
        let request = human::AcquireControlRequest {
            terminal: r.terminal.clone(),
            operation_id: r.operation_id,
            expected_session_revision: r.expected_session_revision,
            expected_controller_generation: r.expected_controller_generation,
            expires_at_ms: r.expires_at_ms,
        };
        let mut plan = self.acquire(a, &request, fence_id, new_lease_id, now)?;
        plan.next.remember(r.operation_id, a, r)?;
        Ok(plan)
    }
    pub fn watch(&self, a: &Access, r: &human::WatchRequest, now: u64) -> Result<Plan> {
        self.auth(a, &r.terminal, r.expected_session_revision, false, now)?;
        if self.seen(r.operation_id, a, r)? {
            return self.plan(self.clone(), vec![]);
        }
        if r.expires_at_ms <= now
            || r.expires_at_ms > now.checked_add(30000).ok_or(Error::Invalid)?
            || r.expires_at_ms > a.valid_until_ms
        {
            return Err(Error::Expired);
        }
        let mut n = self.clone();
        n.viewers.retain(|_, v| now < v.expires_at_ms);
        if n.viewers.len() >= MAX_VIEWERS {
            return Err(Error::Capacity);
        }
        let sub = WatchSubscription {
            subscription_id: r.operation_id,
            terminal: self.terminal.clone(),
            principal_id: a.principal.clone(),
            expires_at_ms: r.expires_at_ms,
        };
        n.viewers.insert(r.operation_id, sub.clone());
        n.remember(r.operation_id, a, r)?;
        self.plan(n, vec![draft::Effect::Watch(sub)])
    }
    pub fn authorize_watch(&self, a: &Access, subscription_id: Uuid, now: u64) -> Result<()> {
        self.auth(a, &self.terminal, self.session_revision, false, now)?;
        let sub = self.viewers.get(&subscription_id).ok_or(Error::Stale)?;
        if sub.principal_id != a.principal {
            return Err(Error::Forbidden);
        }
        if now >= sub.expires_at_ms {
            return Err(Error::Expired);
        }
        Ok(())
    }
    pub fn resize(&self, a: &Access, r: &human::ResizeRequest, now: u64) -> Result<Plan> {
        self.auth(a, &r.terminal, r.expected_session_revision, true, now)?;
        if self.seen(r.operation_id, a, r)? {
            return self.plan(self.clone(), vec![]);
        }
        let lease = self.active(a, r.lease_id, r.expected_controller_generation, now)?;
        if self.paused() {
            return Err(Error::Unknown);
        }
        if r.sequence <= self.resize_sequence {
            return Err(Error::Stale);
        }
        if r.expires_at_ms > a.execution_expires_at_ms || r.expires_at_ms > a.valid_until_ms {
            return Err(Error::Expired);
        }
        let c = ResizeCommand {
            operation_id: r.operation_id,
            lease: lease.clone(),
            sequence: r.sequence,
            issued_at_ms: now,
            deadline_ms: r.expires_at_ms,
            columns: r.columns,
            rows: r.rows,
        };
        valid(&c)?;
        let mut n = self.clone();
        n.resize_sequence = r.sequence;
        n.resize_digests
            .insert(r.operation_id, (digest(&c)?, r.sequence, None));
        n.remember(r.operation_id, a, r)?;
        self.plan(n, vec![draft::Effect::Resize(c)])
    }
    pub fn observe_resize(&self, p: &draft::Peer, r: &draft::ResizeReceipt) -> Result<Plan> {
        self.peer(p)?;
        if r.terminal != self.terminal {
            return Err(Error::Stale);
        }
        let (digest, sequence, prior) = self
            .resize_digests
            .get(&r.operation_id)
            .ok_or(Error::Stale)?;
        if *digest != r.command_digest || *sequence != r.sequence {
            return Err(Error::Conflict);
        }
        // This stage validates keyed observations only. A production outbox may
        // retry the exact idempotent command while unexpired/current; no input
        // injection semantics are inferred from a resize acknowledgement.
        if prior.is_some_and(|v| v != r.applied) {
            return Err(Error::Conflict);
        }
        let mut n = self.clone();
        n.resize_digests.get_mut(&r.operation_id).unwrap().2 = Some(r.applied);
        self.plan(n, vec![])
    }
    pub fn request_capture(
        &self,
        a: &Access,
        uncertain: Uuid,
        capture_request_id: Uuid,
        lease_id: Uuid,
        generation: u64,
        now: u64,
    ) -> Result<Plan> {
        self.auth(a, &self.terminal, self.session_revision, true, now)?;
        if !a.watch {
            return Err(Error::Forbidden);
        }
        self.active(a, lease_id, generation, now)?;
        nonnil(capture_request_id)?;
        if !self.unknown.contains(&uncertain) {
            return Err(Error::Transition);
        }
        if self.captures.contains_key(&capture_request_id) {
            return Err(Error::Conflict);
        }
        if self.captures.len() >= MAX_OPERATIONS {
            return Err(Error::Capacity);
        }
        let expires = now
            .checked_add(2000)
            .ok_or(Error::Invalid)?
            .min(self.controller.as_ref().unwrap().lease.expires_at_ms);
        if expires <= now {
            return Err(Error::Expired);
        }
        let request = draft::Capture {
            terminal: self.terminal.clone(),
            capture_request_id,
            uncertain_operation_id: uncertain,
            lease_id,
            controller_generation: generation,
            after_ledger_revision: self.revision,
            expires_at_ms: expires,
        };
        let mut n = self.clone();
        n.captures.insert(
            capture_request_id,
            CaptureState {
                request: request.clone(),
                snapshot: None,
                snapshot_digest: None,
                snapshot_id: None,
                invalidated: false,
            },
        );
        self.plan(n, vec![draft::Effect::Capture(request)])
    }
    pub fn observe_capture(&self, p: &draft::Peer, s: &Snapshot, now: u64) -> Result<Plan> {
        if s.state.len() > self.terminal_policy.replay_bytes as usize {
            return Err(Error::Capacity);
        }

        self.peer(p)?;
        valid(s)?;
        if s.terminal != self.terminal {
            return Err(Error::Stale);
        }
        let old = self
            .captures
            .get(&s.capture_request_id)
            .ok_or(Error::Stale)?;
        if old.invalidated
            || old.request.controller_generation != self.generation
            || self
                .controller
                .as_ref()
                .is_none_or(|c| c.lease.lease_id != old.request.lease_id)
            || !self.unknown.contains(&old.request.uncertain_operation_id)
        {
            return Err(Error::Stale);
        }
        if now >= old.request.expires_at_ms {
            return Err(Error::Expired);
        }
        if self
            .captures
            .iter()
            .any(|(id, c)| *id != s.capture_request_id && c.snapshot_id == Some(s.snapshot_id))
        {
            return Err(Error::Conflict);
        }
        let d = digest(s)?;
        if old.snapshot_digest.as_ref().is_some_and(|v| *v != d) {
            return Err(Error::Conflict);
        }
        let mut n = self.clone();
        let capture = n.captures.get_mut(&s.capture_request_id).unwrap();
        capture.snapshot = Some(s.clone());
        capture.snapshot_digest = Some(d);
        capture.snapshot_id = Some(s.snapshot_id);
        self.plan(n, vec![])
    }
    pub fn resolve(&self, a: &Access, r: &human::ResolveInputRequest, now: u64) -> Result<Plan> {
        self.auth(a, &r.terminal, r.expected_session_revision, true, now)?;
        if self.seen(r.operation_id, a, r)? {
            return self.plan(self.clone(), vec![]);
        }
        if !a.watch {
            return Err(Error::Forbidden);
        }
        self.active(a, r.lease_id, r.expected_controller_generation, now)?;
        if now >= r.expires_at_ms {
            return Err(Error::Expired);
        }
        let capture = self
            .captures
            .get(&r.fresh_capture_request_id)
            .ok_or(Error::Stale)?;
        if now >= capture.request.expires_at_ms {
            return Err(Error::Expired);
        }
        if capture.invalidated {
            return Err(Error::Stale);
        }
        let s = capture.snapshot.as_ref().ok_or(Error::Stale)?;
        if capture.request.uncertain_operation_id != r.uncertain_operation_id
            || capture.request.controller_generation != self.generation
            || capture.request.lease_id != r.lease_id
            || s.snapshot_id != r.fresh_snapshot_id
            || s.through_sequence != r.fresh_snapshot_sequence
        {
            return Err(Error::Stale);
        }
        if !self.unknown.contains(&r.uncertain_operation_id) {
            return Err(Error::Transition);
        }
        let mut n = self.clone();
        n.unknown.remove(&r.uncertain_operation_id);
        n.remember(r.operation_id, a, r)?;
        // Keep the token/digest tombstone, discard sensitive renderer state.
        n.captures
            .get_mut(&r.fresh_capture_request_id)
            .unwrap()
            .snapshot = None;
        self.plan(n, vec![])
    }
}
impl Ledger {
    pub fn new_epoch(
        terminal: TerminalIdentity,
        session_revision: u64,
        execution_ceiling_ms: u64,
        margin: Option<u64>,
    ) -> Result<Self> {
        let mut n = Self::new(terminal, session_revision, execution_ceiling_ms, margin)?;
        n.epoch_enabled = true;
        Ok(n)
    }
    /// Trusted server nonce, not a human-selected epoch. A grant can open exactly
    /// one epoch. Changing it requires a replacement controller generation.
    pub fn open_epoch(&self, a: &Access, epoch_id: Uuid, now: u64) -> Result<Plan> {
        if !self.epoch_enabled {
            return Err(Error::Unsupported);
        }
        self.auth(a, &self.terminal, self.session_revision, true, now)?;
        nonnil(epoch_id)?;
        if self.epoch.is_some() {
            return Err(Error::Conflict);
        }
        let c = self.controller.as_ref().ok_or(Error::Stale)?;
        self.active(a, c.lease.lease_id, self.generation, now)?;
        let mut n = self.clone();
        let remove: Vec<_> = n
            .inputs
            .iter()
            .filter(|(id, v)| v.epoch.is_some() && v.command.is_none() && !n.unknown.contains(id))
            .map(|(id, _)| *id)
            .collect();
        for id in remove {
            n.inputs.remove(&id);
            n.seen.remove(&id);
        }
        n.epoch = Some(EpochState {
            permit: epoch::Permit {
                terminal: self.terminal.clone(),
                epoch_id,
                lease_id: c.lease.lease_id,
                controller_generation: self.generation,
                expires_at_ms: c
                    .lease
                    .expires_at_ms
                    .min(a.valid_until_ms)
                    .min(a.execution_expires_at_ms)
                    .min(self.execution_expires_at_ms),
                derivation_version: 1,
            },
            highwater: 0,
            retired_through: 0,
            receipts: BTreeMap::new(),
        });
        self.plan(n, vec![])
    }
    pub fn epoch_permit(&self) -> Option<epoch::Permit> {
        self.epoch.as_ref().map(|v| v.permit.clone())
    }
    pub fn epoch_input(&self, a: &Access, r: &epoch::Request, now: u64) -> Result<Plan> {
        r.validate()?;
        if !self.epoch_enabled {
            return Err(Error::Unsupported);
        }
        self.auth(
            a,
            &r.input.terminal,
            r.input.expected_session_revision,
            true,
            now,
        )?;
        let e = self.epoch.as_ref().ok_or(Error::Stale)?;
        self.active(a, e.permit.lease_id, e.permit.controller_generation, now)?;
        if r.epoch_id != e.permit.epoch_id
            || r.sequence <= e.retired_through
            || r.input.lease_id != e.permit.lease_id
            || r.input.expected_controller_generation != e.permit.controller_generation
        {
            return Err(Error::Stale);
        }
        if r.input.operation_id != epoch::operation_id(&e.permit, r.sequence)? {
            return Err(Error::Conflict);
        }
        if r.sequence > e.highwater.checked_add(1).ok_or(Error::Capacity)? {
            return Err(Error::Stale);
        }
        if r.sequence <= e.highwater {
            if e.receipts.get(&r.sequence) != Some(&r.input.operation_id) {
                return Err(Error::Stale);
            };
            return self.admit_input(a, &r.input, now);
        }
        let mut base = self.clone();
        if e.receipts.len() >= epoch::RECEIPT_WINDOW {
            let (&sequence, &id) = e.receipts.first_key_value().ok_or(Error::Invalid)?;
            let input = self.inputs.get(&id).ok_or(Error::Invalid)?;
            if input.command.is_some() || self.unknown.contains(&id) {
                return Err(Error::Capacity);
            }
            base.inputs.remove(&id);
            base.seen.remove(&id);
            let epoch = base.epoch.as_mut().unwrap();
            epoch.receipts.remove(&sequence);
            epoch.retired_through = sequence;
        }
        let mut plan = base.admit_input(a, &r.input, now)?;
        plan.next
            .inputs
            .get_mut(&r.input.operation_id)
            .unwrap()
            .epoch = Some(e.permit.epoch_id);
        let epoch = plan.next.epoch.as_mut().unwrap();
        epoch.highwater = r.sequence;
        epoch.receipts.insert(r.sequence, r.input.operation_id);
        plan.next.check()?;
        plan.next.encode()?;
        Ok(plan)
    }
    pub fn dispatch_epoch_input(&self, a: &Access, id: Uuid, now: u64) -> Result<Plan> {
        if !self.epoch_enabled {
            return Err(Error::Unsupported);
        }
        let e = self.epoch.as_ref().ok_or(Error::Stale)?;
        let sequence = *e
            .receipts
            .iter()
            .find(|(_, v)| **v == id)
            .map(|(k, _)| k)
            .ok_or(Error::Stale)?;
        for (_, prior) in e.receipts.range(..sequence) {
            if self.inputs.get(prior).is_some_and(|v| v.command.is_some()) {
                return Err(Error::Capacity);
            }
        }
        let mut plan = self.dispatch_raw(a, id, now)?;
        let draft::Effect::Input(command) = plan.effects.pop().ok_or(Error::Invalid)? else {
            return Err(Error::Invalid);
        };
        plan.effects.push(draft::Effect::EpochInput(epoch::Command {
            permit: e.permit.clone(),
            sequence,
            input: command,
        }));
        plan.next.check()?;
        plan.next.encode()?;
        Ok(plan)
    }
}
impl Ledger {
    pub fn epoch_retirement(&self) -> Option<epoch::Retirement> {
        self.epoch.as_ref().map(|v| epoch::Retirement {
            permit: v.permit.clone(),
            retired_through: v.retired_through,
        })
    }
    pub fn observe_epoch_input(&self, p: &draft::Peer, r: &epoch::Receipt) -> Result<Plan> {
        self.peer(p)?;
        valid(&r.receipt)?;
        let e = self.epoch.as_ref().ok_or(Error::Stale)?;
        if r.epoch_id != e.permit.epoch_id
            || r.receipt.terminal != self.terminal
            || r.sequence == 0
            || r.receipt.operation_id != epoch::operation_id(&e.permit, r.sequence)?
        {
            return Err(Error::Stale);
        }
        if r.sequence <= e.retired_through {
            return self.plan(self.clone(), vec![]);
        }
        if e.receipts.get(&r.sequence) != Some(&r.receipt.operation_id) {
            return Err(Error::Stale);
        }
        self.observe_input(p, &r.receipt)
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct LedgerSummary {
    pub terminal: TerminalIdentity,
    pub session_revision: u64,
    pub ledger_revision: u64,
    pub generation: u64,
    pub control: draft::ControlStatus,
    pub input_mode: draft::InputMode,
    pub terminal_policy: TerminalPolicy,
    pub pending_bytes: usize,
    pub mutation_count: usize,
    pub input_count: usize,
    pub subscriptions: Vec<WatchSubscription>,
    pub epoch_retirement: Option<epoch::Retirement>,
    pub resize_sequence: u64,
}
#[derive(Clone, Debug, Serialize)]
pub struct InputMetadata {
    pub operation_id: Uuid,
    pub principal: Text,
    pub request_digest: Digest,
    pub command_digest: Digest,
    pub disposition: InputDisposition,
    pub deadline_ms: u64,
    pub dispatched: bool,
    pub byte_count: usize,
    pub admission_revision: u64,
    pub epoch_id: Option<Uuid>,
}
impl std::fmt::Debug for Ledger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ledger")
            .field("terminal", &self.terminal)
            .field("revision", &self.revision)
            .field("generation", &self.generation)
            .field("input_count", &self.inputs.len())
            .field("paused", &self.paused())
            .finish_non_exhaustive()
    }
}
impl Ledger {
    pub fn load(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_LEDGER_WIRE_BYTES {
            return Err(Error::Capacity);
        }
        let value: Self = serde_json::from_slice(bytes).map_err(|_| Error::Invalid)?;
        value.check()?;
        Ok(value)
    }
    pub fn summary(&self) -> LedgerSummary {
        LedgerSummary {
            terminal: self.terminal.clone(),
            session_revision: self.session_revision,
            ledger_revision: self.revision,
            generation: self.generation,
            control: self.status(),
            input_mode: self.input_mode(),
            terminal_policy: self.terminal_policy.clone(),
            pending_bytes: self
                .inputs
                .values()
                .filter(|v| v.command.is_some())
                .map(|v| v.byte_count)
                .sum(),
            mutation_count: self.seen.len(),
            input_count: self.inputs.len(),
            subscriptions: self.viewers.values().cloned().collect(),
            epoch_retirement: self.epoch_retirement(),
            resize_sequence: self.resize_sequence,
        }
    }
    pub fn input_metadata(&self) -> Vec<InputMetadata> {
        self.inputs
            .iter()
            .map(|(id, v)| InputMetadata {
                operation_id: *id,
                principal: v.principal.clone(),
                request_digest: v.request_digest.clone(),
                command_digest: v.command_digest.clone(),
                disposition: v.disposition,
                deadline_ms: v.deadline_ms,
                dispatched: v.dispatched,
                byte_count: v.byte_count,
                admission_revision: v.admission_revision,
                epoch_id: v.epoch,
            })
            .collect()
    }
    pub fn receipt(&self, a: &Access, operation_id: Uuid, now: u64) -> Result<human::InputReceipt> {
        self.auth(a, &self.terminal, self.session_revision, true, now)?;
        let input = self.inputs.get(&operation_id).ok_or(Error::Stale)?;
        if input.principal != a.principal {
            return Err(Error::Forbidden);
        }
        Ok(human::InputReceipt {
            operation_id,
            terminal: self.terminal.clone(),
            disposition: input.disposition,
            input_paused: self.paused(),
            observed_at_ms: now,
        })
    }
}
impl Ledger {
    pub fn open_epoch_request(
        &self,
        a: &Access,
        r: &epoch::Open,
        server_nonce: Uuid,
        now: u64,
    ) -> Result<Plan> {
        r.validate()?;
        self.auth(a, &r.terminal, r.expected_session_revision, true, now)?;
        if self.seen(r.operation_id, a, r)? {
            return self.plan(self.clone(), vec![]);
        }
        self.active(a, r.lease_id, r.expected_controller_generation, now)?;
        if now >= r.expires_at_ms
            || r.expires_at_ms > now.checked_add(2000).ok_or(Error::Invalid)?
        {
            return Err(Error::Expired);
        }
        let mut plan = self.open_epoch(a, server_nonce, now)?;
        plan.next.remember(r.operation_id, a, r)?;
        plan.next.check()?;
        plan.next.encode()?;
        Ok(plan)
    }
    pub fn capture(
        &self,
        a: &Access,
        r: &draft::HumanCapture,
        server_token: Uuid,
        now: u64,
    ) -> Result<Plan> {
        self.auth(a, &r.terminal, r.expected_session_revision, true, now)?;
        if self.seen(r.operation_id, a, r)? {
            return self.plan(self.clone(), vec![]);
        }
        if now >= r.expires_at_ms
            || r.expires_at_ms > now.checked_add(2000).ok_or(Error::Invalid)?
        {
            return Err(Error::Expired);
        }
        let mut plan = self.request_capture(
            a,
            r.uncertain_operation_id,
            server_token,
            r.lease_id,
            r.expected_controller_generation,
            now,
        )?;
        let capture = plan
            .next
            .captures
            .get_mut(&server_token)
            .ok_or(Error::Invalid)?;
        capture.request.expires_at_ms = capture
            .request
            .expires_at_ms
            .min(r.expires_at_ms)
            .min(a.valid_until_ms);
        plan.effects = vec![draft::Effect::Capture(capture.request.clone())];
        plan.next.remember(r.operation_id, a, r)?;
        plan.next.check()?;
        plan.next.encode()?;
        Ok(plan)
    }
    pub fn capture_snapshot(&self, a: &Access, token: Uuid, now: u64) -> Result<Snapshot> {
        self.auth(a, &self.terminal, self.session_revision, false, now)?;
        let c = self.captures.get(&token).ok_or(Error::Stale)?;
        self.active(a, c.request.lease_id, c.request.controller_generation, now)?;
        if c.invalidated || now >= c.request.expires_at_ms {
            return Err(Error::Expired);
        }
        c.snapshot.clone().ok_or(Error::PendingFence)
    }
}
impl Ledger {
    /// Trusted common-AuthorityView reconciliation for the SAME immutable
    /// session/spec/terminal binding. No launch or ceiling extension. Old pending
    /// delivery becomes conservative unknown; all control must be reacquired.
    pub fn rebind_authority(
        &self,
        authority_revision: u64,
        execution_ceiling: u64,
        fence_id: Uuid,
    ) -> Result<Plan> {
        if execution_ceiling != self.execution_expires_at_ms
            || authority_revision <= self.session_revision
        {
            return Err(Error::Stale);
        }
        nonnil(fence_id)?;
        let mut n = self.clone();
        n.session_revision = authority_revision;
        n.generation = n.generation.checked_add(1).ok_or(Error::Capacity)?;
        n.controller = None;
        n.retire_controller_inputs();
        n.epoch = None;
        n.resize_sequence = 0;
        for (id, input) in &mut n.inputs {
            if input.command.take().is_some() {
                input.disposition = if input.dispatched {
                    n.unknown.insert(*id);
                    InputDisposition::DeliveryUnknown
                } else {
                    InputDisposition::Expired
                };
            }
        }
        for capture in n.captures.values_mut() {
            capture.snapshot = None;
            capture.invalidated = true;
        }
        let fence = draft::Fence {
            terminal: self.terminal.clone(),
            fence_id,
            generation: n.generation,
            controller: None,
        };
        n.last_fence = Some(fence.clone());
        self.plan(n, vec![draft::Effect::Fence(fence)])
    }
}

impl Ledger {
    pub fn input_mode(&self) -> draft::InputMode {
        if self.epoch_enabled {
            draft::InputMode::EpochV1
        } else {
            draft::InputMode::FiniteV1
        }
    }
}

impl Ledger {
    pub fn queued_inputs(&self) -> Vec<Uuid> {
        let mut values: Vec<_> = self
            .inputs
            .iter()
            .filter(|(_, v)| {
                v.disposition == InputDisposition::Queued
                    && !v.dispatched
                    && v.command.as_ref().is_some_and(|c| {
                        self.controller.as_ref().is_some_and(|owner| {
                            owner.lease.lease_id == c.lease.lease_id
                                && owner.lease.generation == c.lease.generation
                        })
                    })
            })
            .map(|(id, v)| (v.admission_revision, *id))
            .collect();
        values.sort_unstable();
        values.into_iter().map(|(_, id)| id).collect()
    }
    pub fn inspected_receipt(
        &self,
        a: &Access,
        operation_id: Uuid,
        now: u64,
    ) -> Result<human::InputReceipt> {
        self.auth(a, &self.terminal, self.session_revision, false, now)?;
        let input = self.inputs.get(&operation_id).ok_or(Error::Stale)?;
        Ok(human::InputReceipt {
            operation_id,
            terminal: self.terminal.clone(),
            disposition: input.disposition,
            input_paused: self.paused(),
            observed_at_ms: now,
        })
    }
}

fn valid_policy(p: &TerminalPolicy) -> Result<()> {
    if p.max_input_bytes == 0
        || p.max_input_bytes as usize > MAX_INPUT_BYTES
        || p.max_pending_input_bytes < p.max_input_bytes
        || p.max_pending_input_bytes as usize > MAX_PENDING_INPUT_BYTES
        || p.max_frame_bytes == 0
        || p.max_frame_bytes as usize > MAX_FRAME_BYTES
        || p.replay_bytes < p.max_frame_bytes
        || p.replay_bytes as usize > MAX_REPLAY_BYTES
    {
        return Err(Error::Invalid);
    }
    Ok(())
}

impl Ledger {
    /// A replay queue is never an input authority, regardless of plaintext queue
    /// flags. Fresh input effects come exclusively from a successful one-shot CAS.
    pub fn authorize_replayable_effect(
        &self,
        a: Option<&Access>,
        effect: &draft::Effect,
        now: u64,
    ) -> Result<()> {
        effect.validate().map_err(|_| Error::Invalid)?;
        if effect.terminal() != self.terminal() {
            return Err(Error::Stale);
        }
        match effect {
            draft::Effect::Input(_) | draft::Effect::EpochInput(_) => Err(Error::Transition),
            draft::Effect::Fence(f) if f.controller.is_none() => {
                if self.last_fence.as_ref() != Some(f) {
                    return Err(Error::Stale);
                }
                Ok(())
            }
            draft::Effect::Fence(f) => {
                let a = a.ok_or(Error::Forbidden)?;
                self.auth(a, self.terminal(), self.session_revision, true, now)?;
                let c = self.controller.as_ref().ok_or(Error::Stale)?;
                if self.last_fence.as_ref() != Some(f)
                    || f.controller.as_ref() != Some(&c.lease)
                    || c.fence_id != f.fence_id
                    || !a.execution_active
                    || now >= a.execution_expires_at_ms
                    || now >= self.execution_expires_at_ms
                    || now < c.lease.issued_at_ms
                    || now >= c.lease.expires_at_ms
                    || c.lease.expires_at_ms > a.valid_until_ms
                    || c.lease.expires_at_ms > a.execution_expires_at_ms
                    || c.lease.principal_id != a.principal
                {
                    return Err(Error::Stale);
                }
                Ok(())
            }
            draft::Effect::Watch(w) => {
                let a = a.ok_or(Error::Forbidden)?;
                self.authorize_watch(a, w.subscription_id, now)?;
                if self.viewers.get(&w.subscription_id) != Some(w)
                    || w.expires_at_ms > a.valid_until_ms
                {
                    return Err(Error::Stale);
                }
                Ok(())
            }
            draft::Effect::Resize(r) => {
                let a = a.ok_or(Error::Forbidden)?;
                let lease = self.active(a, r.lease.lease_id, r.lease.generation, now)?;
                self.auth(a, self.terminal(), self.session_revision, true, now)?;
                let command_digest = digest(r)?;
                if lease != &r.lease
                    || r.sequence != self.resize_sequence
                    || now >= r.deadline_ms
                    || r.deadline_ms > a.valid_until_ms
                    || r.deadline_ms > a.execution_expires_at_ms
                    || self
                        .resize_digests
                        .get(&r.operation_id)
                        .is_none_or(|(d, s, applied)| {
                            *d != command_digest || *s != r.sequence || applied.is_some()
                        })
                {
                    return Err(Error::Stale);
                }
                Ok(())
            }
            draft::Effect::Capture(c) => {
                let a = a.ok_or(Error::Forbidden)?;
                self.auth(a, self.terminal(), self.session_revision, true, now)?;
                if !a.watch {
                    return Err(Error::Forbidden);
                }
                self.active(a, c.lease_id, c.controller_generation, now)?;
                let state = self
                    .captures
                    .get(&c.capture_request_id)
                    .ok_or(Error::Stale)?;
                if state.invalidated
                    || state.snapshot_id.is_some()
                    || state.request != *c
                    || !self.unknown.contains(&c.uncertain_operation_id)
                    || now >= c.expires_at_ms
                    || c.expires_at_ms > a.valid_until_ms
                    || c.expires_at_ms > a.execution_expires_at_ms
                {
                    return Err(Error::Stale);
                }
                Ok(())
            }
        }
    }
}

impl Ledger {
    /// Trusted adapter refusal after current authority/scope is proved absent.
    /// Retains immutable IDs/digests and all dispatched/unknown outcomes; only
    /// commands that canonical state proves never dispatched become Rejected.
    pub fn reject_queued_for(&self, principal: &Text) -> Result<Plan> {
        let mut next = self.clone();
        for input in next.inputs.values_mut() {
            if input.principal == *principal
                && !input.dispatched
                && input.disposition == InputDisposition::Queued
                && input.command.take().is_some()
            {
                input.disposition = InputDisposition::Rejected;
            }
        }
        self.plan(next, vec![])
    }
}
