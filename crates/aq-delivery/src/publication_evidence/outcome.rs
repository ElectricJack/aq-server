use super::*;
use crate::DeliveryOutcome;
pub enum Recorded {
    Published { outcome: Value, head: String },
    Unpublished(Value),
    Indeterminate(Value),
}

pub enum PublicationStatus {
    Recorded(Recorded),
    DestinationBlocked,
    RecoveryBlocked(String),
    Failed(String),
}
impl PublicationStatus {
    pub fn from_outcome(
        candidate_oid: &ObjectId,
        outcome: DeliveryOutcome,
    ) -> Result<Self, serde_json::Error> {
        let recorded = match outcome {
            DeliveryOutcome::Published {
                old,
                new,
                acknowledged_at,
                ..
            } => Recorded::Published {
                outcome: json!({"outcome":"applied","old":old,"new":new,"acknowledged_at":acknowledged_at}),
                head: new.hex,
            },
            // Observed presence of exactly the candidate. A target that merely
            // contains it is a different head AQ cannot follow: held.
            DeliveryOutcome::ObservedPresent {
                observed_target,
                observed_at,
                ..
            } if &observed_target == candidate_oid => Recorded::Published {
                outcome: json!({"outcome":"already_present","observed_target":observed_target,"observed_at":observed_at,"caused_by_this_operation":false}),
                head: candidate_oid.hex.clone(),
            },
            DeliveryOutcome::ObservedPresent {
                observed_target,
                observed_at,
                ..
            } => Recorded::Indeterminate(
                json!({"reason":"observed_target_differs","outcome":"already_present","observed_target":observed_target,"observed_at":observed_at,"caused_by_this_operation":false}),
            ),
            DeliveryOutcome::Refused(refusal) => {
                Recorded::Unpublished(serde_json::to_value(refusal)?)
            }
            DeliveryOutcome::Ambiguous { operation_id } => Recorded::Indeterminate(
                json!({"reason":"operation_indeterminate","operation_id":operation_id,"next":"operation.reconcile under a new operation ID; never a re-push"}),
            ),
            DeliveryOutcome::DestinationBlocked => return Ok(Self::DestinationBlocked),
            DeliveryOutcome::RecoveryBlocked(blocked) => {
                return Ok(Self::RecoveryBlocked(blocked.message));
            }
            DeliveryOutcome::Failed { error, .. } => return Ok(Self::Failed(error.code)),
        };
        Ok(Self::Recorded(recorded))
    }
}
