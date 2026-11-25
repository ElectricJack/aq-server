use super::*;

impl PublishRequest {
    pub(super) fn refused(
        &self,
        reason: RefusalReason,
        detail: &str,
        observed: (Option<ObjectId>, Option<String>),
    ) -> DeliveryOutcome {
        DeliveryOutcome::Refused(PublicationRefusal {
            operation_id: self.context.operation_id,
            reason,
            detail: detail.into(),
            target: self.target.clone(),
            expected_old: Some(self.expected_old.clone()),
            observed_target: observed.0,
            observed_at: observed.1,
        })
    }
    pub(super) fn outcome(&self, receipt: Receipt<PublishResult>) -> DeliveryOutcome {
        let operation_id = self.context.operation_id;
        match receipt.outcome {
            Ok(PublishResult::Applied {
                old,
                new,
                acknowledged_at,
                ..
            }) => DeliveryOutcome::Published {
                operation_id,
                old,
                new,
                acknowledged_at,
            },
            Ok(PublishResult::AlreadyPresent {
                observed_target,
                observed_at,
                ..
            }) => DeliveryOutcome::ObservedPresent {
                operation_id,
                observed_target,
                observed_at,
            },
            Ok(PublishResult::Rejected {
                reason,
                observed_target,
                observed_at,
            }) => {
                let kind = if reason == "target_moved" {
                    RefusalReason::TargetMoved
                } else {
                    RefusalReason::Rejected
                };
                self.refused(kind, &reason, (observed_target, observed_at))
            }
            Err(e) if e.code == "recovery_blocked" => {
                DeliveryOutcome::RecoveryBlocked(RecoveryBlocked {
                    operation_id,
                    message: e.message,
                    details: e.details,
                })
            }
            Err(e) if e.uncertain() => DeliveryOutcome::Ambiguous { operation_id },
            Err(e) => match e.code.as_str() {
                "target_moved" => {
                    self.refused(RefusalReason::TargetMoved, &e.message, (None, None))
                }
                "conflicted" => self.refused(RefusalReason::Conflicted, &e.message, (None, None)),
                "unsupported_feature" => {
                    self.refused(RefusalReason::TargetNotAllowed, &e.message, (None, None))
                }
                _ => DeliveryOutcome::Failed {
                    operation_id,
                    error: e,
                },
            },
        }
    }
}
