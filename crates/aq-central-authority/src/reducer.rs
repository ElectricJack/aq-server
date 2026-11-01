use super::*;
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Prepared,
    StartCommitted,
    RunningReported,
    StopRequested,
    StopUncertain,
    StoppedReported,
    CancelledBeforeStart,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CancelReason {
    Operator,
    AuthorityRevoked,
    PermitExpired,
    WallDeadline,
    HostLost,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UncertaintyReason {
    Cancellation,
    Expiry,
    HostLost,
    Reported,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DispatchState {
    NotAttempted,
    IntentCommitted,
    Unknown { reason: UncertaintyReason },
    Resolved { evidence: Box<HandoffEvidence> },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Refusal {
    Clock,
    Binding,
    Phase,
    Inactive,
    Sequence,
    Conflict,
    Deadline,
    NeedsReconciliation,
    KnownOutcome,
    BatchHeld,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Decision {
    Applied,
    Replayed,
    Refused { reason: Refusal },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Event {
    CommitStart {
        launch: HostLaunchRef,
        dispatch: DispatchAssociation,
    },
    Running {
        observation: HostObservationRef,
    },
    Renew {
        permit: PermitData,
    },
    Cancel {
        reason: CancelReason,
    },
    Tick,
    StopUncertain {
        observation: HostObservationRef,
    },
    HostLost {
        binding: CentralBinding,
        launch_id: Uuid,
        report: ArtifactRef,
    },
    Stopped {
        evidence: StopEvidenceRef,
    },
    Handoff {
        evidence: HandoffEvidence,
    },
    ReconcileHandoff {
        evidence: HandoffEvidence,
    },
    ReportUnknown {
        association: DispatchAssociation,
    },
}
impl Event {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::CommitStart { launch, dispatch } => {
                launch.validate()?;
                dispatch.validate()
            }
            Self::Running { observation } | Self::StopUncertain { observation } => {
                observation.validate()
            }
            Self::Renew { permit } => permit.validate(),
            Self::Stopped { evidence } => evidence.validate(),
            Self::Handoff { evidence } | Self::ReconcileHandoff { evidence } => evidence.validate(),
            Self::ReportUnknown { association } => association.validate(),
            Self::HostLost {
                binding,
                launch_id,
                report,
            } => {
                binding.validate()?;
                id(*launch_id)?;
                artifact(report)
            }
            Self::Cancel { .. } | Self::Tick => Ok(()),
        }
    }
}
/// Serialized model state, not signed authority, physical proof or a grant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Run {
    plan: CentralPlan,
    admitted: Clock,
    last_clock: Clock,
    original_deadline_boot_ms: u64,
    phase: Phase,
    permit: Option<PermitData>,
    launch: Option<HostLaunchRef>,
    guardian: Option<ArtifactRef>,
    host_observation: Option<HostObservationRef>,
    stop: Option<StopEvidenceRef>,
    uncertain_report: Option<ArtifactRef>,
    cancellation: Option<CancelReason>,
    association: Option<DispatchAssociation>,
    dispatch: DispatchState,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Transition {
    pub state: Run,
    pub decision: Decision,
}
fn refused(reason: Refusal) -> Decision {
    Decision::Refused { reason }
}
impl Run {
    pub fn new(plan: CentralPlan, clock: Clock) -> Result<Self> {
        ensure(plan.generation == 1, "initial generation must be one")?;
        Self::admitted(plan, clock)
    }
    fn admitted(plan: CentralPlan, clock: Clock) -> Result<Self> {
        plan.validate()?;
        clock.validate()?;
        let original_deadline_boot_ms = clock
            .boot_ms
            .checked_add(plan.wall_time_ms)
            .ok_or(Error::Invalid("deadline overflow"))?;
        Ok(Self {
            plan,
            admitted: clock.clone(),
            last_clock: clock,
            original_deadline_boot_ms,
            phase: Phase::Prepared,
            permit: None,
            launch: None,
            guardian: None,
            host_observation: None,
            stop: None,
            uncertain_report: None,
            cancellation: None,
            association: None,
            dispatch: DispatchState::NotAttempted,
        })
    }
    pub fn plan(&self) -> &CentralPlan {
        &self.plan
    }
    pub fn phase(&self) -> Phase {
        self.phase
    }
    pub fn original_deadline_boot_ms(&self) -> u64 {
        self.original_deadline_boot_ms
    }
    pub fn permit(&self) -> Option<&PermitData> {
        self.permit.as_ref()
    }
    pub fn association(&self) -> Option<&DispatchAssociation> {
        self.association.as_ref()
    }
    pub fn dispatch(&self) -> &DispatchState {
        &self.dispatch
    }
    pub fn cancellation(&self) -> Option<CancelReason> {
        self.cancellation
    }
    fn active(&self) -> bool {
        matches!(self.phase, Phase::StartCommitted | Phase::RunningReported)
    }
    fn permit_fits(&self, p: &PermitData) -> bool {
        p.binding == self.plan.binding().expect("validated plan")
            && p.machine_boot_id == self.admitted.machine_boot_id
            && p.issued_boot_ms >= self.admitted.boot_ms
            && p.issued_boot_ms <= self.last_clock.boot_ms
            && p.expires_boot_ms > self.last_clock.boot_ms
            && p.expires_boot_ms <= self.original_deadline_boot_ms
            && p.expires_boot_ms - p.issued_boot_ms <= self.plan.max_permit_ms
    }
    fn host_matches(&self, h: &HostObservationRef) -> bool {
        self.launch
            .as_ref()
            .is_some_and(|l| l.binding == h.binding && l.launch_id == h.launch_id)
            && self.guardian.as_ref().is_none_or(|g| g == &h.guardian)
    }
    fn unknown(&mut self, reason: UncertaintyReason) {
        if matches!(self.dispatch, DispatchState::IntentCommitted) {
            self.dispatch = DispatchState::Unknown { reason };
        }
    }
    fn stopping(&mut self, reason: CancelReason, uncertainty: UncertaintyReason) {
        self.cancellation.get_or_insert(reason);
        self.unknown(uncertainty);
        if self.active() {
            self.phase = Phase::StopRequested;
        }
    }
    fn expire(&mut self) {
        if self.phase == Phase::Prepared
            && self.last_clock.boot_ms >= self.original_deadline_boot_ms
        {
            self.phase = Phase::CancelledBeforeStart;
            self.cancellation = Some(CancelReason::WallDeadline);
        } else if self.active() {
            if self.last_clock.boot_ms >= self.original_deadline_boot_ms {
                self.stopping(CancelReason::WallDeadline, UncertaintyReason::Expiry);
            } else if self
                .permit
                .as_ref()
                .is_some_and(|p| self.last_clock.boot_ms >= p.expires_boot_ms)
            {
                self.stopping(CancelReason::PermitExpired, UncertaintyReason::Expiry);
            }
        }
    }
    pub fn validate(&self) -> Result<()> {
        self.plan.validate()?;
        self.admitted.validate()?;
        self.last_clock.validate()?;
        ensure(
            self.admitted.machine_boot_id == self.last_clock.machine_boot_id
                && self.last_clock.boot_ms >= self.admitted.boot_ms
                && self.admitted.boot_ms.checked_add(self.plan.wall_time_ms)
                    == Some(self.original_deadline_boot_ms),
            "run clock/deadline",
        )?;
        let binding = self.plan.binding()?;
        if let Some(p) = &self.permit {
            p.validate()?;
            ensure(
                p.binding == binding
                    && p.machine_boot_id == self.admitted.machine_boot_id
                    && p.issued_boot_ms >= self.admitted.boot_ms
                    && p.issued_boot_ms <= self.last_clock.boot_ms
                    && p.expires_boot_ms <= self.original_deadline_boot_ms
                    && p.expires_boot_ms - p.issued_boot_ms <= self.plan.max_permit_ms,
                "run permit",
            )?;
        }
        if let Some(a) = &self.association {
            a.validate()?;
            ensure(
                a == &self.plan.dispatch(a.permit.clone())?
                    && a.permit.sequence == 1
                    && a.permit.machine_boot_id == self.admitted.machine_boot_id
                    && a.permit.issued_boot_ms >= self.admitted.boot_ms
                    && a.permit.expires_boot_ms <= self.original_deadline_boot_ms
                    && a.permit.issued_boot_ms <= self.last_clock.boot_ms
                    && a.permit.expires_boot_ms - a.permit.issued_boot_ms
                        <= self.plan.max_permit_ms,
                "original dispatch association",
            )?;
            let p = self
                .permit
                .as_ref()
                .ok_or(Error::Invalid("dispatch without permit"))?;
            ensure(
                p.sequence >= a.permit.sequence
                    && p.issued_boot_ms >= a.permit.issued_boot_ms
                    && p.expires_boot_ms >= a.permit.expires_boot_ms
                    && (p.sequence != a.permit.sequence || *p == a.permit),
                "permit high watermark",
            )?;
        }
        if let Some(l) = &self.launch {
            l.validate()?;
            ensure(l.binding == binding, "launch binding")?;
        }
        if let Some(r) = &self.uncertain_report {
            artifact(r)?;
        }
        if let Some(g) = &self.guardian {
            artifact(g)?;
        }
        if let Some(h) = &self.host_observation {
            h.validate()?;
            ensure(self.host_matches(h), "host observation association")?;
        }
        if let Some(s) = &self.stop {
            s.validate()?;
            ensure(
                self.host_matches(&s.host) && self.guardian.as_ref() == Some(&s.host.guardian),
                "stop association",
            )?;
        }
        match &self.dispatch {
            DispatchState::NotAttempted => {
                ensure(self.association.is_none(), "unattempted association")?
            }
            DispatchState::Resolved { evidence } => {
                evidence.validate()?;
                ensure(
                    self.association.as_ref() == Some(&evidence.association),
                    "handoff association",
                )?;
            }
            _ => ensure(self.association.is_some(), "dispatch intent absent")?,
        }
        match self.phase {
            Phase::Prepared | Phase::CancelledBeforeStart => ensure(
                self.permit.is_none()
                    && self.launch.is_none()
                    && self.guardian.is_none()
                    && self.stop.is_none()
                    && self.uncertain_report.is_none()
                    && matches!(self.dispatch, DispatchState::NotAttempted),
                "prestart state",
            )?,
            _ => ensure(
                self.permit.is_some() && self.launch.is_some() && self.association.is_some(),
                "started state",
            )?,
        }
        ensure(
            (self.phase == Phase::StoppedReported) == self.stop.is_some(),
            "stopped evidence",
        )?;
        if self.phase == Phase::Prepared {
            ensure(
                self.cancellation.is_none()
                    && self.last_clock.boot_ms < self.original_deadline_boot_ms,
                "cancelled or expired prepared state",
            )?;
        }
        if self.phase == Phase::RunningReported {
            ensure(
                self.guardian.is_some() && self.host_observation.is_some(),
                "running reference",
            )?;
        }
        if matches!(
            self.phase,
            Phase::CancelledBeforeStart | Phase::StopRequested | Phase::StopUncertain
        ) {
            ensure(self.cancellation.is_some(), "stop cause")?;
        }
        if matches!(
            self.phase,
            Phase::StopRequested | Phase::StopUncertain | Phase::StoppedReported
        ) {
            ensure(
                !matches!(self.dispatch, DispatchState::IntentCommitted),
                "stopped input must be resolved or unknown",
            )?;
        }
        if self.phase == Phase::StopUncertain {
            ensure(self.uncertain_report.is_some(), "uncertain stop report")?;
        }
        if self.active() {
            ensure(
                self.cancellation.is_none()
                    && self.last_clock.boot_ms < self.original_deadline_boot_ms
                    && self.last_clock.boot_ms
                        < self.permit.as_ref().expect("started").expires_boot_ms,
                "active model expired/cancelled",
            )?;
        }
        Ok(())
    }
    /// Pure data transition. Even Applied is NOT permission to perform any effect.
    pub fn transition(&self, event: &Event, clock: Clock) -> Result<Transition> {
        self.validate()?;
        event.validate()?;
        clock.validate()?;
        if clock.machine_boot_id != self.admitted.machine_boot_id
            || clock.boot_ms < self.last_clock.boot_ms
        {
            return Ok(Transition {
                state: self.clone(),
                decision: refused(Refusal::Clock),
            });
        }
        let mut next = self.clone();
        next.last_clock = clock;
        next.expire();
        let decision = next.apply(event);
        next.validate()?;
        Ok(Transition {
            state: next,
            decision,
        })
    }
    fn apply(&mut self, event: &Event) -> Decision {
        match event {
            Event::Tick => Decision::Applied,
            Event::CommitStart { launch, dispatch } => {
                if self.active()
                    && self.launch.as_ref() == Some(launch)
                    && self.association.as_ref() == Some(dispatch)
                {
                    return Decision::Replayed;
                }
                if self.phase != Phase::Prepared {
                    return refused(Refusal::Phase);
                }
                if launch.binding != self.plan.binding().expect("validated")
                    || self.plan.dispatch(dispatch.permit.clone()).ok().as_ref() != Some(dispatch)
                    || dispatch.permit.binding != launch.binding
                {
                    return refused(Refusal::Binding);
                }
                if dispatch.permit.sequence != 1 {
                    return refused(Refusal::Sequence);
                }
                if !self.permit_fits(&dispatch.permit) {
                    return refused(Refusal::Deadline);
                }
                self.permit = Some(dispatch.permit.clone());
                self.launch = Some(launch.clone());
                self.association = Some(dispatch.clone());
                self.dispatch = DispatchState::IntentCommitted;
                self.phase = Phase::StartCommitted;
                Decision::Applied
            }
            Event::Renew { permit } => {
                if !self.active() {
                    return refused(Refusal::Inactive);
                }
                if permit.binding != self.plan.binding().expect("validated")
                    || permit.machine_boot_id != self.admitted.machine_boot_id
                {
                    return refused(Refusal::Binding);
                }
                let old = self.permit.as_ref().expect("active");
                if old == permit {
                    return Decision::Replayed;
                }
                if permit.sequence == old.sequence {
                    return refused(Refusal::Conflict);
                }
                if old.sequence.checked_add(1) != Some(permit.sequence) {
                    return refused(Refusal::Sequence);
                }
                if !self.permit_fits(permit)
                    || permit.issued_boot_ms < old.issued_boot_ms
                    || permit.expires_boot_ms <= old.expires_boot_ms
                {
                    return refused(Refusal::Deadline);
                }
                self.permit = Some(permit.clone());
                Decision::Applied
            }
            Event::Running { observation } => {
                if !self.host_matches(observation) {
                    return refused(Refusal::Binding);
                }
                if self.host_observation.as_ref() == Some(observation) {
                    return Decision::Replayed;
                }
                self.guardian = Some(observation.guardian.clone());
                self.host_observation = Some(observation.clone());
                if self.phase == Phase::StartCommitted {
                    self.phase = Phase::RunningReported;
                }
                Decision::Applied
            }
            Event::Cancel { reason } => {
                if matches!(
                    self.phase,
                    Phase::StoppedReported | Phase::CancelledBeforeStart
                ) {
                    return Decision::Replayed;
                }
                if self.phase == Phase::Prepared {
                    self.phase = Phase::CancelledBeforeStart;
                    self.cancellation = Some(*reason);
                } else {
                    self.stopping(*reason, UncertaintyReason::Cancellation);
                }
                Decision::Applied
            }
            Event::StopUncertain { observation } => {
                if self.phase == Phase::StoppedReported {
                    return refused(Refusal::KnownOutcome);
                }
                if !self.host_matches(observation) {
                    return refused(Refusal::Binding);
                }
                self.guardian = Some(observation.guardian.clone());
                self.host_observation = Some(observation.clone());
                self.uncertain_report
                    .get_or_insert(observation.observation.clone());
                self.cancellation.get_or_insert(CancelReason::HostLost);
                self.unknown(UncertaintyReason::HostLost);
                self.phase = Phase::StopUncertain;
                Decision::Applied
            }
            Event::HostLost {
                binding,
                launch_id,
                report,
            } => {
                if self.phase == Phase::StoppedReported {
                    return refused(Refusal::KnownOutcome);
                }
                if !self
                    .launch
                    .as_ref()
                    .is_some_and(|l| l.binding == *binding && l.launch_id == *launch_id)
                {
                    return refused(Refusal::Binding);
                }
                self.cancellation.get_or_insert(CancelReason::HostLost);
                self.uncertain_report.get_or_insert(report.clone());
                self.unknown(UncertaintyReason::HostLost);
                self.phase = Phase::StopUncertain;
                Decision::Applied
            }
            Event::Stopped { evidence } => {
                if !self.host_matches(&evidence.host) {
                    return refused(Refusal::Binding);
                }
                if let Some(old) = &self.stop {
                    return if old == evidence {
                        Decision::Replayed
                    } else {
                        refused(Refusal::Conflict)
                    };
                }
                self.guardian = Some(evidence.host.guardian.clone());
                self.host_observation = Some(evidence.host.clone());
                self.stop = Some(evidence.clone());
                self.unknown(UncertaintyReason::HostLost);
                self.phase = Phase::StoppedReported;
                Decision::Applied
            }
            Event::ReportUnknown { association } => {
                if self.association.as_ref() != Some(association) {
                    return refused(Refusal::Binding);
                }
                match self.dispatch {
                    DispatchState::IntentCommitted => {
                        self.unknown(UncertaintyReason::Reported);
                        Decision::Applied
                    }
                    DispatchState::Unknown { .. } => Decision::Replayed,
                    _ => refused(Refusal::KnownOutcome),
                }
            }
            Event::Handoff { evidence } => self.handoff(evidence, false),
            Event::ReconcileHandoff { evidence } => self.handoff(evidence, true),
        }
    }
    fn handoff(&mut self, evidence: &HandoffEvidence, reconcile: bool) -> Decision {
        if self.association.as_ref() != Some(&evidence.association) {
            return refused(Refusal::Binding);
        }
        match &self.dispatch {
            DispatchState::Resolved { evidence: old } => {
                return if old.as_ref() == evidence {
                    Decision::Replayed
                } else {
                    refused(Refusal::Conflict)
                };
            }
            DispatchState::Unknown { .. } if !reconcile => {
                return refused(Refusal::NeedsReconciliation);
            }
            DispatchState::IntentCommitted if reconcile => {
                return refused(Refusal::NeedsReconciliation);
            }
            DispatchState::NotAttempted => return refused(Refusal::Phase),
            _ => {}
        }
        self.dispatch = DispatchState::Resolved {
            evidence: Box::new(evidence.clone()),
        };
        Decision::Applied
    }
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
        if self.plan.subject.actor_id() != plan.subject.actor_id()
            || self.plan.subject.conversation_id() != plan.subject.conversation_id()
            || self.plan.generation.checked_add(1) != Some(plan.generation)
            || self.plan.dispatch_id == plan.dispatch_id
            || self.plan.host_operation_id == plan.host_operation_id
            || self.plan.dispatch_id == plan.host_operation_id
            || self.plan.host_operation_id == plan.dispatch_id
            || self.plan.session == plan.session
        {
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
        validate_successor_history(&plan, self.plan.generation, history)?;
        let retryable = matches!(&self.dispatch, DispatchState::NotAttempted)
            || matches!(&self.dispatch, DispatchState::Resolved { evidence }
                if evidence.disposition == HandoffDisposition::NotHandedOff);
        for next in &plan.batch.messages {
            if let Some(old) = self
                .plan
                .batch
                .messages
                .iter()
                .find(|m| m.message_id == next.message_id || m.cursor == next.cursor)
            {
                if old != next {
                    return Err(Error::Refused(Refusal::Conflict));
                }
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
            }
        }
        Self::admitted(plan, clock)
    }
}
