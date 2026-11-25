use super::*;

fn timestamp(value: &str) -> bool {
    chrono::DateTime::parse_from_rfc3339(value).is_ok()
}
fn requested_oid(oid: &ObjectId, expected: &Value, allow_zero: bool) -> bool {
    crate::refs::oid(oid, allow_zero).is_ok()
        && serde_json::to_value(oid).ok().as_ref() == Some(expected)
}
fn observed_present(oid: &ObjectId, at: &str, caused: bool) -> bool {
    !caused && crate::refs::oid(oid, false).is_ok() && timestamp(at)
}
impl WireResult for PublishResult {
    fn validate(&self, _: &OperationContext, _: &str, params: &Value) -> bool {
        match self {
            Self::Applied {
                target,
                old,
                new,
                acknowledged_at,
            } => {
                params["target"] == *target
                    && requested_oid(old, &params["expected_old"], true)
                    && requested_oid(new, &params["candidate"], false)
                    && timestamp(acknowledged_at)
            }
            Self::AlreadyPresent {
                observed_target,
                observed_at,
                caused_by_this_operation,
            } => observed_present(observed_target, observed_at, *caused_by_this_operation),
            Self::Rejected {
                reason,
                observed_target,
                observed_at,
            } => {
                !reason.is_empty()
                    && observed_target
                        .as_ref()
                        .is_none_or(|o| o.validate().is_ok())
                    && observed_at.as_ref().is_none_or(|t| timestamp(t))
            }
        }
    }
}
impl WireResult for ReconcileResult {
    fn validate(&self, _: &OperationContext, operation: &str, params: &Value) -> bool {
        if params["target_operation_id"] != self.target_operation_id.to_string()
            || self
                .observed_target
                .as_ref()
                .is_some_and(|o| o.validate().is_err())
        {
            return false;
        }
        match (&self.resolution, self.outcome.as_deref()) {
            (None, Some("indeterminate")) => operation == "operation.reconcile",
            (
                Some(Resolution::AlreadyPresent {
                    observed_target,
                    observed_at,
                    caused_by_this_operation,
                }),
                None,
            ) => {
                operation == "operation.reconcile"
                    && observed_present(observed_target, observed_at, *caused_by_this_operation)
            }
            (
                Some(Resolution::Resolved {
                    disposition,
                    reason,
                    resolved_at,
                    ..
                }),
                None,
            ) => {
                operation == "operation.resolve"
                    && params["disposition"] == *disposition
                    && params["reason"] == *reason
                    && timestamp(resolved_at)
            }
            _ => false,
        }
    }
}
