//! Delivery evidence for the project task work view.
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

pub fn completion_wait(graph: Option<Value>, delivery: Option<Value>) -> Option<Value> {
    if delivery
        .as_ref()
        .is_some_and(|wait| wait["code"] == "delivery_settlement_grant_required")
    {
        delivery
    } else {
        graph.or(delivery)
    }
}

/// Show the current delivery decision or the policy that can decide an open result.
pub async fn waiting(
    pool: &PgPool,
    namespace: Option<(Uuid, i64)>,
    h: &crate::Hold,
    at_ms: i64,
) -> Option<Value> {
    use aq_playbook_domain::delivery_store as store;
    let mut db = pool.acquire().await.ok()?;
    if let Some(namespace) = namespace
        && let Ok(Some((result, operation, command, grant_current))) =
            store::settlement_wait(&mut db, namespace, h.attempt_id, at_ms).await
    {
        let (code, owner, trigger, message) = if grant_current {
            (
                "delivery_settlement_pending",
                "aq_delivery",
                "delivery_reconciliation",
                format!(
                    "AQ delivery will recheck the admitted {command} decision for this result."
                ),
            )
        } else {
            (
                "delivery_settlement_grant_required",
                "delivery_policy_owner",
                "scoped_delivery_authorization",
                format!(
                    "The delivery policy owner must restore authority for the admitted {command} decision."
                ),
            )
        };
        return Some(
            json!({"code": code, "attempt_id": h.attempt_id, "result_id": result,
            "operation_id": operation, "blocker_owner": owner, "wake_condition": trigger, "message": message}),
        );
    }
    if let Ok(Some((result, retained, quiescent))) = store::rework_wait(&mut db, h.attempt_id).await
    {
        return Some(rework(h.attempt_id, result, retained, quiescent));
    }
    let (result, hold) = store::result_wait(&mut db, h.attempt_id).await.ok()??;
    if let Some(reason) = hold {
        let evidence = store::held_pr_evidence(&mut db, result)
            .await
            .ok()
            .flatten();
        return Some(crate::delivery_hold(
            h,
            result,
            &reason,
            evidence.as_deref(),
        ));
    }
    let active = match namespace {
        Some(namespace) => store::active(&mut db, namespace).await.ok()?,
        None => false,
    };
    let mut waiting = crate::result_waiting(h.attempt_id, result, active);
    let detail = match store::checking_pr(&mut db, result).await {
        Ok(Some(check)) => Some(check),
        _ => store::publication_refusal(&mut db, result)
            .await
            .ok()
            .flatten(),
    };
    if let Some(detail) = detail
        && let Some(Value::String(message)) = waiting.get_mut("message")
    {
        message.push(' ');
        message.push_str(&detail);
    }
    Some(waiting)
}

fn rework(attempt: Uuid, result: Uuid, retained: bool, quiescent: bool) -> Value {
    let missing: Vec<_> = [
        ("retained_source", retained),
        ("quiescent_effects", quiescent),
    ]
    .into_iter()
    .filter_map(|(name, present)| (!present).then_some(name))
    .collect();
    let (code, owner, wake, message) = if missing.is_empty() {
        (
            "replacement_facts_ready",
            "aq_scheduler",
            "worker_poll",
            "AQ has the stopped attempt's retained source and quiescent effects. It reassesses retry limits and placement on worker polls.",
        )
    } else {
        (
            "replacement_facts_pending",
            "aq_settlement",
            "replacement_evidence_reconciliation",
            "AQ settlement will recheck the missing evidence for this returned result.",
        )
    };
    json!({"code": code, "attempt_id": attempt, "result_id": result, "missing_evidence": missing,
        "blocker_owner": owner, "wake_condition": wake, "message": message})
}

/// An optional read of the recorded result, restricted to the same claim as
/// the budget hold. Readiness stays available if the detail cannot be read.
pub(super) async fn budget_evidence(
    pool: &PgPool,
    project: Uuid,
    task: Uuid,
    attempt: Uuid,
) -> Option<String> {
    let mut db = pool.acquire().await.ok()?;
    aq_playbook_domain::delivery_store::returned_result_evidence(&mut db, project, task, attempt)
        .await
        .ok()?
}
