//! Admission, FIFO dispatch and receipt transitions for terminal input.
use super::*;

impl Ledger {
    pub fn input(&self, a: &Access, r: &human::InputRequest, now: u64) -> Result<Plan> {
        if self.epoch_enabled {
            return Err(Error::Unsupported);
        }
        self.admit_input(a, r, now)
    }
    fn check_input_length(&self, request: &human::InputRequest) -> Result<()> {
        if (1..=self.terminal_policy.max_input_bytes as usize).contains(&request.bytes.len()) {
            Ok(())
        } else {
            Err(Error::Invalid)
        }
    }

    pub(super) fn admit_input(
        &self,
        a: &Access,
        r: &human::InputRequest,
        now: u64,
    ) -> Result<Plan> {
        self.check_input_length(r)?;
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
        if r.expires_at_ms > a.control_deadline() {
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
            escape_key: r.escape_key,
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
        self.check_input_length(r)?;
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
    pub(super) fn dispatch_raw(&self, a: &Access, id: Uuid, now: u64) -> Result<Plan> {
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
        if !complete_terminal_input(&c.bytes, c.escape_key) {
            return Err(Error::Transition);
        }
        self.auth(a, &c.lease.terminal, self.session_revision, true, now)?;
        self.active(a, c.lease.lease_id, c.lease.generation, now)?;
        if !a.covers_deadline(now, input.deadline_ms) {
            return Err(Error::Expired);
        }
        let first = self
            .queued_for_lease(&c.lease)
            .min_by_key(|(_, input)| input.admission_revision);
        if first.map(|(id, _)| *id) != Some(id) {
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
        if !old.can_transition_to(r.disposition) {
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
    fn queued_for_lease<'a>(
        &'a self,
        lease: &'a ControllerLease,
    ) -> impl Iterator<Item = (&'a Uuid, &'a Input)> {
        self.inputs
            .iter()
            .filter(move |(_, input)| input.is_queued_for(lease))
    }

    pub fn queued_inputs(&self) -> Vec<Uuid> {
        let Some(owner) = &self.controller else {
            return Vec::new();
        };
        let mut values: Vec<_> = self
            .queued_for_lease(&owner.lease)
            .map(|(id, v)| (v.admission_revision, *id))
            .collect();
        values.sort_unstable();
        values.into_iter().map(|(_, id)| id).collect()
    }

    /// Trusted adapter refusal after current authority/scope is proved absent.
    /// Retains immutable IDs/digests and all dispatched/unknown outcomes; only
    /// commands that canonical state proves never dispatched become Rejected.
    pub fn reject_queued_for(&self, principal: &Text) -> Result<Plan> {
        let mut next = self.clone();
        for input in next.inputs.values_mut() {
            if input.principal != *principal || input.dispatched {
                continue;
            }
            if input.disposition == InputDisposition::Queued && input.command.take().is_some() {
                input.disposition = InputDisposition::Rejected;
            }
        }
        self.plan(next, vec![])
    }
}

impl Input {
    fn is_queued_for(&self, lease: &ControllerLease) -> bool {
        self.disposition == InputDisposition::Queued
            && !self.dispatched
            && self.command.as_ref().is_some_and(|c| {
                c.lease.lease_id == lease.lease_id && c.lease.generation == lease.generation
            })
    }
    fn can_transition_to(&self, next: InputDisposition) -> bool {
        fn rank(d: InputDisposition) -> u8 {
            match d {
                InputDisposition::Queued => 0,
                InputDisposition::Received => 1,
                InputDisposition::Injecting => 2,
                _ => 3,
            }
        }
        self.dispatched
            && self.command.is_some()
            && next != InputDisposition::Queued
            && rank(next) >= rank(self.disposition)
    }
}
