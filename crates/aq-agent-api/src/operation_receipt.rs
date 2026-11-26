//! Pure validation of a terminal graph-operation row retained by the dispatch owner.
//! It reads the row; it does not authenticate, dispatch or fabricate an upstream receipt.
use crate::{InvalidContract, digest, valid};
use serde_json::Value;

/// Whether the retained operation row is final (`Ok(true)`) or still in flight (`Ok(false)`).
/// A row whose projection, owner binding, idempotency key, lease or receipt does not agree
/// is `Err`, never "final". The dispatch owner already verified the upstream receipt; a
/// supplied receipt is not regarded as proof here.
pub fn settled(row: &Value, binding: &Value) -> Result<bool, InvalidContract> {
    let request = &row["request"];
    let status = &row["status"];
    if !valid("CommandRequest", request) || !valid("OperationStatus", status) {
        return Err(InvalidContract);
    }
    if !matches_operation(request, row, binding) || !matches_operation(status, row, binding) {
        return Err(InvalidContract);
    }
    if row["payload_digest"] != digest(request)?
        || status["payload_digest"] != row["payload_digest"]
    {
        return Err(InvalidContract);
    }
    validate_dispatch(row, binding)?;
    match row["phase"].as_str() {
        Some("queued" | "dispatching" | "unknown") => Ok(false),
        Some(phase @ ("confirmed" | "conflict" | "rejected")) => {
            validate_final(row, binding, phase)?;
            Ok(true)
        }
        _ => Err(InvalidContract),
    }
}

fn matches_operation(projection: &Value, row: &Value, binding: &Value) -> bool {
    projection["binding"] == *binding && projection["operation_id"] == row["operation_id"]
}

fn validate_dispatch(row: &Value, binding: &Value) -> Result<(), InvalidContract> {
    let mapping = &row["mapped"];
    let operation_id = row["operation_id"].as_str().ok_or(InvalidContract)?;
    let key = format!("aq-agent-v1:{operation_id}");
    if mapping["service"]["workspace_id"] != binding["workspace_id"]
        || mapping["payload"]["idempotency_key"] != key
    {
        return Err(InvalidContract);
    }
    if row["dispatch_count"].as_u64().is_none_or(|count| count > 8) {
        return Err(InvalidContract);
    }
    Ok(())
}

fn validate_final(row: &Value, binding: &Value, phase: &str) -> Result<(), InvalidContract> {
    let status = &row["status"];
    if status["state"] != phase {
        return Err(InvalidContract);
    }
    // owner_cancel can finalize a never-dispatched queued row without a lease.
    // A stale lease on any final row is deliberately unavailable.
    if !row["lease_id"].is_null() || !row["lease_expires_at_ms"].is_null() {
        return Err(InvalidContract);
    }
    if phase == "confirmed" {
        return validate_confirmation(row, binding);
    }
    if status["effect"] != "not_applied" {
        return Err(InvalidContract);
    }
    Ok(())
}

fn validate_confirmation(row: &Value, binding: &Value) -> Result<(), InvalidContract> {
    let status = &row["status"];
    if status["effect"] != "confirmed" || row["dispatch_count"].as_u64() == Some(0) {
        return Err(InvalidContract);
    }
    let receipt = &status["receipt"];
    if receipt["workspace_id"] != binding["workspace_id"] || receipt["task_completion"] != false {
        return Err(InvalidContract);
    }
    if !matches!(
        receipt["kind"].as_str(),
        Some("graph_mutation" | "graph_draft")
    ) {
        return Err(InvalidContract);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn row(state: &str) -> (Value, Value) {
        let fixture: Value =
            serde_json::from_str(include_str!("../artifacts/fixtures/base.json")).unwrap();
        let request = fixture["command_requests"]["correct_record"].clone();
        let binding = request["binding"].clone();
        let status = fixture["operation_statuses"][state].clone();
        assert!(valid("CommandRequest", &request));
        assert!(valid("OperationStatus", &status));
        let phase = if state == "pending" { "queued" } else { state };
        let row = json!({"operation_id":request["operation_id"],"request":request,"payload_digest":digest(&request).unwrap(),"status":status,"phase":phase,"dispatch_count":1,"lease_id":null,"lease_expires_at_ms":null,"mapped":{"service":{"workspace_id":binding["workspace_id"]},"payload":{"idempotency_key":format!("aq-agent-v1:{}",request["operation_id"].as_str().unwrap())}}});
        (row, binding)
    }
    #[test]
    fn terminal_owner_rows_settle_but_pending_and_unknown_never_do() {
        for state in ["confirmed", "conflict", "rejected"] {
            let (r, b) = row(state);
            assert!(settled(&r, &b).unwrap(), "{state}");
        }
        for state in ["pending", "unknown"] {
            let (r, b) = row(state);
            assert!(!settled(&r, &b).unwrap(), "{state}");
        }
    }
    #[test]
    fn final_phase_alone_cannot_hide_possible_effect_or_changed_owner() {
        let (original, b) = row("confirmed");
        for path in [
            "/status/payload_digest",
            "/payload_digest",
            "/request/binding/context_digest",
        ] {
            let mut r = original.clone();
            *r.pointer_mut(path).unwrap() = json!("a".repeat(64));
            assert!(settled(&r, &b).is_err(), "{path}");
        }
        let mut r = original.clone();
        r["lease_id"] = json!(uuid::Uuid::from_u128(0xbeef));
        assert!(settled(&r, &b).is_err());
        let mut r = original.clone();
        r["dispatch_count"] = json!(0);
        assert!(settled(&r, &b).is_err());
        let mut r = original.clone();
        r["status"]["receipt"]["workspace_id"] = json!(uuid::Uuid::from_u128(0xbeef));
        assert!(settled(&r, &b).is_err());
        let (mut r, b) = row("unknown");
        r["phase"] = json!("confirmed");
        assert!(settled(&r, &b).is_err());
    }
}
