//! Durable graph intent: authority admission, fixed public adapter, receipt/outbox.
use super::*;
mod authorize;
mod dispatch;
use axum::extract::Path;

// Fixed human actor path shares the installed Graph transport/identity, not agent authority.
pub(super) fn service_identity(rt: &Runtime, workspace: Uuid) -> Result<Value> {
    authorize::identity(rt, &json!(workspace))
}
pub(super) async fn post_human_creation(
    rt: &Runtime,
    workspace: Uuid,
    bytes: Vec<u8>,
) -> Result<(StatusCode, Value)> {
    dispatch::post_mutation_bytes(rt, workspace, bytes).await
}
const MAX_RETAINED_OPERATIONS: i64 = 4096;
const MAX_RETAINED_BYTES: i64 = 32 * 1024 * 1024;
fn admission_budget(count: i64, bytes: i64, additional: i64) -> Result<()> {
    if count >= MAX_RETAINED_OPERATIONS
        || bytes
            .checked_add(additional)
            .is_none_or(|total| total > MAX_RETAINED_BYTES)
    {
        Err(Error("overloaded"))
    } else {
        Ok(())
    }
}
pub(super) fn admin_routes() -> Router<AppState> {
    Router::new()
        .route("/api/agent-operations/{operation}/retry", post(owner_retry))
        .route(
            "/api/agent-operations/{operation}/cancel",
            post(owner_cancel),
        )
        .route("/api/agent-operations/{operation}", get(owner_get))
}
pub(super) fn machine_routes() -> Router<AppState> {
    Router::new()
        .route("/v1/agent/graph/commands", post(command))
        .route("/v1/agent/operations/{operation_id}", get(agent_get))
}
pub(super) fn node_routes() -> Router<AppState> {
    Router::new()
        .route("/v2/worker/agent-operations/query", post(node_query))
        .route("/v2/worker/agent-operations/retry", post(node_retry))
}
fn status_response(body: Value) -> Response {
    let status = match body["state"].as_str() {
        Some("confirmed") => StatusCode::OK,
        Some("conflict") => StatusCode::CONFLICT,
        Some("rejected") => StatusCode::UNPROCESSABLE_ENTITY,
        _ => StatusCode::ACCEPTED,
    };
    response(status, body)
}
fn bounded_status_response(p: &Principal, body: Value) -> Response {
    if p.bound(&body).is_ok() {
        return status_response(body);
    }
    // Intent/effect may already be durable. A response-size fault is never proof
    // that the graph mutation was not applied.
    response(
        StatusCode::PAYLOAD_TOO_LARGE,
        json!({"api_version":1,"request_id":Uuid::new_v4(),"operation_id":body["operation_id"],"code":"response_too_large","message":"operation response exceeds budget; inspect retained intent through recovery","effect":"unknown","retryable":false,"recovery":{"action":"manual_reconciliation","after_ms":0,"remaining_attempts":0,"remaining_elapsed_ms":0}}),
    )
}
fn common(request: &Value) -> Result<Value> {
    Ok(
        json!({"api_version":1,"operation_id":request["operation_id"],"binding":request["binding"],"payload_digest":digest(request)?,"admission":"recorded"}),
    )
}
fn pending(
    request: &Value,
    policy: &Value,
    possible: bool,
    used: u64,
    elapsed: u64,
    manual: bool,
) -> Result<Value> {
    let mut status = common(request)?;
    status["state"] = json!(if possible { "unknown" } else { "pending" });
    status["effect"] = json!(if possible {
        "possible"
    } else {
        "not_dispatched"
    });
    let attempts = policy["max_attempts"]
        .as_u64()
        .unwrap_or(0)
        .saturating_sub(used);
    let remaining = policy["max_elapsed_ms"]
        .as_u64()
        .unwrap_or(0)
        .saturating_sub(elapsed);
    let stop = manual || attempts == 0 || remaining == 0;
    status["recovery"] = json!({"action":if stop{"manual_reconciliation"}else if possible{"query_or_replay_identical"}else{"poll_existing"},"after_ms":if stop{0}else{policy["initial_delay_ms"].as_u64().unwrap_or(250).min(30000)},"remaining_attempts":attempts,"remaining_elapsed_ms":remaining});
    Ok(status)
}
async fn operation(state: &AppState, op: Uuid) -> Result<sqlx::postgres::PgRow> {
    sqlx::query("SELECT * FROM agent_graph_operations WHERE operation_id=$1")
        .bind(op)
        .fetch_optional(&state.store.pool)
        .await?
        .ok_or(Error("operation_unknown"))
}
fn owns(p: &Principal, row: &sqlx::postgres::PgRow) -> Result<()> {
    if row.get::<Uuid, _>("attempt_id") != id(&p.binding, "attempt_id")? {
        Err(Error("operation_unknown"))
    } else {
        Ok(())
    }
}
async fn command(
    State(state): State<AppState>,
    headers: HeaderMap,
    StrictJson(request): StrictJson<Value>,
) -> Result<Response> {
    if request["api_version"] != 1 {
        return Err(Error("unsupported_api_version"));
    }
    if !contract::valid("CommandRequest", &request) {
        return Err(Error("invalid_request"));
    }
    let p = authenticate(&state, &headers).await?;
    if p.binding != request["binding"] {
        return Err(Error("stale_generation"));
    }
    let op = id(&request, "operation_id")?;
    let hash = digest(&request)?;
    // Identical replay never refetches mutable graph state or changes its guards.
    let old = sqlx::query("SELECT * FROM agent_graph_operations WHERE operation_id=$1")
        .bind(op)
        .fetch_optional(&state.store.pool)
        .await?;
    if let Some(row) = old {
        owns(&p, &row)?;
        if row.get::<String, _>("payload_digest") != hash {
            return Err(Error("id_conflict"));
        }
        let current = authenticate(&state, &headers).await?;
        if current.version != p.version || current.revision != p.revision {
            return Err(Error("session_revoked"));
        }
        if headers.get("Prefer").is_some_and(|v| v == "respond-async") {
            return Ok(bounded_status_response(&p, row.get("status")));
        }
        return Ok(bounded_status_response(
            &p,
            dispatch::recoverable_run(&state, op, &request, &p.profile["retry_budget"]).await?,
        ));
    }
    let (mapped, creations) = tokio::time::timeout(
        Duration::from_secs(15),
        authorize::prepare(&state, &p, &request),
    )
    .await
    .map_err(|_| Error("overloaded"))??;
    let initial = pending(&request, &p.profile["retry_budget"], false, 0, 0, false)?;
    p.bound(&initial)?;
    let mut admitted_status = initial.clone();
    let mut tx = state.store.pool.begin().await?;
    let current = authenticate_locked(&state, &headers, &mut tx).await?;
    if current.binding != p.binding
        || current.version != p.version
        || current.revision != p.revision
    {
        return Err(Error("session_revoked"));
    }
    // Cross-attempt UUID reuse and same-attempt concurrent admission are fenced.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(format!("agent-operation:{op}"))
        .execute(&mut *tx)
        .await?;
    let existing = sqlx::query(
        "SELECT attempt_id,payload_digest,status FROM agent_graph_operations WHERE operation_id=$1",
    )
    .bind(op)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(row) = existing {
        if row.get::<Uuid, _>("attempt_id") != id(&p.binding, "attempt_id")?
            || row.get::<String, _>("payload_digest") != hash
        {
            return Err(Error("id_conflict"));
        }
        admitted_status = row.get("status");
    } else {
        let retained=sqlx::query("SELECT count(*)::bigint AS count,COALESCE(sum(retained_bytes),0)::bigint AS bytes FROM agent_graph_operations WHERE attempt_id=$1").bind(id(&p.binding,"attempt_id")?).fetch_one(&mut *tx).await?;
        let retained_bytes = (canonical(&request)?.len()
            + canonical(&mapped)?.len()
            + canonical(&p.profile["retry_budget"])?.len()
            + 32768) as i64;
        admission_budget(retained.get("count"), retained.get("bytes"), retained_bytes)?;
        let reserved:i64=sqlx::query_scalar("SELECT COALESCE(sum(reserved_creations),0)::bigint FROM agent_graph_operations WHERE attempt_id=$1").bind(id(&p.binding,"attempt_id")?).fetch_one(&mut *tx).await?;
        if reserved.saturating_add(creations as i64)
            > p.profile["write_scope"]["max_created_records"]
                .as_i64()
                .unwrap_or(0)
        {
            return Err(Error("scope_denied"));
        }
        sqlx::query("INSERT INTO agent_graph_operations(operation_id,attempt_id,node_id,request,payload_digest,mapped,policy,status,phase,created_at_ms,reserved_creations,retained_bytes) VALUES($1,$2,$3,$4,$5,$6,$7,$8,'queued',$9,$10,$11)").bind(op).bind(id(&p.binding,"attempt_id")?).bind(id(&p.binding,"node_id")?).bind(&request).bind(hash).bind(mapped).bind(&p.profile["retry_budget"]).bind(&initial).bind(now(&state) as i64).bind(creations as i32).bind(retained_bytes).execute(&mut *tx).await?;
    }
    if current.expires <= now(&state) {
        return Err(Error("session_expired"));
    }
    lock_attempt(
        &state,
        &mut tx,
        id(&p.binding, "attempt_id")?,
        id(&p.binding, "node_id")?,
    )
    .await?;
    tx.commit().await?;
    if headers.get("Prefer").is_some_and(|v| v == "respond-async") {
        return Ok(bounded_status_response(&p, admitted_status));
    }
    // No authority transaction or connection is held across HTTP.
    let body = dispatch::recoverable_run(&state, op, &request, &p.profile["retry_budget"]).await?;
    Ok(bounded_status_response(&p, body))
}
async fn agent_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(op): Path<Uuid>,
) -> Result<Response> {
    let mut tx = state.store.pool.begin().await?;
    let p = authenticate_locked(&state, &headers, &mut tx).await?;
    let row = sqlx::query("SELECT * FROM agent_graph_operations WHERE operation_id=$1")
        .bind(op)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error("operation_unknown"))?;
    owns(&p, &row)?;
    let body: Value = row.get("status");
    if p.expires <= now(&state) {
        return Err(Error("session_expired"));
    }
    tx.commit().await?;
    Ok(bounded_status_response(&p, body))
}
async fn owner_get(
    State(state): State<AppState>,
    RequireOwner(_): RequireOwner,
    Path(op): Path<Uuid>,
) -> Result<Response> {
    Ok(status_response(operation(&state, op).await?.get("status")))
}
async fn owner_retry(
    State(state): State<AppState>,
    RequireOwner(_): RequireOwner,
    Path(op): Path<Uuid>,
) -> Result<Response> {
    let row = operation(&state, op).await?;
    Ok(status_response(
        dispatch::recoverable_run(
            &state,
            op,
            &row.get::<Value, _>("request"),
            &row.get::<Value, _>("policy"),
        )
        .await?,
    ))
}
async fn owner_cancel(
    State(state): State<AppState>,
    RequireOwner(_): RequireOwner,
    Path(op): Path<Uuid>,
) -> Result<Response> {
    let mut tx = state.store.pool.begin().await?;
    let row = sqlx::query("SELECT * FROM agent_graph_operations WHERE operation_id=$1 FOR UPDATE")
        .bind(op)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error("operation_unknown"))?;
    let old: Value = row.get("status");
    if row.get::<String, _>("phase") == "queued" && row.get::<i32, _>("dispatch_count") == 0 {
        let request: Value = row.get("request");
        let mut body = common(&request)?;
        body["state"] = json!("rejected");
        body["effect"] = json!("not_applied");
        body["code"] = json!("session_revoked");
        body["retryable"] = json!(false);
        sqlx::query(
            "UPDATE agent_graph_operations SET phase='rejected',status=$2 WHERE operation_id=$1",
        )
        .bind(op)
        .bind(&body)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(status_response(body))
    } else {
        tx.commit().await?;
        Ok(status_response(old))
    }
}
async fn recover_rows(
    state: &AppState,
    headers: &HeaderMap,
    request: &Value,
) -> Result<(Vec<sqlx::postgres::PgRow>, Option<String>, bool)> {
    if !contract::valid("RecoveryRequest", request) {
        return Err(Error("invalid_request"));
    }
    version(headers)?;
    let node = crate::worker_api::authenticate(state, headers)
        .await
        .map_err(|_| Error("unauthorized"))?;
    if request["node_id"] != json!(node.id) {
        return Err(Error("unauthorized"));
    }
    let attempt = id(&request["attempt"], "attempt_id")?;
    let mut tx = state.store.pool.begin().await?;
    let active: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM nodes WHERE id=$1 AND revoked_at IS NULL AND credential_hash=$2 FOR UPDATE",
    )
    .bind(node.id)
    .bind(crate::tokens::verifier(bearer(headers)?))
    .fetch_optional(&mut *tx)
    .await?;
    if active.is_none() {
        return Err(Error("unauthorized"));
    }
    let mut after = 0i64;
    let mut upper:i64=sqlx::query_scalar("SELECT COALESCE(max(operation_seq),0)::bigint FROM agent_graph_operations WHERE node_id=$1 AND attempt_id=$2").bind(node.id).bind(attempt).fetch_one(&mut *tx).await?;
    let limit = request["limit"].as_u64().unwrap_or(64) as usize;
    if request["mode"] == "list" && !request["cursor"].is_null() {
        let token = request["cursor"].as_str().ok_or(Error("cursor_invalid"))?;
        let row=sqlx::query("SELECT after_sequence,upper_sequence,query_digest FROM agent_operation_cursors WHERE verifier=$1 AND node_id=$2 AND attempt_id=$3 AND expires_at_ms>$4").bind(crate::tokens::verifier(token)).bind(node.id).bind(attempt).bind(now(state) as i64).fetch_optional(&mut *tx).await?.ok_or(Error("cursor_invalid"))?;
        if row.get::<String, _>("query_digest")
            != digest(&json!({"node":node.id,"attempt":request["attempt"],"limit":limit}))?
        {
            return Err(Error("cursor_invalid"));
        }
        after = row.get("after_sequence");
        upper = row.get("upper_sequence");
    }
    let mut rows = if request["mode"] == "lookup" {
        let ids: Vec<Uuid> = request["operation_ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().parse().unwrap())
            .collect();
        let rows=sqlx::query("SELECT * FROM agent_graph_operations WHERE operation_id=ANY($1) AND node_id=$2 AND attempt_id=$3 ORDER BY operation_seq").bind(&ids).bind(node.id).bind(attempt).fetch_all(&mut *tx).await?;
        if rows.len() != ids.len() {
            return Err(Error("operation_unknown"));
        }
        rows
    } else {
        sqlx::query("SELECT * FROM agent_graph_operations WHERE node_id=$1 AND attempt_id=$2 AND operation_seq>$3 AND operation_seq<=$4 ORDER BY operation_seq LIMIT $5").bind(node.id).bind(attempt).bind(after).bind(upper).bind(limit as i64+1).fetch_all(&mut *tx).await?
    };
    for row in &rows {
        let req: Value = row.get("request");
        for field in ["attempt_id", "session_id", "generation"] {
            if req["binding"][field] != request["attempt"][field] {
                return Err(Error("operation_unknown"));
            }
        }
    }
    let more = rows.len() > limit;
    rows.truncate(limit);
    let next = if more {
        sqlx::query("DELETE FROM agent_operation_cursors WHERE expires_at_ms<=$1")
            .bind(now(state) as i64)
            .execute(&mut *tx)
            .await?;
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM agent_operation_cursors WHERE node_id=$1 AND attempt_id=$2",
        )
        .bind(node.id)
        .bind(attempt)
        .fetch_one(&mut *tx)
        .await?;
        if count >= 128 {
            return Err(Error("overloaded"));
        }
        let token = secret();
        sqlx::query("INSERT INTO agent_operation_cursors(verifier,node_id,attempt_id,query_digest,after_sequence,upper_sequence,expires_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7)").bind(crate::tokens::verifier(&token)).bind(node.id).bind(attempt).bind(digest(&json!({"node":node.id,"attempt":request["attempt"],"limit":limit}))?).bind(rows.last().unwrap().get::<i64,_>("operation_seq")).bind(upper).bind(now(state).saturating_add(300000) as i64).execute(&mut *tx).await?;
        Some(token)
    } else {
        None
    };
    tx.commit().await?;
    Ok((rows, next, more))
}
fn recovery_response(
    request: &Value,
    rows: Vec<Value>,
    next: Option<String>,
    more: bool,
) -> Result<Response> {
    let body = json!({"api_version":1,"request_id":request["request_id"],"attempt":request["attempt"],"operations":rows,"next_cursor":next,"coverage":{"status":if more{"partial"}else{"complete"},"complete_for":"authorized_projection","reasons":if more{vec!["node_budget"]}else{vec![]}}});
    if canonical(&body)?.len() > 1048576 {
        return Err(Error("response_too_large"));
    }
    Ok(ok(body))
}
async fn node_query(
    State(state): State<AppState>,
    headers: HeaderMap,
    StrictJson(request): StrictJson<Value>,
) -> Result<Response> {
    let (rows, next, more) = recover_rows(&state, &headers, &request).await?;
    recovery_response(
        &request,
        rows.into_iter().map(|r| r.get("status")).collect(),
        next,
        more,
    )
}
async fn node_retry(
    State(state): State<AppState>,
    headers: HeaderMap,
    StrictJson(request): StrictJson<Value>,
) -> Result<Response> {
    if request["mode"] != "lookup"
        || request["operation_ids"]
            .as_array()
            .is_none_or(|a| a.len() != 1)
    {
        return Err(Error("invalid_request"));
    }
    let (rows, _, _) = recover_rows(&state, &headers, &request).await?;
    let op = rows[0].get("operation_id");
    let body = dispatch::recoverable_run(
        &state,
        op,
        &rows[0].get::<Value, _>("request"),
        &rows[0].get::<Value, _>("policy"),
    )
    .await?;
    recovery_response(&request, vec![body], None, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retained_admission_count_and_byte_boundaries_are_exact() {
        assert!(admission_budget(4095, MAX_RETAINED_BYTES - 1, 1).is_ok());
        assert_eq!(admission_budget(4096, 0, 1).unwrap_err().0, "overloaded");
        assert_eq!(
            admission_budget(0, MAX_RETAINED_BYTES, 1).unwrap_err().0,
            "overloaded"
        );
        assert_eq!(
            admission_budget(0, i64::MAX, 1).unwrap_err().0,
            "overloaded"
        );
    }
    #[tokio::test]
    async fn oversized_postadmission_status_reports_unknown_with_existing_identity() {
        let f: Value = serde_json::from_str(include_str!(
            "../../../../protocol/agent-access/v1/fixtures/base.json"
        ))
        .unwrap();
        let c = &f["context"];
        let mut p = Principal {
            created: Vec::new(),
            binding: c["binding"].clone(),
            profile: c["profile"].clone(),
            context: c["context"].clone(),
            version: 1,
            revision: 1,
            expires: 1,
        };
        p.profile["max_response_bytes"] = json!(1024);
        let op = Uuid::new_v4();
        let ids: Vec<Uuid> = (0..64).map(|_| Uuid::new_v4()).collect();
        let versions: Vec<Value> = ids
            .iter()
            .map(|id| json!({"kind":"node","id":id,"version":"1","content_revision":"1"}))
            .collect();
        let body = json!({"api_version":1,"operation_id":op,"binding":p.binding,"payload_digest":"a".repeat(64),"state":"confirmed","admission":"recorded","effect":"confirmed","receipt":{"kind":"graph_mutation","workspace_id":p.binding["workspace_id"],"change_set_id":Uuid::new_v4(),"versions":versions,"affected_record_ids":ids,"task_completion":false}});
        assert!(contract::valid("OperationStatus", &body));
        let response = bounded_status_response(&p, body);
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let bytes = axum::body::to_bytes(response.into_body(), 1048576)
            .await
            .unwrap();
        let error: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(error["effect"], "unknown");
        assert_eq!(error["operation_id"], json!(op));
        assert!(contract::valid("ErrorResponse", &error));
        assert!(bytes.len() <= 1024);
    }
}
