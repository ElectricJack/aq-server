//! Durable scoped mail. No terminal, node delivery or human-notification effects.
use super::*;
use axum::routing::post;
pub mod global;
pub(crate) mod session_action;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Recipient {
    Task {
        workspace_id: Uuid,
        project_id: Uuid,
        task_id: Uuid,
    },
    Session {
        workspace_id: Uuid,
        project_id: Uuid,
        task_id: Uuid,
        session_id: Uuid,
        attempt_id: Uuid,
        generation: u64,
    },
    /// bold-stone-51: the singleton supervisor conversation mailbox. Only an
    /// agent session may address it here; owners use the conversation API.
    GlobalSupervisor { conversation_id: Uuid },
}
impl Recipient {
    /// Task scope of a task or session mailbox. The supervisor mailbox has none.
    fn task_scope(&self) -> Option<(Uuid, Uuid, Uuid)> {
        match *self {
            Self::Task {
                workspace_id,
                project_id,
                task_id,
            }
            | Self::Session {
                workspace_id,
                project_id,
                task_id,
                ..
            } => Some((workspace_id, project_id, task_id)),
            Self::GlobalSupervisor { .. } => None,
        }
    }
    fn scope(&self) -> Result<(Uuid, Uuid, Uuid)> {
        self.task_scope().ok_or(Error("not_found"))
    }
    pub(in crate::agent_access) fn key(&self) -> Result<String> {
        digest(&serde_json::to_value(self)?)
    }
    fn validate(&self) -> Result<()> {
        if let Self::GlobalSupervisor { conversation_id } = self {
            return if conversation_id.is_nil() {
                Err(Error("invalid_request"))
            } else {
                Ok(())
            };
        }
        let (w, p, t) = self.scope()?;
        if [w, p, t].iter().any(Uuid::is_nil) {
            return Err(Error("invalid_request"));
        }
        if let Self::Session {
            session_id,
            attempt_id,
            generation,
            ..
        } = self
            && (session_id.is_nil()
                || attempt_id.is_nil()
                || *generation == 0
                || *generation > i64::MAX as u64)
        {
            return Err(Error("invalid_request"));
        }
        Ok(())
    }
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Send {
    pub(crate) operation_id: Uuid,
    pub(crate) message_id: Uuid,
    pub(crate) thread_id: Uuid,
    pub(crate) recipient: Recipient,
    pub(crate) body: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Inbox {
    recipient: Recipient,
    /// Exclusive mailbox cursor. Thread filtering does not change its meaning.
    after: String,
    thread_id: Option<Uuid>,
    limit: u32,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Ack {
    operation_id: Uuid,
    recipient: Recipient,
    message_id: Uuid,
    disposition: Disposition,
}
#[derive(Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Disposition {
    Received,
    Read,
}
impl Disposition {
    fn name(&self) -> &'static str {
        match self {
            Self::Received => "received",
            Self::Read => "read",
        }
    }
}
pub(super) fn admin_routes() -> Router<AppState> {
    Router::new()
        .route("/api/messages/send", post(owner_send))
        .route("/api/messages/inbox", post(owner_inbox))
}
pub(super) fn machine_routes() -> Router<AppState> {
    Router::new()
        .route("/v1/agent/messages/send", post(agent_send))
        .route("/v1/agent/messages/inbox", post(agent_inbox))
        .route("/v1/agent/messages/ack", post(agent_ack))
        .route("/v1/agent/messages/body", post(agent_body))
}
fn capability(p: &Principal, name: &str) -> Result<()> {
    if p.profile["capabilities"]
        .as_array()
        .is_some_and(|c| c.contains(&json!(name)))
    {
        Ok(())
    } else {
        Err(Error("unsupported_capability"))
    }
}
fn sender(p: &Principal) -> Value {
    json!({"kind":"session","binding":p.binding})
}
fn sender_key(sender: &Value) -> Result<String> {
    digest(sender)
}
fn scope(p: &Principal, r: &Recipient, own: bool) -> Result<()> {
    r.validate()?;
    // Any bound session may write to the supervisor; none may read its mailbox.
    if let Recipient::GlobalSupervisor { .. } = r {
        return if own { Err(Error("not_found")) } else { Ok(()) };
    }
    let (w, project, task) = r.scope()?;
    if p.binding["workspace_id"] != json!(w)
        || p.binding["project_id"] != json!(project)
        || !p.readable(&json!(task))
    {
        return Err(Error("not_found"));
    }
    if own && p.binding["task_id"] != json!(task) {
        return Err(Error("not_found"));
    }
    if own
        && let Recipient::Session {
            session_id,
            attempt_id,
            generation,
            ..
        } = r
        && (p.binding["session_id"] != json!(session_id)
            || p.binding["attempt_id"] != json!(attempt_id)
            || p.binding["generation"] != json!(generation))
    {
        return Err(Error("stale_generation"));
    }
    Ok(())
}
async fn validate_session(tx: &mut Transaction<'_, Postgres>, r: &Recipient) -> Result<()> {
    if let Recipient::Session {
        workspace_id,
        project_id,
        task_id,
        session_id,
        attempt_id,
        generation,
    } = r
    {
        let b: Option<Value> =
            sqlx::query_scalar("SELECT binding FROM agent_launch_bindings WHERE attempt_id=$1")
                .bind(attempt_id)
                .fetch_optional(&mut **tx)
                .await?;
        let b = b.ok_or(Error("not_found"))?;
        for (key, value) in [
            ("workspace_id", json!(workspace_id)),
            ("project_id", json!(project_id)),
            ("task_id", json!(task_id)),
            ("session_id", json!(session_id)),
            ("generation", json!(generation)),
        ] {
            if b[key] != value {
                return Err(Error("not_found"));
            }
        }
    }
    Ok(())
}
// Serialize each actor's operation IDs even across daemon processes. This also
// prevents a changed-kind replay (send versus acknowledgement) from taking effect.
async fn replay(
    tx: &mut Transaction<'_, Postgres>,
    sender: &str,
    op: Uuid,
    payload: &str,
) -> Result<Option<Value>> {
    if op.is_nil() {
        return Err(Error("invalid_request"));
    }
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(sender)
        .execute(&mut **tx)
        .await?;
    let old=sqlx::query("SELECT payload_digest,response FROM mailbox_operations WHERE sender_key=$1 AND operation_id=$2").bind(sender).bind(op).fetch_optional(&mut **tx).await?;
    match old {
        Some(row) if row.get::<String, _>("payload_digest") == payload => {
            Ok(Some(row.get("response")))
        }
        Some(_) => Err(Error("id_conflict")),
        None => Ok(None),
    }
}
async fn receipt(
    tx: &mut Transaction<'_, Postgres>,
    sender: &str,
    op: Uuid,
    payload: &str,
    response: &Value,
) -> Result<()> {
    sqlx::query("INSERT INTO mailbox_operations(sender_key,operation_id,payload_digest,response) VALUES($1,$2,$3,$4)").bind(sender).bind(op).bind(payload).bind(response).execute(&mut **tx).await?;
    Ok(())
}
async fn send(
    tx: &mut Transaction<'_, Postgres>,
    request: Send,
    sender: Value,
    at: u64,
) -> Result<Value> {
    request.recipient.validate()?;
    if request.message_id.is_nil()
        || request.thread_id.is_nil()
        || request.body.is_empty()
        || request.body.len() > 16384
    {
        return Err(Error("invalid_request"));
    }
    let supervisor = match request.recipient {
        // Owners write through the conversation API under its own receipt contract.
        Recipient::GlobalSupervisor { .. } if sender["kind"] != "session" => {
            return Err(Error("not_found"));
        }
        Recipient::GlobalSupervisor { conversation_id } if request.thread_id != conversation_id => {
            return Err(Error("invalid_request"));
        }
        Recipient::GlobalSupervisor { conversation_id } => Some(conversation_id),
        _ => None,
    };
    let payload = digest(&json!({"kind":"send","request":request,"sender":sender}))?;
    let actor = sender_key(&sender)?;
    if let Some(r) = replay(tx, &actor, request.operation_id, &payload).await? {
        return Ok(r);
    }
    if let Some(conversation) = supervisor {
        admit_supervisor(tx, conversation, request.body.len()).await?;
    } else {
        validate_session(tx, &request.recipient).await?;
        let (workspace, project, task) = request.recipient.scope()?;
        let known: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM agent_launch_bindings WHERE binding->>'workspace_id'=$1 AND binding->>'project_id'=$2 AND binding->>'task_id'=$3)").bind(workspace.to_string()).bind(project.to_string()).bind(task.to_string()).fetch_one(&mut **tx).await?;
        if !known {
            return Err(Error("not_found"));
        }
    }
    let cursor = append(
        tx,
        Envelope {
            message_id: request.message_id,
            thread_id: request.thread_id,
            recipient: serde_json::to_value(&request.recipient)?,
            mailbox_key: request.recipient.key()?,
            body: &request.body,
        },
        &sender,
        &payload,
        at,
    )
    .await?;
    let response = json!({"api_version":1,"operation_id":request.operation_id,"message_id":request.message_id,"recipient":request.recipient,"thread_id":request.thread_id,"cursor":cursor.to_string(),"disposition":"queued","node_delivery":"not_attempted","harness_delivery":"not_attempted","notification_delivery":"not_attempted"});
    receipt(tx, &actor, request.operation_id, &payload, &response).await?;
    Ok(response)
}
/// Lock the singleton conversation, require the current one, and reserve its
/// retained capacity exactly as an owner admission does.
async fn admit_supervisor(
    tx: &mut Transaction<'_, Postgres>,
    conversation: Uuid,
    body: usize,
) -> Result<()> {
    let current: Option<(Uuid, i64)> = sqlx::query_as(
        "SELECT conversation_id,revision FROM global_supervisor_conversation WHERE singleton FOR UPDATE",
    )
    .fetch_optional(&mut **tx)
    .await?;
    if current != Some((conversation, 1)) {
        return Err(Error("not_found"));
    }
    let reserved = i64::try_from(body + 4096).map_err(|_| Error("invalid_request"))?;
    let row: Option<Uuid> = sqlx::query_scalar("UPDATE global_supervisor_conversation SET retained_operations=retained_operations+1,retained_bytes=retained_bytes+$1 WHERE singleton AND retained_operations<$2 AND retained_bytes<=$3-$1 RETURNING conversation_id")
        .bind(reserved).bind(global::MAX_OPERATIONS).bind(global::MAX_BYTES).fetch_optional(&mut **tx).await?;
    row.map(|_| ()).ok_or(Error("overloaded"))
}
// The caller must first authorize its exact recipient domain. This primitive
// owns the one mailbox order/body boundary; it has no public request surface.
struct Envelope<'a> {
    message_id: Uuid,
    thread_id: Uuid,
    recipient: Value,
    mailbox_key: String,
    body: &'a str,
}
async fn append(
    tx: &mut Transaction<'_, Postgres>,
    mail: Envelope<'_>,
    sender: &Value,
    payload: &str,
    at: u64,
) -> Result<i64> {
    let key = &mail.mailbox_key;
    sqlx::query("INSERT INTO mailbox_heads(mailbox_key) VALUES($1) ON CONFLICT DO NOTHING")
        .bind(key)
        .execute(&mut **tx)
        .await?;
    let cursor: i64 =
        sqlx::query_scalar("SELECT next_cursor FROM mailbox_heads WHERE mailbox_key=$1 FOR UPDATE")
            .bind(key)
            .fetch_one(&mut **tx)
            .await?;
    let inserted = sqlx::query("INSERT INTO mailbox_messages(message_id,mailbox_key,cursor,thread_id,sender,recipient,payload_digest,body,created_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9) ON CONFLICT(message_id) DO NOTHING")
        .bind(mail.message_id).bind(key).bind(cursor).bind(mail.thread_id).bind(sender).bind(mail.recipient).bind(payload).bind(mail.body).bind(at as i64)
        .execute(&mut **tx).await?.rows_affected();
    if inserted == 0 {
        return Err(Error("id_conflict"));
    }
    sqlx::query("UPDATE mailbox_heads SET next_cursor=next_cursor+1 WHERE mailbox_key=$1")
        .bind(key)
        .execute(&mut **tx)
        .await?;
    Ok(cursor)
}
/// Crate-internal service seam for an already scoped, derived owner principal.
/// The caller's transaction owns the current grant and domain outbox boundary.
/// Recheck the user role under lock; no workflow-supplied sender is accepted.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OwnerSendError {
    Invalid,
    NotFound,
    Conflict,
    Unknown,
}
pub(crate) async fn owner_send_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    request: Send,
    owner_id: Uuid,
    at: u64,
) -> std::result::Result<Value, OwnerSendError> {
    let role: Option<String> = sqlx::query_scalar("SELECT role FROM users WHERE id=$1 FOR SHARE")
        .bind(owner_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(|_| OwnerSendError::Unknown)?;
    if role.as_deref() != Some("owner") {
        return Err(OwnerSendError::NotFound);
    }
    send(tx, request, json!({"kind":"owner","user_id":owner_id}), at)
        .await
        .map_err(send_error)
}
fn send_error(error: Error) -> OwnerSendError {
    match error.0 {
        "invalid_request" => OwnerSendError::Invalid,
        "not_found" | "stale_generation" => OwnerSendError::NotFound,
        "id_conflict" => OwnerSendError::Conflict,
        _ => OwnerSendError::Unknown,
    }
}
/// H3 stall nudges from AQ itself. The sender is the fixed system component,
/// never a caller-supplied principal; the ladder action ID is the operation ID.
pub(crate) async fn system_send_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    request: Send,
    at: u64,
) -> std::result::Result<Value, OwnerSendError> {
    send(
        tx,
        request,
        json!({"kind":"system","component":"stall_ladder"}),
        at,
    )
    .await
    .map_err(send_error)
}
/// F5 owner answers for an asking attempt that already ended. Sent by the fixed
/// system component to the task mailbox; the owner's answer operation is the
/// mailbox operation ID.
pub(crate) async fn question_send_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    request: Send,
    at: u64,
) -> std::result::Result<Value, OwnerSendError> {
    send(
        tx,
        request,
        json!({"kind":"system","component":"question_answer"}),
        at,
    )
    .await
    .map_err(send_error)
}
/// Mail the node may hand to the harness now, oldest first. H3: a stall nudge
/// stored inbox-only, or any stall nudge while the attempt's terminal input is
/// unknown or a human controller holds the pane, stays readable in the inbox
/// but is never injected.
async fn injectable(
    tx: &mut Transaction<'_, Postgres>,
    attempt: Uuid,
    task_key: &str,
    session_key: &str,
    limit: i64,
) -> Result<Vec<sqlx::postgres::PgRow>> {
    let sql = format!(
        "SELECT m.*,d.delivery_id,d.disposition FROM mailbox_messages m LEFT JOIN mailbox_deliveries d ON d.message_id=m.message_id AND d.attempt_id=$1 WHERE m.mailbox_key IN ($2,$3) AND (d.disposition IS NULL OR d.disposition IN ('pending','received_by_node')) AND NOT EXISTS(SELECT 1 FROM mailbox_receipts a WHERE a.message_id=m.message_id) AND NOT EXISTS(SELECT 1 FROM mailbox_deliveries previous WHERE previous.message_id=m.message_id AND (previous.disposition IN ('handed_to_harness','delivery_unknown') OR (previous.attempt_id<>$1 AND previous.disposition IN ('pending','received_by_node')))) AND NOT EXISTS(SELECT 1 FROM stall_nudges n WHERE n.message_id=m.message_id AND (n.inbox_only OR {} OR {})) ORDER BY m.created_at_ms,m.mailbox_key,m.cursor LIMIT $4",
        crate::stall::ports::HUMAN_CONTROL_SQL,
        crate::stall::ports::INPUT_UNKNOWN_SQL
    );
    Ok(sqlx::query(&sql)
        .bind(attempt)
        .bind(task_key)
        .bind(session_key)
        .bind(limit)
        .fetch_all(&mut **tx)
        .await?)
}
/// Message IDs `node_poll` would hand this attempt's harness now.
pub(crate) async fn injectable_ids(
    tx: &mut Transaction<'_, Postgres>,
    attempt: Uuid,
    task: &Recipient,
    session: &Recipient,
    limit: i64,
) -> std::result::Result<Vec<Uuid>, OwnerSendError> {
    let keys = task.key().and_then(|t| Ok((t, session.key()?)));
    let (task, session) = keys.map_err(|_| OwnerSendError::Invalid)?;
    let rows = injectable(tx, attempt, &task, &session, limit)
        .await
        .map_err(|_| OwnerSendError::Unknown)?;
    Ok(rows.iter().map(|r| r.get("message_id")).collect())
}
async fn owner_send(
    State(state): State<AppState>,
    RequireOwner(user): RequireOwner,
    StrictJson(r): StrictJson<Send>,
) -> Result<Response> {
    let mut tx = state.store.pool.begin().await?;
    let response = send(
        &mut tx,
        r,
        json!({"kind":"owner","user_id":user.id}),
        now(&state),
    )
    .await?;
    tx.commit().await?;
    Ok(ok(response))
}
async fn agent_send(
    State(state): State<AppState>,
    headers: HeaderMap,
    StrictJson(r): StrictJson<Send>,
) -> Result<Response> {
    let mut tx = state.store.pool.begin().await?;
    let p = authenticate_locked(&state, &headers, &mut tx).await?;
    capability(&p, "message.send")?;
    scope(&p, &r.recipient, false)?;
    let response = send(&mut tx, r, sender(&p), now(&state)).await?;
    p.bound(&response)?;
    if p.expires <= now(&state) {
        return Err(Error("session_expired"));
    }
    tx.commit().await?;
    Ok(ok(response))
}
async fn inbox(
    tx: &mut Transaction<'_, Postgres>,
    request: Inbox,
    attempt: Option<Uuid>,
) -> Result<Value> {
    request.recipient.validate()?;
    // The supervisor mailbox is read only through the conversation API.
    request.recipient.scope()?;
    let after: i64 = request
        .after
        .parse()
        .map_err(|_| Error("invalid_request"))?;
    if after < 0
        || after.to_string() != request.after
        || !(1..=32).contains(&request.limit)
        || request.thread_id.is_some_and(|t| t.is_nil())
    {
        return Err(Error("invalid_request"));
    }
    let key = request.recipient.key()?;
    let rows=sqlx::query("SELECT m.*,r.disposition,d.attempt_id AS delivery_attempt,d.disposition AS delivery_disposition FROM mailbox_messages m LEFT JOIN LATERAL(SELECT disposition FROM mailbox_receipts WHERE message_id=m.message_id AND ($5::uuid IS NULL OR attempt_id=$5) ORDER BY acknowledged_at_ms DESC,attempt_id LIMIT 1) r ON TRUE LEFT JOIN LATERAL(SELECT attempt_id,disposition FROM mailbox_deliveries WHERE message_id=m.message_id AND ($5::uuid IS NULL OR attempt_id=$5) ORDER BY updated_at_ms DESC,attempt_id LIMIT 1) d ON TRUE WHERE m.mailbox_key=$1 AND m.cursor>$2 AND ($3::uuid IS NULL OR m.thread_id=$3) ORDER BY m.cursor LIMIT $4").bind(key).bind(after).bind(request.thread_id).bind(request.limit as i64+1).bind(attempt).fetch_all(&mut **tx).await?;
    let has_more = rows.len() > request.limit as usize;
    let mut last = after;
    let messages:Vec<Value>=rows.into_iter().take(request.limit as usize).map(|row| {
        last=row.get("cursor"); json!({"message_id":row.get::<Uuid,_>("message_id"),"thread_id":row.get::<Uuid,_>("thread_id"),"cursor":last.to_string(),"sender":row.get::<Value,_>("sender"),"recipient":row.get::<Value,_>("recipient"),"body":row.get::<String,_>("body"),"payload_digest":row.get::<String,_>("payload_digest"),"created_at_ms":row.get::<i64,_>("created_at_ms"),"disposition":row.get::<Option<String>,_>("disposition").unwrap_or_else(||"queued".into()),"delivery":row.get::<Option<Uuid>,_>("delivery_attempt").map(|attempt|json!({"attempt_id":attempt,"disposition":row.get::<String,_>("delivery_disposition")}))})
    }).collect();
    Ok(
        json!({"api_version":1,"recipient":request.recipient,"after":request.after,"next_cursor":last.to_string(),"has_more":has_more,"messages":messages}),
    )
}
async fn owner_inbox(
    State(state): State<AppState>,
    RequireOwner(_): RequireOwner,
    StrictJson(r): StrictJson<Inbox>,
) -> Result<Response> {
    let mut tx = state.store.pool.begin().await?;
    let response = inbox(&mut tx, r, None).await?;
    tx.commit().await?;
    Ok(ok(response))
}
async fn agent_inbox(
    State(state): State<AppState>,
    headers: HeaderMap,
    StrictJson(r): StrictJson<Inbox>,
) -> Result<Response> {
    let mut tx = state.store.pool.begin().await?;
    let p = authenticate_locked(&state, &headers, &mut tx).await?;
    capability(&p, "message.read")?;
    scope(&p, &r.recipient, true)?;
    let response = inbox(&mut tx, r, Some(id(&p.binding, "attempt_id")?)).await?;
    let response = bounded_inbox(&p, response)?;
    if p.expires <= now(&state) {
        return Err(Error("session_expired"));
    }
    tx.commit().await?;
    Ok(ok(response))
}
async fn agent_ack(
    State(state): State<AppState>,
    headers: HeaderMap,
    StrictJson(r): StrictJson<Ack>,
) -> Result<Response> {
    let mut tx = state.store.pool.begin().await?;
    let p = authenticate_locked(&state, &headers, &mut tx).await?;
    capability(&p, "message.ack")?;
    scope(&p, &r.recipient, true)?;
    let actor = sender_key(&sender(&p))?;
    let payload = digest(&json!({"kind":"ack","request":r,"sender":sender(&p)}))?;
    if let Some(response) = replay(&mut tx, &actor, r.operation_id, &payload).await? {
        if p.expires <= now(&state) {
            return Err(Error("session_expired"));
        }
        tx.commit().await?;
        return Ok(ok(response));
    }
    let cursor: Option<i64> = sqlx::query_scalar(
        "SELECT cursor FROM mailbox_messages WHERE message_id=$1 AND mailbox_key=$2",
    )
    .bind(r.message_id)
    .bind(r.recipient.key()?)
    .fetch_optional(&mut *tx)
    .await?;
    let cursor = cursor.ok_or(Error("not_found"))?;
    let attempt = id(&p.binding, "attempt_id")?;
    sqlx::query("INSERT INTO mailbox_receipts(message_id,attempt_id,disposition,acknowledged_at_ms) VALUES($1,$2,$3,$4) ON CONFLICT(message_id,attempt_id) DO UPDATE SET disposition=CASE WHEN mailbox_receipts.disposition='read' THEN 'read' ELSE EXCLUDED.disposition END,acknowledged_at_ms=EXCLUDED.acknowledged_at_ms").bind(r.message_id).bind(attempt).bind(r.disposition.name()).bind(now(&state) as i64).execute(&mut *tx).await?;
    let disposition: String = sqlx::query_scalar(
        "SELECT disposition FROM mailbox_receipts WHERE message_id=$1 AND attempt_id=$2",
    )
    .bind(r.message_id)
    .bind(attempt)
    .fetch_one(&mut *tx)
    .await?;
    let response = json!({"api_version":1,"operation_id":r.operation_id,"message_id":r.message_id,"recipient":r.recipient,"cursor":cursor.to_string(),"attempt_id":attempt,"disposition":disposition,"node_delivery":"not_attempted","harness_delivery":"not_attempted"});
    p.bound(&response)?;
    if p.expires <= now(&state) {
        return Err(Error("session_expired"));
    }
    receipt(&mut tx, &actor, r.operation_id, &payload, &response).await?;
    tx.commit().await?;
    Ok(ok(response))
}

// Bound complete envelopes, preserving an exclusive cursor. An oversized single
// message is represented explicitly; it is not acknowledged or injected.
fn bounded_inbox(p: &Principal, mut body: Value) -> Result<Value> {
    while p.bound(&body).is_err() {
        let recipient = body["recipient"].clone();
        let items = body["messages"].as_array_mut().ok_or(Error("internal"))?;
        if items.len() > 1 {
            items.pop();
            let cursor = items.last().unwrap()["cursor"].clone();
            body["next_cursor"] = cursor;
            body["has_more"] = json!(true);
        } else if items.len() == 1 && items[0].get("body_omitted").is_none() {
            let item = &items[0];
            let descriptor = json!({"message_id":item["message_id"],"cursor":item["cursor"],"body_omitted":true,"body_bytes":item["body"].as_str().map(str::len).unwrap_or(0),"recovery":"message_body","body_request":{"recipient":recipient,"message_id":item["message_id"],"offset":"0","max_bytes":16384}});
            body["messages"] = json!([descriptor]);
            body["next_cursor"] = body["after"].clone();
            body["has_more"] = json!(true);
        } else {
            return Err(Error("response_too_large"));
        }
    }
    Ok(body)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NodePoll {
    identity: NodeIdentity,
    attempt: AttemptRef,
    limit: u32,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct NodeAck {
    operation_id: Uuid,
    identity: NodeIdentity,
    attempt: AttemptRef,
    delivery_id: Uuid,
    disposition: NodeDisposition,
}
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum NodeDisposition {
    ReceivedByNode,
    HandedToHarness,
    DeliveryUnknown,
    Stale,
}
impl NodeDisposition {
    fn name(&self) -> &'static str {
        match self {
            Self::ReceivedByNode => "received_by_node",
            Self::HandedToHarness => "handed_to_harness",
            Self::DeliveryUnknown => "delivery_unknown",
            Self::Stale => "stale",
        }
    }
}
pub(super) fn node_routes() -> Router<AppState> {
    Router::new()
        .route("/v2/worker/messages/poll", post(node_poll))
        .route("/v2/worker/messages/ack", post(node_ack))
}
async fn node_authority(
    state: &AppState,
    node: &crate::worker_api::AuthenticatedNode,
    tx: &mut Transaction<'_, Postgres>,
    identity: &NodeIdentity,
    attempt: &AttemptRef,
) -> Result<Value> {
    crate::worker_api::lock_current_node(tx, node)
        .await
        .map_err(|_| Error("unauthorized"))?;
    if node
        .permitted_attempt()
        .is_some_and(|id| id != attempt.attempt_id)
    {
        return Err(Error("unauthorized"));
    }
    if node.id() != identity.node_id {
        return Err(Error("unauthorized"));
    }
    let a = lock_attempt(state, tx, attempt.attempt_id, node.id()).await?;
    if a.identity != *identity || a.attempt != *attempt {
        return Err(Error("stale_generation"));
    }
    let binding: Option<Value> = sqlx::query_scalar(
        "SELECT binding FROM agent_launch_bindings WHERE attempt_id=$1 AND NOT revoked",
    )
    .bind(attempt.attempt_id)
    .fetch_optional(&mut **tx)
    .await?;
    binding.ok_or(Error("not_found"))
}
fn recipients(b: &Value) -> Result<[Recipient; 2]> {
    let task = Recipient::Task {
        workspace_id: id(b, "workspace_id")?,
        project_id: id(b, "project_id")?,
        task_id: id(b, "task_id")?,
    };
    let (workspace_id, project_id, task_id) = task.scope()?;
    Ok([
        task,
        Recipient::Session {
            workspace_id,
            project_id,
            task_id,
            session_id: id(b, "session_id")?,
            attempt_id: id(b, "attempt_id")?,
            generation: b["generation"].as_u64().ok_or(Error("internal"))?,
        },
    ])
}
async fn node_poll(
    State(state): State<AppState>,
    headers: HeaderMap,
    StrictJson(r): StrictJson<NodePoll>,
) -> Result<Response> {
    if !(1..=16).contains(&r.limit) {
        return Err(Error("invalid_request"));
    }
    version(&headers)?;
    let auth = crate::delegated_permits::runtime::authenticate_runner(
        &state,
        &headers,
        aq_admission_sandbox::sandbox::RunnerOperation::MailboxPoll,
        Some(r.attempt.attempt_id),
    )
    .await
    .map_err(|_| Error("unauthorized"))?;
    let mut tx = state.store.pool.begin().await?;
    let b = node_authority(&state, &auth, &mut tx, &r.identity, &r.attempt).await?;
    let profile: Value =
        sqlx::query_scalar("SELECT profile FROM agent_launch_bindings WHERE attempt_id=$1")
            .bind(r.attempt.attempt_id)
            .fetch_one(&mut *tx)
            .await?;
    if !profile["capabilities"]
        .as_array()
        .is_some_and(|c| c.contains(&json!("message.read")))
    {
        return Err(Error("unsupported_capability"));
    }
    let recipients = recipients(&b)?;
    let task = recipients[0].key()?;
    let session = recipients[1].key()?;
    let rows = injectable(
        &mut tx,
        r.attempt.attempt_id,
        &task,
        &session,
        r.limit as i64,
    )
    .await?;
    let mut deliveries = Vec::new();
    for row in rows {
        let message: Uuid = row.get("message_id");
        let delivery = row
            .get::<Option<Uuid>, _>("delivery_id")
            .unwrap_or_else(Uuid::new_v4);
        sqlx::query("INSERT INTO mailbox_deliveries(delivery_id,message_id,attempt_id,disposition,updated_at_ms) VALUES($1,$2,$3,'pending',$4) ON CONFLICT(message_id,attempt_id) DO NOTHING").bind(delivery).bind(message).bind(r.attempt.attempt_id).bind(now(&state) as i64).execute(&mut *tx).await?;
        deliveries.push(json!({"delivery_id":delivery,"message_id":message,"recipient":row.get::<Value,_>("recipient"),"thread_id":row.get::<Uuid,_>("thread_id"),"cursor":row.get::<i64,_>("cursor").to_string(),"body":row.get::<String,_>("body"),"payload_digest":row.get::<String,_>("payload_digest"),"sender":row.get::<Value,_>("sender"),"disposition":row.get::<Option<String>,_>("disposition").unwrap_or_else(||"pending".into())}));
    }
    node_authority(&state, &auth, &mut tx, &r.identity, &r.attempt).await?;
    tx.commit().await?;
    Ok(ok(
        json!({"api_version":1,"attempt":r.attempt,"deliveries":deliveries}),
    ))
}
async fn node_ack(
    State(state): State<AppState>,
    headers: HeaderMap,
    StrictJson(r): StrictJson<NodeAck>,
) -> Result<Response> {
    version(&headers)?;
    let auth = crate::delegated_permits::runtime::authenticate_runner(
        &state,
        &headers,
        aq_admission_sandbox::sandbox::RunnerOperation::MailboxAck,
        Some(r.attempt.attempt_id),
    )
    .await
    .map_err(|_| Error("unauthorized"))?;
    let mut tx = state.store.pool.begin().await?;
    node_authority(&state, &auth, &mut tx, &r.identity, &r.attempt).await?;
    let actor = format!(
        "node-delivery:{}:{}",
        r.identity.node_id, r.attempt.attempt_id
    );
    let payload = digest(&serde_json::to_value(&r)?)?;
    if let Some(response) = replay(&mut tx, &actor, r.operation_id, &payload).await? {
        node_authority(&state, &auth, &mut tx, &r.identity, &r.attempt).await?;
        tx.commit().await?;
        return Ok(ok(response));
    }
    let old:Option<String>=sqlx::query_scalar("SELECT disposition FROM mailbox_deliveries WHERE delivery_id=$1 AND attempt_id=$2 FOR UPDATE").bind(r.delivery_id).bind(r.attempt.attempt_id).fetch_optional(&mut *tx).await?;
    let old = old.ok_or(Error("not_found"))?;
    let next = r.disposition.name();
    if old != next && !matches!(old.as_str(), "pending" | "received_by_node") {
        return Err(Error("id_conflict"));
    }
    // Unknown means no new injection/poll retry. Later reconciliation requires a
    // separate authority path; ordinary acknowledgements cannot erase uncertainty.
    sqlx::query(
        "UPDATE mailbox_deliveries SET disposition=$2,updated_at_ms=$3 WHERE delivery_id=$1",
    )
    .bind(r.delivery_id)
    .bind(next)
    .bind(now(&state) as i64)
    .execute(&mut *tx)
    .await?;
    let response = json!({"api_version":1,"operation_id":r.operation_id,"delivery_id":r.delivery_id,"attempt":r.attempt,"disposition":next});
    receipt(&mut tx, &actor, r.operation_id, &payload, &response).await?;
    node_authority(&state, &auth, &mut tx, &r.identity, &r.attempt).await?;
    tx.commit().await?;
    Ok(ok(response))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BodyChunk {
    recipient: Recipient,
    message_id: Uuid,
    offset: String,
    max_bytes: u32,
}
async fn agent_body(
    State(state): State<AppState>,
    headers: HeaderMap,
    StrictJson(r): StrictJson<BodyChunk>,
) -> Result<Response> {
    let mut tx = state.store.pool.begin().await?;
    let p = authenticate_locked(&state, &headers, &mut tx).await?;
    capability(&p, "message.read")?;
    scope(&p, &r.recipient, true)?;
    let offset: usize = r.offset.parse().map_err(|_| Error("invalid_request"))?;
    if offset.to_string() != r.offset || !(1..=16384).contains(&r.max_bytes) {
        return Err(Error("invalid_request"));
    }
    let row=sqlx::query("SELECT body,payload_digest,cursor FROM mailbox_messages WHERE message_id=$1 AND mailbox_key=$2").bind(r.message_id).bind(r.recipient.key()?).fetch_optional(&mut *tx).await?.ok_or(Error("not_found"))?;
    let text: String = row.get("body");
    if !text.is_char_boundary(offset) {
        return Err(Error("invalid_request"));
    }
    let mut end = offset.saturating_add(r.max_bytes as usize).min(text.len());
    let response = loop {
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let response = json!({"api_version":1,"message_id":r.message_id,"cursor":row.get::<i64,_>("cursor").to_string(),"payload_digest":row.get::<String,_>("payload_digest"),"offset":r.offset,"next_offset":end.to_string(),"complete":end==text.len(),"body":&text[offset..end]});
        if p.bound(&response).is_ok() && (end > offset || offset == text.len()) {
            break response;
        }
        if end <= offset {
            return Err(Error("response_too_large"));
        }
        let used = canonical(&response)?.len();
        let budget = p.profile["max_response_bytes"].as_u64().unwrap_or(0) as usize;
        end = end
            .saturating_sub(used.saturating_sub(budget).max(1))
            .max(offset);
    };
    if p.expires <= now(&state) {
        return Err(Error("session_expired"));
    }
    tx.commit().await?;
    Ok(ok(response))
}
