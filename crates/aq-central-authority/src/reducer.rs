use super::*;
mod events;
mod successor;
mod validation;
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
    pub fn stopped_evidence(&self) -> Option<&StopEvidenceRef> {
        self.stop.as_ref()
    }
    pub fn cancellation(&self) -> Option<CancelReason> {
        self.cancellation
    }
    fn active(&self) -> bool {
        matches!(self.phase, Phase::StartCommitted | Phase::RunningReported)
    }
    fn permit_fits(&self, p: &PermitData) -> bool {
        p.binding == self.plan.binding().expect("validated plan")
            && self.permit_within_run(p)
            && p.expires_boot_ms > self.last_clock.boot_ms
    }
    fn permit_within_run(&self, p: &PermitData) -> bool {
        p.machine_boot_id == self.admitted.machine_boot_id
            && (self.admitted.boot_ms..=self.last_clock.boot_ms).contains(&p.issued_boot_ms)
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
}
