//! Fixed test-only classifications. No message, credential or body is retained.
use crate::error::AppError;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictClass {
    SelectionExpired,
    NativePreparationUnavailable,
    PhasePreparationUnavailable,
    PhaseExecutionAuthorityUnavailable,
    PhaseExecutionInputsChanged,
    OriginalOfferUnavailable,
    SourceObservationUnavailable,
    CapabilitySnapshotChanged,
    AvailabilityChanged,
    CredentialAuthorizationChanged,
    ReservationOwnerChanged,
    OtherConflict,
}

/// Exact literals only. An unknown or dynamic message becomes one fixed value.
pub(crate) fn classify(error: &AppError) -> Option<ConflictClass> {
    let AppError::Conflict(message) = error else {
        return None;
    };
    Some(match message.as_str() {
        "selection expired during admission locks" => ConflictClass::SelectionExpired,
        "native preparation is not currently available" => {
            ConflictClass::NativePreparationUnavailable
        }
        "accepted preparation v2 is not currently available" => {
            ConflictClass::PhasePreparationUnavailable
        }
        "phase execution authority is not currently available" => {
            ConflictClass::PhaseExecutionAuthorityUnavailable
        }
        "pinned phase execution inputs changed" => ConflictClass::PhaseExecutionInputsChanged,
        "original native offer unavailable or inconsistent" => {
            ConflictClass::OriginalOfferUnavailable
        }
        "configured source observation unavailable or inconsistent" => {
            ConflictClass::SourceObservationUnavailable
        }
        "installed capability snapshot changed" => ConflictClass::CapabilitySnapshotChanged,
        "relevant availability changed" => ConflictClass::AvailabilityChanged,
        "credential authorization changed" => ConflictClass::CredentialAuthorizationChanged,
        "not the exact owned ungranted reservation"
        | "reservation decision mismatch"
        | "native reservation missing"
        | "invalid native reservation"
        | "reservation has issued authority" => ConflictClass::ReservationOwnerChanged,
        _ => ConflictClass::OtherConflict,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn arbitrary_messages_never_enter_classification() {
        let raw = "synthetic-private-message-never-log";
        let classification = classify(&AppError::Conflict(raw.into())).unwrap();
        assert_eq!(classification, ConflictClass::OtherConflict);
        assert!(
            !serde_json::to_string(&classification)
                .unwrap()
                .contains(raw)
        );
        assert_eq!(
            classify(&AppError::Conflict(
                "selection expired during admission locks".into()
            )),
            Some(ConflictClass::SelectionExpired)
        );
        assert!(classify(&AppError::Internal).is_none());
        assert_eq!(
            classify(&AppError::Conflict(
                "pinned phase execution inputs changed".into()
            )),
            Some(ConflictClass::PhaseExecutionInputsChanged)
        );
    }
}
