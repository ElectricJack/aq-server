use crate::*;
record!(PageRequest {limit:u32,cursor:Option<Text>});
impl Validate for PageRequest {
    fn validate(&self) -> Result<(), ContractError> {
        ensure(
            self.limit > 0 && self.limit <= MAX_PAGE as u32,
            "page bound",
        )?;
        if let Some(cursor) = &self.cursor {
            cursor.validate()?;
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum InspectTarget {
    Session { binding: Binding },
    Wait { wait: WaitRef },
    Notification { message_id: Uuid },
}
record!(InspectRequest {
    scope: Scope,
    target: InspectTarget,
    page: PageRequest
});
impl Validate for InspectRequest {
    fn validate(&self) -> Result<(), ContractError> {
        self.scope.validate()?;
        self.page.validate()?;
        match &self.target {
            InspectTarget::Session { binding } => {
                binding.validate()?;
                ensure(
                    binding.project_id == self.scope.project_id,
                    "inspection project",
                )
            }
            InspectTarget::Wait { wait } => wait.validate(),
            InspectTarget::Notification { message_id } => uuid(*message_id),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AuthorityView {
    Unknown,
    CurrentReported { expires_at_ms: u64 },
    Expired,
    Revoked,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProcessObservation {
    Unknown,
    ReportedRunning,
    ReportedStopped,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AccountingCompleteness {
    Unknown,
    Incomplete { reason: Text },
    CompleteForPolicy { policy: ArtifactRef },
}
record!(UsageView {ledger_revision:ArtifactRef,counters:Counters,counter_generation:Option<u64>,costs:Vec<Cost>,completeness:AccountingCompleteness});
impl Validate for UsageView {
    fn validate(&self) -> Result<(), ContractError> {
        artifact(&self.ledger_revision)?;
        ensure(
            self.counter_generation.is_none_or(|v| v > 0),
            "usage generation",
        )?;
        ensure(self.costs.len() <= 8, "cost projection bound")?;
        for cost in &self.costs {
            cost.validate()?;
        }
        match &self.completeness {
            AccountingCompleteness::Unknown => Ok(()),
            AccountingCompleteness::Incomplete { reason } => reason.validate(),
            AccountingCompleteness::CompleteForPolicy { policy } => artifact(policy),
        }
    }
}
record!(SessionView {binding:Binding,authority:AuthorityView,process:ProcessObservation,last_observation:Option<EventRef>,observed_at_ms:Option<u64>,usage:UsageView,active_wait:Option<WaitRef>,pending_operation_ids:Vec<Uuid>});
impl Validate for SessionView {
    fn validate(&self) -> Result<(), ContractError> {
        self.binding.validate()?;
        self.usage.validate()?;
        if let Some(event) = &self.last_observation {
            event.validate()?;
        }
        if let Some(wait) = &self.active_wait {
            wait.validate()?;
        }
        unique_ids(&self.pending_operation_ids, MAX_MEMBERS)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContinuationState {
    PendingPolicy,
    Held {
        reason: Text,
    },
    WakeAdmitted {
        admission_id: Uuid,
        successor: Binding,
        offer_digest: Digest,
    },
    BootstrapAcknowledged {
        receipt: ArtifactRef,
    },
    Cancelled,
}
record!(WaitView {registration:RegisteredWait,outcome:Option<WaitOutcome>,continuation:ContinuationState,parking_report:Option<ArtifactRef>});
impl Validate for WaitView {
    fn validate(&self) -> Result<(), ContractError> {
        self.registration.validate()?;
        if let Some(outcome) = &self.outcome {
            outcome.validate_for(&self.registration.predicate)?;
            ensure(
                outcome.outcome.wait == self.registration.wait,
                "wait view outcome binding",
            )?;
        }
        match &self.continuation {
            ContinuationState::Held { reason } => reason.validate()?,
            ContinuationState::WakeAdmitted {
                admission_id,
                successor,
                ..
            } => {
                uuid(*admission_id)?;
                successor.validate()?;
                ensure(
                    successor.task_id == self.registration.binding.task_id
                        && successor.project_id == self.registration.scope.project_id
                        && successor.lineage_id == self.registration.binding.lineage_id
                        && successor.attempt_id != self.registration.binding.attempt_id
                        && successor.session_id() != self.registration.binding.session_id(),
                    "wake admission scope/fresh attempt",
                )?;
            }
            ContinuationState::BootstrapAcknowledged { receipt } => artifact(receipt)?,
            _ => {}
        }
        if let Some(r) = &self.parking_report {
            artifact(r)?;
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DeliveryState {
    NoExternalRoute,
    Queued,
    Dispatching,
    AcceptedByTransport {
        receipt: ArtifactRef,
    },
    DeliveryUnknown {
        since_ms: u64,
    },
    RetryScheduled {
        next_attempt_ms: u64,
        attempts_remaining: u32,
    },
    PermanentlyFailed {
        reason: Text,
    },
    Suppressed {
        policy: ArtifactRef,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HumanAcknowledgment {
    Unacknowledged,
    Acknowledged {
        authenticated_actor: ArtifactRef,
        receipt: ArtifactRef,
        revision: u64,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DecisionAnswer {
    Approve,
    Reject,
    Cancel,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DecisionView {
    NotApplicable,
    Pending {
        decision_id: Uuid,
        subject_revision: Text,
        question_digest: Digest,
    },
    Answered {
        decision_id: Uuid,
        subject_revision: Text,
        question_digest: Digest,
        answer: DecisionAnswer,
        authenticated_receipt: ArtifactRef,
    },
}
record!(MessageRevision {
    message_id: Uuid,
    revision: u64
});
impl Validate for MessageRevision {
    fn validate(&self) -> Result<(), ContractError> {
        uuid(self.message_id)?;
        ensure(self.revision > 0, "message revision")
    }
}
record!(DigestWindow {occurrence_id:Uuid,schedule_id:Uuid,window_start_ms:u64,window_end_ms:u64,timezone:Text,winning_logical_revision:Option<MessageRevision>});
impl Validate for DigestWindow {
    fn validate(&self) -> Result<(), ContractError> {
        uuid(self.occurrence_id)?;
        uuid(self.schedule_id)?;
        ensure(self.window_end_ms > self.window_start_ms, "digest window")?;
        self.timezone.validate()?;
        if let Some(winner) = &self.winning_logical_revision {
            winner.validate()?;
        }
        Ok(())
    }
}
record!(NotificationView {scope:Scope,message:MessageRevision,route_revision:Option<ArtifactRef>,delivery:DeliveryState,human_acknowledgment:HumanAcknowledgment,decision:DecisionView,digest_window:Option<DigestWindow>});
impl Validate for NotificationView {
    fn validate(&self) -> Result<(), ContractError> {
        self.scope.validate()?;
        self.message.validate()?;
        if let Some(route) = &self.route_revision {
            artifact(route)?;
        }
        match &self.delivery {
            DeliveryState::NoExternalRoute => {
                ensure(self.route_revision.is_none(), "unconfigured route")?
            }
            DeliveryState::AcceptedByTransport { receipt } => {
                ensure(
                    self.route_revision.is_some(),
                    "transport acceptance needs configured route",
                )?;
                artifact(receipt)?;
            }
            DeliveryState::PermanentlyFailed { reason } => reason.validate()?,
            DeliveryState::Suppressed { policy } => artifact(policy)?,
            _ => ensure(self.route_revision.is_some(), "delivery route revision")?,
        }
        if let HumanAcknowledgment::Acknowledged {
            authenticated_actor,
            receipt,
            revision,
        } = &self.human_acknowledgment
        {
            artifact(authenticated_actor)?;
            artifact(receipt)?;
            ensure(*revision > 0, "human acknowledgment revision")?;
        }
        match &self.decision {
            DecisionView::NotApplicable => {}
            DecisionView::Pending {
                decision_id,
                subject_revision,
                ..
            } => {
                uuid(*decision_id)?;
                subject_revision.validate()?;
            }
            DecisionView::Answered {
                decision_id,
                subject_revision,
                authenticated_receipt,
                ..
            } => {
                uuid(*decision_id)?;
                subject_revision.validate()?;
                artifact(authenticated_receipt)?;
            }
        }
        if let Some(window) = &self.digest_window {
            window.validate()?;
            if let Some(winner) = &window.winning_logical_revision {
                ensure(winner == &self.message, "one logical digest winner")?;
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    content = "body",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum InspectionItem {
    Session(Box<SessionView>),
    Wait(Box<WaitView>),
    Notification(Box<NotificationView>),
}
impl Validate for InspectionItem {
    fn validate(&self) -> Result<(), ContractError> {
        match self {
            Self::Session(v) => v.validate(),
            Self::Wait(v) => v.validate(),
            Self::Notification(v) => v.validate(),
        }
    }
}
impl InspectionItem {
    fn validate_scope(&self, scope: &Scope) -> Result<(), ContractError> {
        let project = match self {
            InspectionItem::Session(v) => v.binding.project_id,
            InspectionItem::Wait(v) => v.registration.scope.project_id,
            InspectionItem::Notification(v) => v.scope.project_id,
        };
        ensure(project == scope.project_id, "inspection item project")?;
        match self {
            InspectionItem::Wait(v) => {
                ensure(v.registration.scope == *scope, "wait inspection workspace")?
            }
            InspectionItem::Notification(v) => {
                ensure(v.scope == *scope, "notification inspection workspace")?
            }
            _ => {}
        }
        Ok(())
    }
}
record!(InspectionPage {scope:Scope,items:Vec<InspectionItem>,next_cursor:Option<Text>,complete_for_authorized_scope:bool,reasons:Vec<Text>});
impl Validate for InspectionPage {
    fn validate(&self) -> Result<(), ContractError> {
        self.scope.validate()?;
        ensure(
            self.items.len() <= MAX_PAGE && self.reasons.len() <= MAX_MEMBERS,
            "inspection bounds",
        )?;
        for item in &self.items {
            item.validate()?;
            item.validate_scope(&self.scope)?;
        }
        if let Some(cursor) = &self.next_cursor {
            cursor.validate()?;
        }
        for reason in &self.reasons {
            reason.validate()?;
        }
        ensure(
            !self.complete_for_authorized_scope
                || (self.next_cursor.is_none() && self.reasons.is_empty()),
            "inspection coverage",
        )
    }
}
