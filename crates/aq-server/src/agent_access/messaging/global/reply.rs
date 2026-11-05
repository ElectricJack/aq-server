//! F1b: the generation-bound supervisor credential and the supervisor reply
//! writer. The author is derived from the credential only; the reply carries
//! the exact dispatch association of the turn it answers.
use super::*;
use crate::agent_access::human::{enqueue::GraphCreate, graph_actions::GraphPayload};
use aq_central_authority::{Digest, DispatchAssociation, ReplyAssociation};
use axum::http::{HeaderMap, header::AUTHORIZATION};

pub const SCOPES: [&str; 5] = [
    "graph.read",
    "graph.write",
    "enqueue",
    "messaging",
    "decisions",
];
pub const REPLY_CONTRACT: &str = "supervisor-conversation-reply-1";
const TOKEN_PREFIX: &str = "aqsv1_";
const MAX_TTL_MS: u64 = 24 * 60 * 60 * 1000;

/// The only identity a supervisor credential grants. No worker, task or attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SupervisorPrincipal {
    pub actor_id: Uuid,
    pub generation: u64,
    pub session_id: Uuid,
    pub scopes: Vec<String>,
}

pub(super) fn rejected() -> AppError {
    AppError::Unauthenticated("supervisor credential rejected".into())
}
fn inactive() -> AppError {
    AppError::Conflict("supervisor session is not active".into())
}
fn token_digest(token: &str) -> String {
    hex::encode(sha2::Sha256::digest(token.as_bytes()))
}
fn well_formed(token: &str) -> bool {
    token.strip_prefix(TOKEN_PREFIX).is_some_and(|h| {
        h.len() == 64
            && h.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}
fn millis(n: u64) -> Result<i64> {
    i64::try_from(n)
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(unavailable)
}

/// The actor's head generation and session while that generation is active.
async fn current(
    pool: &PgPool,
    tx: &mut Transaction<'_, Postgres>,
    actor: Uuid,
) -> Result<Option<(u64, Uuid)>> {
    let snapshot = dispatch::sessions(pool)
        .inspect_in_transaction(tx, actor)
        .await
        .map_err(|_| unavailable())?;
    Ok(snapshot.filter(dispatch::active).and_then(|s| {
        let plan = s.run.plan();
        dispatch::session_id(plan).map(|id| (plan.generation, id))
    }))
}

/// Issues the one live credential for the actor's active head generation.
/// Prior live credentials are revoked; only the digest is stored.
pub async fn issue_credential(
    pool: &PgPool,
    actor: Uuid,
    ttl_ms: u64,
    at_ms: u64,
) -> Result<String> {
    if ttl_ms == 0 || ttl_ms > MAX_TTL_MS {
        return Err(invalid());
    }
    let at = millis(at_ms)?;
    let expires = millis(at_ms.checked_add(ttl_ms).ok_or_else(invalid)?)?;
    let mut tx = pool.begin().await.map_err(db)?;
    let (generation, session_id) = current(pool, &mut tx, actor).await?.ok_or_else(inactive)?;
    let g = i64::try_from(generation).map_err(|_| inactive())?;
    let token = {
        use rand::RngCore;
        let mut b = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut b);
        format!("{TOKEN_PREFIX}{}", hex::encode(b))
    };
    sqlx::query("UPDATE central_supervisor_credentials SET revoked_at_ms=$2 WHERE actor_id=$1 AND revoked_at_ms IS NULL")
        .bind(actor)
        .bind(at)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    sqlx::query("INSERT INTO central_supervisor_credentials(token_digest,actor_id,generation,session_id,scopes,issued_at_ms,expires_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7)")
        .bind(token_digest(&token))
        .bind(actor)
        .bind(g)
        .bind(session_id)
        .bind(SCOPES.iter().map(|s| s.to_string()).collect::<Vec<_>>())
        .bind(at)
        .bind(expires)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    tx.commit().await.map_err(db)?;
    Ok(token)
}

/// Use-time check: present, unrevoked, unexpired, scoped, and still the
/// actor's active head generation. Anything else is 401 (403 for scope).
pub async fn authenticate(
    pool: &PgPool,
    bearer: &str,
    scope: &str,
    at_ms: u64,
) -> Result<SupervisorPrincipal> {
    let mut tx = pool.begin().await.map_err(db)?;
    let principal = authenticate_in_transaction(pool, &mut tx, bearer, scope, at_ms).await?;
    tx.commit().await.map_err(db)?;
    Ok(principal)
}

/// Authenticate inside the transaction that performs the protected operation.
/// Required scopes: graph.read for graph reads, graph.write for graph mutations,
/// enqueue for work creation, messaging for replies/mail, decisions for decisions.
/// Locks the actor before the credential, matching issuance and lifecycle writes.
/// Keep this transaction open through the protected read or write.
pub async fn authenticate_in_transaction(
    pool: &PgPool,
    tx: &mut Transaction<'_, Postgres>,
    bearer: &str,
    scope: &str,
    at_ms: u64,
) -> Result<SupervisorPrincipal> {
    if !well_formed(bearer) {
        return Err(rejected());
    }
    let actor: Uuid = sqlx::query_scalar(
        "SELECT actor_id FROM central_supervisor_credentials WHERE token_digest=$1",
    )
    .bind(token_digest(bearer))
    .fetch_optional(&mut **tx)
    .await
    .map_err(db)?
    .ok_or_else(rejected)?;
    let active = current(pool, tx, actor).await?.ok_or_else(rejected)?;
    let at = millis(at_ms)?;
    let row = sqlx::query("SELECT actor_id,generation,session_id,scopes,expires_at_ms,revoked_at_ms FROM central_supervisor_credentials WHERE token_digest=$1 FOR SHARE")
        .bind(token_digest(bearer))
        .fetch_optional(&mut **tx)
        .await
        .map_err(db)?
        .ok_or_else(rejected)?;
    if row.get::<Option<i64>, _>("revoked_at_ms").is_some()
        || row.get::<i64, _>("expires_at_ms") <= at
    {
        return Err(rejected());
    }
    let principal = SupervisorPrincipal {
        actor_id: row.get("actor_id"),
        generation: u64::try_from(row.get::<i64, _>("generation")).map_err(|_| rejected())?,
        session_id: row.get("session_id"),
        scopes: row.get("scopes"),
    };
    if active != (principal.generation, principal.session_id) {
        return Err(rejected());
    }
    if !principal.scopes.iter().any(|s| s == scope) {
        return Err(AppError::Forbidden(
            "supervisor credential lacks scope".into(),
        ));
    }
    Ok(principal)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplyRequest {
    operation_id: Uuid,
    message_id: Uuid,
    dispatch_id: Uuid,
    body: String,
    /// Optional typed History items. Omitted when empty, so pre-attachment
    /// replay digests are unchanged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    attachments: Vec<AttachmentRequest>,
}

pub const MAX_ATTACHMENTS: usize = 4;
const MAX_ATTACHMENT_BYTES: usize = 65536;

/// A supervisor-authored attachment. Drafts carry their exact proposed graph
/// payload; receipts name one of this session's confirmed creation operations.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum AttachmentRequest {
    ProposedTaskDraft {
        draft_id: Uuid,
        payload: GraphCreate,
    },
    ActionReceipt {
        operation_id: Uuid,
    },
}
impl ReplyRequest {
    fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() || bytes.len() > 131072 {
            return Err(invalid());
        }
        let raw = aq_agent_api::parse(bytes).map_err(|_| invalid())?;
        let v: Self = serde_json::from_value(raw).map_err(|_| invalid())?;
        if v.operation_id == v.message_id
            || v.operation_id.is_nil()
            || v.message_id.is_nil()
            || v.dispatch_id.is_nil()
            || v.body.trim().is_empty()
            || v.body.len() > 16384
            || v.body.contains('\0')
            || v.attachments.len() > MAX_ATTACHMENTS
            || serde_json::to_vec(&v.attachments)
                .map_err(|_| invalid())?
                .len()
                > MAX_ATTACHMENT_BYTES
        {
            return Err(invalid());
        }
        let mut ids = std::collections::BTreeSet::new();
        for a in &v.attachments {
            let fresh = match a {
                AttachmentRequest::ProposedTaskDraft { draft_id, payload } => {
                    !draft_id.is_nil() && payload.valid() && ids.insert(*draft_id)
                }
                AttachmentRequest::ActionReceipt { operation_id } => {
                    !operation_id.is_nil() && ids.insert(*operation_id)
                }
            };
            if !fresh {
                return Err(invalid());
            }
        }
        Ok(v)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplyReceipt {
    pub contract: String,
    pub reply_message_id: Uuid,
    pub operation_id: Uuid,
    pub dispatch_id: Uuid,
    pub cursor: String,
    pub author: Value,
    pub logical_admission: String,
    pub host_delivery: String,
    pub agent_receipt: Option<Value>,
}

pub(super) fn author(p: &SupervisorPrincipal) -> Value {
    json!({"kind":"central_supervisor","actor_id":p.actor_id,"generation":p.generation,"session_id":p.session_id})
}
pub(super) fn reply_recipient(conversation: Uuid) -> Value {
    json!({"kind":"global_supervisor_reply","conversation_id":conversation})
}

/// The exact association of the principal's generation, if it is `dispatch_id`.
async fn association(
    pool: &PgPool,
    p: &SupervisorPrincipal,
    dispatch_id: Uuid,
) -> Result<DispatchAssociation> {
    let snapshot = dispatch::sessions(pool)
        .generation(p.actor_id, p.generation)
        .await
        .map_err(|_| unavailable())?
        .ok_or_else(unknown_dispatch)?;
    snapshot
        .run
        .association()
        .filter(|a| a.dispatch_id == dispatch_id)
        .cloned()
        .ok_or_else(unknown_dispatch)
}
fn unknown_dispatch() -> AppError {
    AppError::Conflict("reply does not answer this generation's dispatch".into())
}

pub(super) async fn reply(
    State(state): State<AppState>,
    Extension(service): Extension<Arc<Service>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Result<Json<ReplyReceipt>> {
    let at = stamp(&state)?;
    let bearer = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(rejected)?;
    let principal = authenticate(&service.pool, bearer, "messaging", at).await?;
    let request = ReplyRequest::parse(&bytes)?;
    let dispatch = association(&service.pool, &principal, request.dispatch_id).await?;
    write(&service, &principal, bearer, request, dispatch, at)
        .await
        .map(Json)
}

async fn write(
    service: &Service,
    p: &SupervisorPrincipal,
    bearer: &str,
    request: ReplyRequest,
    dispatch: DispatchAssociation,
    at: u64,
) -> Result<ReplyReceipt> {
    let mut tx = service.tx().await?;
    let current =
        authenticate_in_transaction(&service.pool, &mut tx, bearer, "messaging", at).await?;
    if &current != p {
        return Err(rejected());
    }
    let conversation = definition(&mut tx, None, true).await?;
    let sender = author(p);
    let sender_key = sender_key(&sender).map_err(|_| unavailable())?;
    let payload = digest(&json!({"kind":"global_supervisor_reply","conversation_id":conversation.id,"request":&request,"sender":&sender}))
        .map_err(|_| unavailable())?;
    if let Some(old) = replay(&mut tx, &sender_key, request.operation_id, &payload)
        .await
        .map_err(|e| {
            if e.0 == "id_conflict" {
                conflict()
            } else {
                unavailable()
            }
        })?
    {
        let receipt: ReplyReceipt = serde_json::from_value(old).map_err(|_| unavailable())?;
        if receipt.contract != REPLY_CONTRACT || receipt.operation_id != request.operation_id {
            return Err(conflict());
        }
        tx.commit().await.map_err(db)?;
        return Ok(receipt);
    }
    let row = sqlx::query("SELECT actor_id,generation,host_operation_id,batch_digest FROM central_supervisor_dispatches WHERE dispatch_id=$1")
        .bind(request.dispatch_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db)?
        .ok_or_else(unknown_dispatch)?;
    if row.get::<Uuid, _>("actor_id") != p.actor_id
        || u64::try_from(row.get::<i64, _>("generation")).ok() != Some(p.generation)
        || row.get::<Uuid, _>("host_operation_id") != dispatch.host_operation_id
        || row.get::<String, _>("batch_digest") != dispatch.batch_digest.as_str()
    {
        return Err(unknown_dispatch());
    }
    let attachments = resolve(&mut tx, p, &request).await?;
    let attachment_bytes = serde_json::to_vec(&attachments)
        .map_err(|_| unavailable())?
        .len();
    let reserved =
        i64::try_from(request.body.len() + attachment_bytes + 4096).map_err(|_| invalid())?;
    let budget = sqlx::query("UPDATE global_supervisor_conversation SET retained_operations=retained_operations+1,retained_bytes=retained_bytes+$1 WHERE singleton AND retained_operations<$2 AND retained_bytes<=$3-$1 RETURNING conversation_id")
        .bind(reserved)
        .bind(MAX_OPERATIONS)
        .bind(MAX_BYTES)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db)?;
    if budget.is_none() {
        return Err(AppError::Maintenance(
            "supervisor mailbox retention capacity exhausted".into(),
        ));
    }
    let to = reply_recipient(conversation.id);
    let mailbox_key = digest(&to).map_err(|_| unavailable())?;
    let cursor = append(
        &mut tx,
        Mail {
            message_id: request.message_id,
            thread_id: conversation.id,
            recipient: to,
            mailbox_key,
            body: &request.body,
        },
        &sender,
        &payload,
        at,
    )
    .await
    .map_err(|e| {
        if e.0 == "id_conflict" {
            conflict()
        } else {
            unavailable()
        }
    })?;
    let body_digest = Digest::of(request.body.as_bytes());
    let link = ReplyAssociation {
        dispatch,
        reply_message_id: request.message_id,
        reply_operation_id: request.operation_id,
        body_digest: body_digest.clone(),
    };
    link.validate().map_err(|_| invalid())?;
    // Migration 49 consumes the associated reservation and clears its actor's
    // current pointer in this transaction. Reply replay returns the original
    // receipt above, so it cannot release a newer reservation.
    // Without attachments the row is exactly the pre-0062 row (column default).
    let insert = if attachments.is_empty() {
        "INSERT INTO central_supervisor_replies(reply_message_id,reply_operation_id,dispatch_id,body_digest,association) VALUES($1,$2,$3,$4,$5)"
    } else {
        "INSERT INTO central_supervisor_replies(reply_message_id,reply_operation_id,dispatch_id,body_digest,association,attachments) VALUES($1,$2,$3,$4,$5,$6)"
    };
    let mut insert = sqlx::query(insert)
        .bind(request.message_id)
        .bind(request.operation_id)
        .bind(request.dispatch_id)
        .bind(body_digest.as_str())
        .bind(json!(link));
    if !attachments.is_empty() {
        insert = insert.bind(Value::Array(attachments));
    }
    insert.execute(&mut *tx).await.map_err(|_| conflict())?;
    let result = ReplyReceipt {
        contract: REPLY_CONTRACT.into(),
        reply_message_id: request.message_id,
        operation_id: request.operation_id,
        dispatch_id: request.dispatch_id,
        cursor: cursor.to_string(),
        author: sender,
        logical_admission: "queued".into(),
        host_delivery: "not_attempted".into(),
        agent_receipt: None,
    };
    receipt(
        &mut tx,
        &sender_key,
        request.operation_id,
        &payload,
        &json!(result),
    )
    .await
    .map_err(|_| unavailable())?;
    tx.commit().await.map_err(db)?;
    Ok(result)
}

fn receipt_unavailable() -> AppError {
    AppError::Conflict("attached operation is not a confirmed receipt of this session".into())
}

/// Server-derived History items, linked to the reply that produced them.
/// Receipts are snapshotted from this session's journal in the reply
/// transaction, so Owners retain access after the journal row retires.
async fn resolve(
    tx: &mut Transaction<'_, Postgres>,
    p: &SupervisorPrincipal,
    request: &ReplyRequest,
) -> Result<Vec<Value>> {
    let message = request.message_id;
    let mut out = Vec::with_capacity(request.attachments.len());
    for a in &request.attachments {
        out.push(match a {
            AttachmentRequest::ProposedTaskDraft { draft_id, payload } => {
                let payload = json!(payload);
                let payload_digest =
                    format!("sha256:{}", digest(&payload).map_err(|_| invalid())?);
                json!({"type":"proposed_task_draft","message_id":message,"draft_id":draft_id,"project_id":payload["project_id"],"revision":payload["expected_version"],"payload_digest":payload_digest,"payload":payload})
            }
            AttachmentRequest::ActionReceipt { operation_id } => {
                let row = sqlx::query("SELECT kind,request_digest,response FROM dashboard_operations WHERE operation_id=$1 AND supervisor_session_id=$2 AND disposition='confirmed' AND retired_at IS NULL AND kind IN ('task.create','task.graph.create')")
                    .bind(operation_id)
                    .bind(p.session_id)
                    .fetch_optional(&mut **tx)
                    .await
                    .map_err(db)?
                    .ok_or_else(receipt_unavailable)?;
                let response: Value = row.get("response");
                action_receipt(
                    message,
                    *operation_id,
                    row.get("kind"),
                    row.get("request_digest"),
                    &response,
                )
                .ok_or_else(unavailable)?
            }
        });
    }
    Ok(out)
}

fn action_receipt(
    message: Uuid,
    operation: Uuid,
    kind: String,
    request: String,
    op: &Value,
) -> Option<Value> {
    let subject = op["subject_refs"].as_array()?.first()?;
    let project: Uuid = subject["id"].as_str()?.parse().ok()?;
    if subject["type"] != "project" {
        return None;
    }
    let ids = |entity: &str| -> Option<Vec<Uuid>> {
        op["affected_entities"]
            .as_array()?
            .iter()
            .filter(|e| e["type"] == entity)
            .map(|e| e["id"].as_str()?.parse().ok())
            .collect()
    };
    let refs: Vec<Value> = op["receipt_refs"]
        .as_array()?
        .iter()
        .map(|r| json!({"type":r["type"],"id":r["id"]}))
        .collect();
    let tasks = ids("task")?;
    if tasks.is_empty() || refs.is_empty() {
        return None;
    }
    Some(
        json!({"type":"action_receipt","message_id":message,"operation_id":operation,"kind":kind,"project_id":project,"project_revision":subject.get("revision"),"created_task_ids":tasks,"created_edge_ids":ids("graph_edge")?,"receipt_refs":refs,"request_digest":request}),
    )
}

#[cfg(test)]
#[path = "reply_tests.rs"]
mod tests;
