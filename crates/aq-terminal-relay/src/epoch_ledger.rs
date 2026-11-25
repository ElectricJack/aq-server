//! Negotiated epoch input sequencing and bounded receipt retirement.
use super::*;

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
        if e.is_repeated_request(r)? {
            return self.admit_input(a, &r.input, now);
        }
        let mut base = self.clone();
        base.make_room_for_epoch_receipt()?;
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
    fn make_room_for_epoch_receipt(&mut self) -> Result<()> {
        let epoch = self.epoch.as_mut().ok_or(Error::Stale)?;
        if epoch.receipts.len() < epoch::RECEIPT_WINDOW {
            return Ok(());
        }
        let (&sequence, &id) = epoch.receipts.first_key_value().ok_or(Error::Invalid)?;
        let input = self.inputs.get(&id).ok_or(Error::Invalid)?;
        if input.command.is_some() || self.unknown.contains(&id) {
            return Err(Error::Capacity);
        }
        self.inputs.remove(&id);
        self.seen.remove(&id);
        epoch.receipts.remove(&sequence);
        epoch.retired_through = sequence;
        Ok(())
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
        if r.epoch_id != e.permit.epoch_id || r.receipt.terminal != self.terminal {
            return Err(Error::Stale);
        }
        if r.sequence == 0 || r.receipt.operation_id != epoch::operation_id(&e.permit, r.sequence)?
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

impl EpochState {
    fn is_repeated_request(&self, request: &epoch::Request) -> Result<bool> {
        let permit = &self.permit;
        if request.epoch_id != permit.epoch_id || request.sequence <= self.retired_through {
            return Err(Error::Stale);
        }
        if request.input.lease_id != permit.lease_id
            || request.input.expected_controller_generation != permit.controller_generation
        {
            return Err(Error::Stale);
        }
        if request.input.operation_id != epoch::operation_id(permit, request.sequence)? {
            return Err(Error::Conflict);
        }
        let next = self.highwater.checked_add(1).ok_or(Error::Capacity)?;
        match request.sequence.cmp(&next) {
            std::cmp::Ordering::Greater => Err(Error::Stale),
            std::cmp::Ordering::Equal => Ok(false),
            std::cmp::Ordering::Less
                if self.receipts.get(&request.sequence) == Some(&request.input.operation_id) =>
            {
                Ok(true)
            }
            _ => Err(Error::Stale),
        }
    }
}
