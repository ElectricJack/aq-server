//! Consistency checks for durable ledgers before loading or committing them.
use super::*;

fn invariant(holds: bool) -> Result<()> {
    holds.then_some(()).ok_or(Error::Invalid)
}

impl Ledger {
    pub(super) fn check(&self) -> Result<()> {
        valid_policy(&self.terminal_policy)?;
        if !self.terminal_policy.input_enabled {
            invariant(self.controller.is_none() && self.inputs.is_empty() && !self.epoch_enabled)?;
        }
        valid(&self.terminal)?;
        self.check_storage_bounds()?;
        self.check_controller()?;
        if let Some(epoch) = &self.epoch {
            epoch.check(self)?;
        }
        let pending = self.pending_input_bytes()?;
        for (id, capture) in &self.captures {
            capture.check(self, *id)?;
        }
        invariant(pending <= self.terminal_policy.max_pending_input_bytes as usize)?;
        invariant(self.unknown.iter().all(|id| {
            self.inputs
                .get(id)
                .is_some_and(|input| input.disposition == InputDisposition::DeliveryUnknown)
        }))
    }

    fn pending_input_bytes(&self) -> Result<usize> {
        let mut pending = 0usize;
        for (id, input) in &self.inputs {
            let bytes = input.check(self, *id)?;
            pending = pending.checked_add(bytes).ok_or(Error::Capacity)?;
        }
        Ok(pending)
    }

    fn check_storage_bounds(&self) -> Result<()> {
        invariant(self.format == 1)?;
        invariant(self.session_revision > 0)?;
        let counts = [
            self.seen.len(),
            self.inputs.len(),
            self.captures.len(),
            self.resize_digests.len(),
        ];
        invariant(counts.into_iter().all(|count| count <= MAX_OPERATIONS))?;
        invariant(self.viewers.len() <= MAX_VIEWERS)?;
        invariant(self.unknown.len() <= self.inputs.len())?;
        invariant(
            self.execution_expires_at_ms > 0 && self.expiry_margin_ms.is_none_or(|v| v <= 5000),
        )?;
        invariant(
            !self
                .seen
                .keys()
                .chain(self.captures.keys())
                .any(Uuid::is_nil),
        )
    }

    fn check_controller(&self) -> Result<()> {
        if let Some(fence) = &self.last_fence {
            invariant(fence.generation == self.generation && fence.terminal == self.terminal)?;
            nonnil(fence.fence_id)?;
        }
        if let Some(controller) = &self.controller {
            let lease = &controller.lease;
            valid(lease)?;
            invariant(lease.terminal == self.terminal && lease.generation == self.generation)?;
            invariant(lease.execution_expires_at_ms <= self.execution_expires_at_ms)?;
        }
        Ok(())
    }
}

impl EpochState {
    fn check(&self, ledger: &Ledger) -> Result<()> {
        let permit = &self.permit;
        permit.validate()?;
        invariant(ledger.epoch_enabled && permit.terminal == ledger.terminal)?;
        invariant(permit.controller_generation == ledger.generation)?;
        let lease = &ledger.controller.as_ref().ok_or(Error::Invalid)?.lease;
        invariant(
            lease.lease_id == permit.lease_id && permit.expires_at_ms <= lease.expires_at_ms,
        )?;
        invariant(self.receipts.len() <= epoch::RECEIPT_WINDOW)?;
        invariant(
            self.highwater.checked_sub(self.retired_through) == Some(self.receipts.len() as u64),
        )?;
        for (sequence, id) in &self.receipts {
            invariant(*sequence > self.retired_through && *sequence <= self.highwater)?;
            invariant(epoch::operation_id(permit, *sequence)? == *id)?;
            let input = ledger.inputs.get(id).ok_or(Error::Invalid)?;
            invariant(input.epoch == Some(permit.epoch_id))?;
        }
        Ok(())
    }
}

impl Input {
    fn check(&self, ledger: &Ledger, id: Uuid) -> Result<usize> {
        let seen = ledger.seen.get(&id).ok_or(Error::Invalid)?;
        nonnil(id)?;
        invariant(seen.principal == self.principal && seen.digest == self.request_digest)?;
        invariant((1..=ledger.revision).contains(&self.admission_revision))?;
        invariant(
            (1..=ledger.terminal_policy.max_input_bytes as usize).contains(&self.byte_count),
        )?;
        invariant(self.has_valid_disposition())?;
        invariant(self.epoch.is_none() || ledger.epoch_enabled)?;
        match &self.command {
            Some(command) => self.check_command(command, &ledger.terminal, id),
            None => Ok(0),
        }
    }

    fn check_command(
        &self,
        command: &InputCommand,
        terminal: &TerminalIdentity,
        id: Uuid,
    ) -> Result<usize> {
        command.validate_envelope().map_err(|_| Error::Invalid)?;
        invariant(command.operation_id == id && &command.lease.terminal == terminal)?;
        invariant(digest(command)? == self.command_digest)?;
        invariant(
            command.bytes.len() == self.byte_count && command.deadline_ms == self.deadline_ms,
        )?;
        Ok(command.bytes.len())
    }

    fn has_valid_disposition(&self) -> bool {
        use InputDisposition::*;
        match self.disposition {
            Queued => self.command.is_some(),
            Received | Injecting => self.dispatched && self.command.is_some(),
            DeliveryUnknown | Applied => self.dispatched && self.command.is_none(),
            Rejected | Expired => self.command.is_none(),
        }
    }
}

impl CaptureState {
    fn check(&self, ledger: &Ledger, id: Uuid) -> Result<()> {
        invariant(
            id == self.request.capture_request_id && self.request.terminal == ledger.terminal,
        )?;
        invariant(self.request.after_ledger_revision <= ledger.revision)?;
        if let Some(snapshot) = &self.snapshot {
            valid(snapshot)?;
            invariant(
                snapshot.capture_request_id == id && self.snapshot_id == Some(snapshot.snapshot_id),
            )?;
            invariant(self.snapshot_digest.as_ref() == Some(&digest(snapshot)?))?;
        }
        Ok(())
    }
}
