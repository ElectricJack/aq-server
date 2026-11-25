use super::*;

impl Run {
    pub(super) fn apply(&mut self, event: &Event) -> Decision {
        match event {
            Event::Tick => Decision::Applied,
            Event::CommitStart { launch, dispatch } => self.commit_start(launch, dispatch),
            Event::Renew { permit } => self.renew(permit),
            Event::Running { observation } => self.running(observation),
            Event::Cancel { reason } => self.cancel(*reason),
            Event::StopUncertain { observation } => self.stop_uncertain(observation),
            Event::HostLost {
                binding,
                launch_id,
                report,
            } => self.host_lost(binding, *launch_id, report),
            Event::Stopped { evidence } => self.stopped(evidence),
            Event::ReportUnknown { association } => self.report_unknown(association),
            Event::Handoff { evidence } => self.handoff(evidence, false),
            Event::ReconcileHandoff { evidence } => self.handoff(evidence, true),
        }
    }
    fn commit_start(&mut self, launch: &HostLaunchRef, dispatch: &DispatchAssociation) -> Decision {
        if self.replays_start(launch, dispatch) {
            return Decision::Replayed;
        }
        if self.phase != Phase::Prepared {
            return refused(Refusal::Phase);
        }
        if self.start_binding_differs(launch, dispatch) {
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
    fn replays_start(&self, launch: &HostLaunchRef, dispatch: &DispatchAssociation) -> bool {
        self.active()
            && self.launch.as_ref() == Some(launch)
            && self.association.as_ref() == Some(dispatch)
    }
    fn start_binding_differs(
        &self,
        launch: &HostLaunchRef,
        dispatch: &DispatchAssociation,
    ) -> bool {
        launch.binding != self.plan.binding().expect("validated")
            || self.plan.dispatch(dispatch.permit.clone()).ok().as_ref() != Some(dispatch)
            || dispatch.permit.binding != launch.binding
    }
    fn renewal_deadline_invalid(&self, old: &PermitData, permit: &PermitData) -> bool {
        !self.permit_fits(permit)
            || permit.issued_boot_ms < old.issued_boot_ms
            || permit.expires_boot_ms <= old.expires_boot_ms
    }
    fn renew(&mut self, permit: &PermitData) -> Decision {
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
        if self.renewal_deadline_invalid(old, permit) {
            return refused(Refusal::Deadline);
        }
        self.permit = Some(permit.clone());
        Decision::Applied
    }
    fn record_observation(&mut self, observation: &HostObservationRef) {
        self.guardian = Some(observation.guardian.clone());
        self.host_observation = Some(observation.clone());
    }
    fn running(&mut self, observation: &HostObservationRef) -> Decision {
        if !self.host_matches(observation) {
            return refused(Refusal::Binding);
        }
        if self.host_observation.as_ref() == Some(observation) {
            return Decision::Replayed;
        }
        self.record_observation(observation);
        if self.phase == Phase::StartCommitted {
            self.phase = Phase::RunningReported;
        }
        Decision::Applied
    }
    fn cancel(&mut self, reason: CancelReason) -> Decision {
        if matches!(
            self.phase,
            Phase::StoppedReported | Phase::CancelledBeforeStart
        ) {
            return Decision::Replayed;
        }
        if self.phase == Phase::Prepared {
            self.phase = Phase::CancelledBeforeStart;
            self.cancellation = Some(reason);
        } else {
            self.stopping(reason, UncertaintyReason::Cancellation);
        }
        Decision::Applied
    }
    fn stop_uncertain(&mut self, observation: &HostObservationRef) -> Decision {
        if self.phase == Phase::StoppedReported {
            return refused(Refusal::KnownOutcome);
        }
        if !self.host_matches(observation) {
            return refused(Refusal::Binding);
        }
        self.record_observation(observation);
        self.uncertain_report
            .get_or_insert(observation.observation.clone());
        self.cancellation.get_or_insert(CancelReason::HostLost);
        self.unknown(UncertaintyReason::HostLost);
        self.phase = Phase::StopUncertain;
        Decision::Applied
    }
    fn host_lost(
        &mut self,
        binding: &CentralBinding,
        launch_id: Uuid,
        report: &ArtifactRef,
    ) -> Decision {
        if self.phase == Phase::StoppedReported {
            return refused(Refusal::KnownOutcome);
        }
        if !self
            .launch
            .as_ref()
            .is_some_and(|l| l.binding == *binding && l.launch_id == launch_id)
        {
            return refused(Refusal::Binding);
        }
        self.cancellation.get_or_insert(CancelReason::HostLost);
        self.uncertain_report.get_or_insert(report.clone());
        self.unknown(UncertaintyReason::HostLost);
        self.phase = Phase::StopUncertain;
        Decision::Applied
    }
    fn stopped(&mut self, evidence: &StopEvidenceRef) -> Decision {
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
        self.record_observation(&evidence.host);
        self.stop = Some(evidence.clone());
        self.unknown(UncertaintyReason::HostLost);
        self.phase = Phase::StoppedReported;
        Decision::Applied
    }
    fn report_unknown(&mut self, association: &DispatchAssociation) -> Decision {
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
}
