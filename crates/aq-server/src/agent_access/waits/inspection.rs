use super::*;
/// Agent inspection of its own task's wait (same task lineage, any attempt).
pub async fn inspect(
    state: &AppState,
    node: Uuid,
    attempt: Uuid,
    request: &wire::Request,
) -> Result<wire::Response> {
    let mut tx = state.store.pool.begin().await?;
    let caller = load_caller(state, &mut tx, node, attempt).await?;
    if let Err(error) = request.validate() {
        return Ok(refused(request, contract_refusal(&error)));
    }
    let wire::Command::Inspect(inspect) = &request.body.command else {
        return Ok(refused(request, wire::RefusalCode::InvalidRequest));
    };
    let wire::InspectTarget::Wait { wait } = &inspect.target else {
        return Ok(refused(request, wire::RefusalCode::UnsupportedCapability));
    };
    if &inspect.scope != caller.scope() {
        return Ok(refused(request, wire::RefusalCode::ScopeDenied));
    }
    let Some(stored) = load_stored(&mut tx, wait.wait_id, false).await? else {
        return Ok(refused(request, wire::RefusalCode::ScopeDenied));
    };
    let registration = stored.snapshot.registration();
    if !same_lineage(&registration.binding, caller.binding()) {
        return Ok(refused(request, wire::RefusalCode::ScopeDenied));
    }
    if &registration.wait_ref() != wait {
        return Ok(refused(request, wire::RefusalCode::StaleGeneration));
    }
    let page = wire::InspectionPage {
        scope: caller.scope().clone(),
        items: vec![wire::InspectionItem::Wait(Box::new(
            stored.snapshot.inspection(
                &stored.registered,
                stored.intent_state.as_deref(),
                stored.ack_receipt.as_ref(),
            )?,
        ))],
        next_cursor: None,
        complete_for_authorized_scope: true,
        reasons: Vec::new(),
    };
    tx.commit().await?;
    Ok(respond(request, wire::ResponseBody::Inspection(page)))
}

/// Owner (G10) list: newest first, optionally one project or one attempt.
pub async fn owner_list(
    state: &AppState,
    project: Option<Uuid>,
    attempt: Option<Uuid>,
) -> Result<Value> {
    let items: Vec<Value> = sqlx::query_scalar("SELECT jsonb_build_object('wait_id',w.wait_id,'project_id',w.project_id,'workspace_id',w.workspace_id,'task_id',w.task_id,'attempt_id',w.attempt_id,'phase',w.phase,'revision',w.revision,'next_due_ms',w.next_due_ms,'deadline_ms',w.deadline_ms,'created_at_ms',w.created_at_ms,'outcome_kind',o.kind,'intent_state',i.state) FROM agent_waits w LEFT JOIN agent_wait_outcomes o USING(wait_id) LEFT JOIN agent_wait_resume_intents i USING(wait_id) WHERE ($1::uuid IS NULL OR w.project_id=$1) AND ($2::uuid IS NULL OR w.attempt_id=$2) ORDER BY w.created_at_ms DESC,w.wait_id LIMIT 200")
        .bind(project)
        .bind(attempt)
        .fetch_all(&state.store.pool)
        .await?;
    Ok(json!({ "items": items }))
}
/// Owner (G10) detail, including per-member freshness and hold reason.
/// `attempt` limits it to that attempt's wait (another is `None`, as missing).
pub async fn owner_show(
    state: &AppState,
    wait_id: Uuid,
    attempt: Option<Uuid>,
) -> Result<Option<Value>> {
    let mut tx = state.store.pool.begin().await?;
    let owner: Option<Uuid> =
        sqlx::query_scalar("SELECT attempt_id FROM agent_waits WHERE wait_id=$1")
            .bind(wait_id)
            .fetch_optional(&mut *tx)
            .await?;
    if attempt.is_some_and(|a| owner != Some(a)) {
        return Ok(None);
    }
    let Some(stored) = load_stored(&mut tx, wait_id, false).await? else {
        return Ok(None);
    };
    tx.commit().await?;
    Ok(Some(json!({
        "view": stored.snapshot.inspection(&stored.registered,stored.intent_state.as_deref(),stored.ack_receipt.as_ref())?,
        "phase": stored.snapshot.phase(),
        "revision": stored.snapshot.revision(),
        "deadline_ms": stored.snapshot.deadline_ms(),
        "next_due_ms": stored.snapshot.next_due_ms(),
        "hold_reason": stored.snapshot.hold_reason(),
        "members": stored.snapshot.member_states(),
        "intent_state": stored.intent_state,
    })))
}
