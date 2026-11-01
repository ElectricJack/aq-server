use super::*;
pub(super) fn receipt(op: &Operation, raw: &Value) -> Result<Receipt> {
    if !contract::upstream_valid("MutationResult", raw)
        || super::super::super::canonical(raw)
            .map_err(|_| unavailable())?
            .len()
            > op.payload.len() + 8192
    {
        return Err(unavailable());
    }
    let payload: Value = serde_json::from_slice(&op.payload).map_err(|_| unavailable())?;
    let expected = &payload["operations"][0];
    let rows = raw["results"].as_array().ok_or_else(unavailable)?;
    if rows.len() != 1 {
        return Err(unavailable());
    }
    let node = &rows[0];
    if !contract::upstream_valid("Node", node)
        || node["id"] != json!(op.id)
        || node["workspace_id"] != json!(op.workspace)
        || node["kind"] != "task"
        || node["created_by"] != op.service["actor"]
    {
        return Err(unavailable());
    }
    for key in [
        "title",
        "body",
        "acceptance_criteria",
        "labels",
        "task_state",
        "metadata",
    ] {
        if node[key] != expected[key] {
            return Err(unavailable());
        }
    }
    if node["content_type"] != "note"
        || node["verification"] != "unverified"
        || node["deleted"] != false
        || node["retired"] != false
        || node["provenance"] != json!([])
    {
        return Err(unavailable());
    }
    for key in [
        "summary",
        "summary_of_revision",
        "last_verified_by",
        "last_verified_at",
        "retirement_reason",
        "valid_from",
        "valid_until",
        "review_at",
        "successor_id",
    ] {
        if !node[key].is_null() {
            return Err(unavailable());
        }
    }
    for key in ["created_at", "updated_at"] {
        if node[key].as_str().is_none_or(|s| s.len() > 64) {
            return Err(unavailable());
        }
    }
    let uuid = |v: &Value| {
        v.as_str()
            .and_then(|s| s.parse::<Uuid>().ok())
            .filter(|id| !id.is_nil())
            .ok_or_else(unavailable)
    };
    let revision = |k: &str| {
        node[k]
            .as_i64()
            .filter(|n| *n > 0)
            .map(|n| n.to_string())
            .ok_or_else(unavailable)
    };
    Ok(Receipt {
        workspace_id: op.workspace,
        change_set_id: uuid(&raw["change_set_id"])?,
        task_id: op.id,
        version: revision("version")?,
        content_revision: revision("content_revision")?,
    })
}
pub(super) async fn run(
    state: &AppState,
    r: &ReadRuntime,
    actor: &User,
    id: Uuid,
) -> Result<Status> {
    let original = journal::load(r, id)
        .await?
        .ok_or_else(|| AppError::NotFound("operation not found".into()))?;
    journal::owns(&original, actor)?;
    let mut tx = journal::transaction(r).await?;
    let blocked = journal::guard(state, &mut tx, actor, &original).await?;
    if original.status.terminal() || blocked.is_some() {
        tx.commit().await.map_err(db)?;
        return journal::status(state, r, actor, &original).await;
    }
    let op = Operation::decode(
        sqlx::query("SELECT * FROM human_graph_operations WHERE operation_id=$1 FOR UPDATE")
            .bind(id)
            .fetch_one(&mut *tx)
            .await
            .map_err(db)?,
    )?;
    if op.status.terminal()
        || op.lease_until.is_some_and(|n| n > stamp(state))
        || op.count >= MAX_ATTEMPTS
        || op.stop.is_some()
    {
        tx.commit().await.map_err(db)?;
        return journal::status(state, r, actor, &op).await;
    }
    let now = stamp(state);
    if now < op.created || now >= op.expires {
        tx.commit().await.map_err(db)?;
        return journal::status(state, r, actor, &op).await;
    }
    let lease = Uuid::new_v4();
    let mut claimed = op.status.clone();
    claimed.state = OperationState::Unknown;
    claimed.effect = Effect::Possible;
    claimed.recovery = Recovery {
        allowed: false,
        reason: Some("dispatch_in_progress".into()),
    };
    sqlx::query("UPDATE human_graph_operations SET status=$2,dispatch_count=dispatch_count+1,lease_id=$3,lease_expires_at_ms=$4 WHERE operation_id=$1").bind(id).bind(serde_json::to_value(&claimed).map_err(|_|unavailable())?).bind(lease).bind(now.checked_add(6000).ok_or_else(unavailable)?).execute(&mut *tx).await.map_err(db)?;
    tx.commit().await.map_err(db)?;
    // Exact retained bytes only; no authority transaction crosses HTTP.
    let outcome = tokio::time::timeout(
        Duration::from_secs(3),
        super::super::super::mutations::post_human_creation(
            runtime(state)?,
            op.workspace,
            op.payload.clone(),
        ),
    )
    .await;
    let mut next = claimed;
    next.recovery = Recovery {
        allowed: true,
        reason: None,
    };
    let mut raw_receipt = None;
    let mut stop = None;
    if let Ok(Ok((code, raw))) = outcome {
        if code.is_success() {
            if let Ok(proof) = receipt(&op, &raw) {
                next.state = OperationState::Confirmed;
                next.effect = Effect::Confirmed;
                next.receipt = Some(proof);
                next.recovery.allowed = false;
                raw_receipt = Some(raw)
            }
        } else if code == StatusCode::CONFLICT && raw["error"]["code"] == "conflict" {
            if op.count == 0 {
                next.state = OperationState::Conflict;
                next.effect = Effect::NotApplied;
                next.recovery.allowed = false
            } else {
                stop = Some("graph_conflict_after_possible_effect".to_string());
                next.recovery = Recovery {
                    allowed: false,
                    reason: stop.clone(),
                }
            }
        } else if matches!(code.as_u16(), 400 | 401 | 403 | 404 | 422)
            && raw["error"]["code"].is_string()
            && op.count == 0
        {
            next.state = OperationState::Rejected;
            next.effect = Effect::NotApplied;
            next.recovery.allowed = false
        }
    }
    let mut tx = journal::transaction(r).await?;
    let current = Operation::decode(
        sqlx::query("SELECT * FROM human_graph_operations WHERE operation_id=$1 FOR UPDATE")
            .bind(id)
            .fetch_one(&mut *tx)
            .await
            .map_err(db)?,
    )?;
    if current.lease == Some(lease) {
        sqlx::query("UPDATE human_graph_operations SET status=$2,raw_receipt=$3,lease_id=NULL,lease_expires_at_ms=NULL,stop_reason=$4 WHERE operation_id=$1").bind(id).bind(serde_json::to_value(&next).map_err(|_|unavailable())?).bind(raw_receipt).bind(stop).execute(&mut *tx).await.map_err(db)?;
    }
    tx.commit().await.map_err(db)?;
    let saved = journal::load(r, id).await?.ok_or_else(unavailable)?;
    journal::status(state, r, actor, &saved).await
}
