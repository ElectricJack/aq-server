use super::*;

impl Run {
    /// Produces only another data model; callers must retain the old run and its holds.
    pub fn successor(
        &self,
        plan: CentralPlan,
        clock: Clock,
        history: &MailHistoryFence,
    ) -> Result<Self> {
        self.validate()?;
        plan.validate()?;
        clock.validate()?;
        if clock.machine_boot_id == self.last_clock.machine_boot_id
            && clock.boot_ms < self.last_clock.boot_ms
        {
            return Err(Error::Refused(Refusal::Clock));
        }
        if !matches!(
            self.phase,
            Phase::StoppedReported | Phase::CancelledBeforeStart
        ) {
            return Err(Error::Refused(Refusal::Phase));
        }
        self.validate_successor_identity(&plan)?;
        validate_successor_history(&plan, self.plan.generation, history)?;
        self.validate_reused_mail(&plan, history)?;
        Self::admitted(plan, clock)
    }
    fn validate_successor_identity(&self, plan: &CentralPlan) -> Result<()> {
        if self.conflicts_with_successor(plan) {
            return Err(Error::Refused(Refusal::Binding));
        }
        if let (
            SessionIdentity::Central { session_id: a, .. },
            SessionIdentity::Central { session_id: b, .. },
        ) = (&self.plan.session, &plan.session)
            && a == b
        {
            return Err(Error::Refused(Refusal::Binding));
        }
        Ok(())
    }
    fn conflicts_with_successor(&self, plan: &CentralPlan) -> bool {
        self.plan.subject.actor_id() != plan.subject.actor_id()
            || self.plan.subject.conversation_id() != plan.subject.conversation_id()
            || self.plan.generation.checked_add(1) != Some(plan.generation)
            || [plan.dispatch_id, plan.host_operation_id]
                .iter()
                .any(|id| [self.plan.dispatch_id, self.plan.host_operation_id].contains(id))
            || self.plan.session == plan.session
    }
    fn validate_reused_mail(&self, plan: &CentralPlan, history: &MailHistoryFence) -> Result<()> {
        for next in &plan.batch.messages {
            if let Some(old) = self
                .plan
                .batch
                .messages
                .iter()
                .find(|m| m.message_id == next.message_id || m.cursor == next.cursor)
            {
                self.validate_retry(old, next, history)?;
            }
        }
        Ok(())
    }
    fn validate_retry(
        &self,
        old: &MailRef,
        next: &MailRef,
        history: &MailHistoryFence,
    ) -> Result<()> {
        if old != next {
            return Err(Error::Refused(Refusal::Conflict));
        }
        let retryable = matches!(&self.dispatch, DispatchState::NotAttempted)
            || matches!(&self.dispatch, DispatchState::Resolved { evidence } if evidence.disposition == HandoffDisposition::NotHandedOff);
        if !retryable {
            return Err(Error::Refused(Refusal::BatchHeld));
        }
        if let DispatchState::Resolved { evidence } = &self.dispatch {
            let entry = history
                .entries
                .iter()
                .find(|e| e.message.message_id == next.message_id)
                .expect("complete coverage");
            if !matches!(&entry.disposition,HistoryDisposition::Retryable{evidence:found} if found==evidence.as_ref())
            {
                return Err(Error::Refused(Refusal::Conflict));
            }
        }
        Ok(())
    }
}
