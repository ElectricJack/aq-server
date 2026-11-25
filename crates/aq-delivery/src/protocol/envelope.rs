use super::*;

fn correlated(value: &Value, id: Uuid) -> bool {
    let id = id.to_string();
    value["protocol_version"] == 1
        && value["request_id"] == id
        && value
            .get("operation_id")
            .is_none_or(|operation| *operation == id)
}
fn one_outcome(value: &Value) -> bool {
    value["ok"].is_boolean() && (value.get("result").is_some() != value.get("error").is_some())
}
fn valid_success(value: &Value, operation: &str, state: Option<&OperationStatus>) -> bool {
    let mutating = matches!(operation, "candidate.create" | "publish");
    let complete = state.is_none_or(|s| *s == OperationStatus::Complete);
    let confirmed = !mutating || (complete && value.get("operation_id").is_some());
    let candidate = matches!(operation, "candidate.create" | "candidate.show");
    let result = &value["result"];
    let shape =
        !candidate || (result.get("candidate").is_some() && result.get("conflict_step").is_some());
    confirmed && shape
}
fn parse<T: DeserializeOwned>(value: &Value, key: &str) -> Result<T> {
    serde_json::from_value(value.get(key).ok_or(DeliveryError::Protocol)?.clone())
        .map_err(|_| DeliveryError::Protocol)
}
fn error(value: &Value) -> Result<GitError> {
    let error: GitError = parse(value, "error")?;
    if error.code.is_empty() {
        return Err(DeliveryError::Protocol);
    }
    Ok(error)
}
pub(crate) fn decode<T: WireResult>(
    raw: Vec<u8>,
    context: &OperationContext,
    operation: &str,
    params: &Value,
) -> Result<Receipt<T>> {
    let value: Value = serde_json::from_slice(&raw).map_err(|_| DeliveryError::Protocol)?;
    if !correlated(&value, context.operation_id) || !one_outcome(&value) {
        return Err(DeliveryError::Protocol);
    }
    let operation_state: Option<OperationStatus> = value
        .get("operation_state")
        .map(|state| serde_json::from_value(state.clone()))
        .transpose()
        .map_err(|_| DeliveryError::Protocol)?;
    let outcome = if value["ok"] == true {
        if !valid_success(&value, operation, operation_state.as_ref()) {
            return Err(DeliveryError::Protocol);
        }
        let result: T = parse(&value, "result")?;
        if !result.validate(context, operation, params) {
            return Err(DeliveryError::Protocol);
        }
        Ok(result)
    } else {
        Err(error(&value)?)
    };
    Ok(Receipt {
        raw,
        operation_state,
        outcome,
    })
}
