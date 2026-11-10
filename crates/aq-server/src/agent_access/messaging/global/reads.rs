use super::*;
use aq_supervisor_control::wire::{Discovery, Limits};
pub(super) async fn discovery(
    Extension(service): Extension<Arc<Service>>,
    RequireOwner(user): RequireOwner,
) -> Result<Json<Discovery>> {
    discover(service, user, None).await
}
pub(super) async fn project_discovery(
    Extension(service): Extension<Arc<Service>>,
    RequireOwner(user): RequireOwner,
    Path(project): Path<Uuid>,
) -> Result<Json<Discovery>> {
    let id: Option<Uuid> = sqlx::query_scalar(
        "SELECT conversation_id FROM supervisor_conversation_ids WHERE project_id=$1",
    )
    .bind(project)
    .fetch_optional(&service.pool)
    .await
    .map_err(db)?;
    discover(service, user, Some(id.ok_or_else(missing)?)).await
}
async fn discover(service: Arc<Service>, user: User, id: Option<Uuid>) -> Result<Json<Discovery>> {
    let mut tx = service.tx().await?;
    owner(&mut tx, &user).await?;
    let conversation = definition(&mut tx, id, false).await?;
    tx.commit().await.map_err(db)?;
    Ok(Json(Discovery {
        contract: CONTRACT,
        runtime: live_runtime(&service.pool, conversation.id).await,
        conversation,
        limits: Limits {
            body_bytes: 16384,
            context_refs: writes::MAX_CONTEXT_REFS as u8,
            attachments: attachments::MAX_ATTACHMENTS as u8,
            history_page: 16,
        },
    }))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Page {
    before: Option<String>,
    after: Option<String>,
    limit: Option<u8>,
}
#[derive(Serialize)]
struct Human {
    kind: &'static str,
    user_id: Uuid,
}
#[derive(Serialize)]
struct Message {
    message_id: Uuid,
    thread_id: Uuid,
    conversation_id: Uuid,
    cursor: String,
    author: Human,
    body: String,
    body_digest: String,
    created_at_ms: u64,
    /// X5: pinned context refs; omitted when the message has none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    context: Vec<writes::ContextRef>,
    /// agile-vault-27: owner files bound to this message, in Create order;
    /// omitted when the message has none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    attachments: Vec<attachments::Descriptor>,
    logical_admission: &'static str,
    host_delivery: &'static str,
    agent_receipt: Option<Value>,
}
#[derive(Serialize)]
struct SupervisorAuthor {
    kind: &'static str,
    actor_id: Uuid,
    generation: u64,
    session_id: Uuid,
}
#[derive(Serialize)]
struct DispatchRef {
    dispatch_id: Uuid,
    host_operation_id: Uuid,
    batch_digest: String,
}
/// A supervisor reply. Its cursor is `"anchor.r"` (see [`Position`]).
#[derive(Serialize)]
struct Reply {
    message_id: Uuid,
    thread_id: Uuid,
    conversation_id: Uuid,
    cursor: String,
    author: SupervisorAuthor,
    body: String,
    body_digest: String,
    created_at_ms: u64,
    dispatch: DispatchRef,
    /// Typed proposed-draft and action-receipt items this reply produced.
    attachments: Vec<Value>,
    logical_admission: &'static str,
    host_delivery: &'static str,
    agent_receipt: Option<Value>,
}
#[derive(Serialize)]
#[serde(untagged)]
enum Item {
    Human(Message),
    Supervisor(Reply),
}
#[derive(Serialize)]
pub(super) struct History {
    contract: &'static str,
    conversation: Conversation,
    runtime: Runtime,
    direction: &'static str,
    through_cursor: String,
    items: Vec<Item>,
    next_cursor: Option<String>,
    has_more: bool,
}
fn message(row: sqlx::postgres::PgRow, id: Uuid) -> Result<Message> {
    let sender: Value = row.get("sender");
    let recipient_value: Value = row.get("recipient");
    let thread: Uuid = row.get("thread_id");
    let message_id: Uuid = row.get("message_id");
    let body: String = row.get("body");
    let cursor: i64 = row.get("cursor");
    let at: i64 = row.get("created_at_ms");
    let payload: String = row.get("payload_digest");
    let context = writes::stored_context(row.get("context_refs"))?;
    let attachments = attachments::stored(row.get("attachment_items"))?;
    let user = sender
        .get("user_id")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<Uuid>().ok())
        .filter(|u| !u.is_nil())
        .ok_or_else(unavailable)?;
    if sender != author(user)
        || recipient_value != recipient(id)
        || thread != id
        || message_id.is_nil()
        || body.trim().is_empty()
        || body.len() > 16384
        || body.contains('\0')
        || cursor <= 0
        || at <= 0
        || payload.len() != 64
        || !payload
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(unavailable());
    }
    Ok(Message {
        message_id,
        thread_id: thread,
        conversation_id: id,
        cursor: cursor.to_string(),
        author: Human {
            kind: "human",
            user_id: user,
        },
        body_digest: hash(body.as_bytes()),
        body,
        created_at_ms: at as u64,
        context,
        attachments,
        logical_admission: "queued",
        host_delivery: "not_attempted",
        agent_receipt: None,
    })
}
/// A position in the merged conversation order. An owner message at mailbox
/// cursor `c` is `(c, 0)`, rendered `"c"`. A supervisor reply is
/// `(anchor, r)`, rendered `"anchor.r"`: `anchor` is the max cursor of the
/// batch it answers and `r` (>= 1) its own reply-mailbox cursor. Replies sort
/// after the owner message they answer and before the next one.
pub(super) type Position = (i64, i64);
pub(super) fn position(s: &str, zero: bool) -> Result<Position> {
    match s.split_once('.') {
        Some((anchor, reply)) => Ok((positive(anchor)?, positive(reply)?)),
        None if zero => Ok((cursor(s)?, 0)),
        None => Ok((positive(s)?, 0)),
    }
}
pub(super) fn render((at, reply): Position) -> String {
    if reply == 0 {
        at.to_string()
    } else {
        format!("{at}.{reply}")
    }
}
pub(super) async fn history(
    Extension(service): Extension<Arc<Service>>,
    RequireOwner(user): RequireOwner,
    Path(id): Path<Uuid>,
    Query(page): Query<Page>,
) -> Result<Json<History>> {
    if id.is_nil() || page.before.is_some() && page.after.is_some() {
        return Err(invalid());
    }
    let limit = page.limit.unwrap_or(16);
    if !(1..=16).contains(&limit) {
        return Err(invalid());
    }
    let before = page
        .before
        .as_deref()
        .map(|s| position(s, false))
        .transpose()?;
    let after = page
        .after
        .as_deref()
        .map(|s| position(s, true))
        .transpose()?;
    let forward = after.is_some();
    // Before the transaction: the pool is two connections wide.
    let runtime = live_runtime(&service.pool, id).await;
    let mut tx = service.tx().await?;
    owner(&mut tx, &user).await?;
    let conversation = definition(&mut tx, Some(id), false).await?;
    let mailbox = key(id)?;
    let replies_key = digest(&reply::reply_recipient(id)).map_err(|_| unavailable())?;
    let next: Option<i64> =
        sqlx::query_scalar("SELECT next_cursor FROM mailbox_heads WHERE mailbox_key=$1")
            .bind(&mailbox)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db)?;
    let through = next
        .unwrap_or(1)
        .checked_sub(1)
        .filter(|v| *v >= 0)
        .ok_or_else(unavailable)?;
    // Exclusive bound in the merged order; both streams are filtered by the
    // same row comparison so a page boundary may fall between any two items.
    let (anchor, at) = match (after, before) {
        (Some(a), _) => a,
        (None, Some(b)) => b,
        (None, None) => (i64::MAX, i64::MAX),
    };
    // Owner mail only: agent-sent supervisor mail (bold-stone-51) is not yet
    // rendered to humans; it reaches the supervisor through dispatch.
    let (human_sql, reply_sql) = if forward {
        (
            "SELECT m.*,c.refs AS context_refs,{descriptors} AS attachment_items FROM mailbox_messages m LEFT JOIN global_supervisor_message_context c ON c.message_id=m.message_id WHERE m.mailbox_key=$1 AND m.sender->>'kind'='owner' AND (m.cursor,0::bigint)>($2,$3) AND m.cursor<=$4 ORDER BY m.cursor LIMIT $5",
            "SELECT * FROM (SELECT r.dispatch_id,r.attachments,d.host_operation_id,d.batch_digest,d.actor_id,d.generation,d.session_id,m.*,(SELECT max(mm.cursor) FROM central_supervisor_mail_members mm WHERE mm.actor_id=d.actor_id AND mm.reservation_id=d.reservation_id) AS anchor FROM central_supervisor_replies r JOIN central_supervisor_dispatches d ON d.dispatch_id=r.dispatch_id JOIN mailbox_messages m ON m.message_id=r.reply_message_id WHERE m.mailbox_key=$1) x WHERE (x.anchor,x.cursor)>($2,$3) AND x.anchor<=$4 ORDER BY x.anchor,x.cursor LIMIT $5",
        )
    } else {
        (
            "SELECT m.*,c.refs AS context_refs,{descriptors} AS attachment_items FROM mailbox_messages m LEFT JOIN global_supervisor_message_context c ON c.message_id=m.message_id WHERE m.mailbox_key=$1 AND m.sender->>'kind'='owner' AND (m.cursor,0::bigint)<($2,$3) AND m.cursor<=$4 ORDER BY m.cursor DESC LIMIT $5",
            "SELECT * FROM (SELECT r.dispatch_id,r.attachments,d.host_operation_id,d.batch_digest,d.actor_id,d.generation,d.session_id,m.*,(SELECT max(mm.cursor) FROM central_supervisor_mail_members mm WHERE mm.actor_id=d.actor_id AND mm.reservation_id=d.reservation_id) AS anchor FROM central_supervisor_replies r JOIN central_supervisor_dispatches d ON d.dispatch_id=r.dispatch_id JOIN mailbox_messages m ON m.message_id=r.reply_message_id WHERE m.mailbox_key=$1) x WHERE (x.anchor,x.cursor)<($2,$3) AND x.anchor<=$4 ORDER BY x.anchor DESC,x.cursor DESC LIMIT $5",
        )
    };
    let fetch = i64::from(limit) + 1;
    let human_sql = human_sql.replace("{descriptors}", attachments::DESCRIPTORS);
    let humans = sqlx::query(&human_sql)
        .bind(&mailbox)
        .bind(anchor)
        .bind(at)
        .bind(through)
        .bind(fetch)
        .fetch_all(&mut *tx)
        .await
        .map_err(db)?
        .into_iter()
        .map(|row| message(row, id).map(|m| m.cursor.parse().map(|c| ((c, 0), m))))
        .collect::<Result<std::result::Result<Vec<(Position, Message)>, _>>>()?
        .map_err(|_| unavailable())?;
    let replies = sqlx::query(reply_sql)
        .bind(&replies_key)
        .bind(anchor)
        .bind(at)
        .bind(through)
        .bind(fetch)
        .fetch_all(&mut *tx)
        .await
        .map_err(db)?
        .into_iter()
        .map(|row| reply(row, id))
        .collect::<Result<Vec<_>>>()?;
    // Merge both streams (each already in page order) into page order.
    let mut humans = humans.into_iter().peekable();
    let mut replies = replies.into_iter().peekable();
    let mut merged: Vec<(Position, Item)> = Vec::new();
    loop {
        let human_first = match (humans.peek(), replies.peek()) {
            (None, None) => break,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (Some(h), Some(r)) => (h.0 < r.0) == forward,
        };
        merged.push(
            if human_first {
                humans.next().map(|(p, m)| (p, Item::Human(m)))
            } else {
                replies.next().map(|(p, r)| (p, Item::Supervisor(r)))
            }
            .ok_or_else(unavailable)?,
        );
        if merged.len() > usize::from(limit) {
            break;
        }
    }
    let mut result = History {
        contract: CONTRACT,
        conversation,
        runtime,
        direction: if forward { "forward" } else { "backward" },
        through_cursor: through.to_string(),
        items: vec![],
        next_cursor: None,
        has_more: false,
    };
    let mut bytes = serde_json::to_vec(&result)
        .map_err(|_| unavailable())?
        .len()
        + 256;
    let mut page: Vec<(Position, Item)> = Vec::new();
    for (at, item) in merged {
        if page.len() >= usize::from(limit) {
            result.has_more = true;
            break;
        }
        let size = serde_json::to_vec(&item).map_err(|_| unavailable())?.len() + 1;
        if bytes.checked_add(size).is_none_or(|n| n > PAGE_BYTES) {
            result.has_more = true;
            break;
        }
        bytes += size;
        page.push((at, item));
    }
    if result.has_more {
        result.next_cursor = Some(render(page.last().ok_or_else(unavailable)?.0));
    }
    if !forward {
        page.reverse();
    }
    let ids: Vec<Uuid> = page
        .iter()
        .filter_map(|(_, item)| match item {
            Item::Human(m) => Some(m.message_id),
            Item::Supervisor(_) => None,
        })
        .collect();
    let carriers: Vec<(Uuid, Uuid, i64)> = sqlx::query("SELECT b.message_id,d.actor_id,d.generation FROM central_supervisor_mail_members b JOIN central_supervisor_dispatches d ON d.actor_id=b.actor_id AND d.reservation_id=b.reservation_id WHERE b.message_id=ANY($1) LIMIT 64")
        .bind(&ids)
        .fetch_all(&mut *tx)
        .await
        .map_err(db)?
        .into_iter()
        .map(|r| (r.get("message_id"), r.get("actor_id"), r.get("generation")))
        .collect();
    tx.commit().await.map_err(db)?;
    let sessions = dispatch::sessions(&service.pool);
    for (_, item) in page.iter_mut() {
        let Item::Human(owner_item) = item else {
            continue;
        };
        if let Some((_, actor, generation)) = carriers.iter().find(|c| c.0 == owner_item.message_id)
        {
            let generation = u64::try_from(*generation).map_err(|_| unavailable())?;
            let snapshot = sessions
                .generation(*actor, generation)
                .await
                .map_err(|_| unavailable())?;
            owner_item.host_delivery =
                dispatch::delivery(snapshot.as_ref().map(|s| s.run.dispatch()));
        }
    }
    result.items = page.into_iter().map(|(_, item)| item).collect();
    if serde_json::to_vec(&result)
        .map_err(|_| unavailable())?
        .len()
        > PAGE_BYTES
    {
        return Err(unavailable());
    }
    Ok(Json(result))
}
fn reply(row: sqlx::postgres::PgRow, id: Uuid) -> Result<(Position, Reply)> {
    let sender: Value = row.get("sender");
    let actor_id: Uuid = row.get("actor_id");
    let generation = u64::try_from(row.get::<i64, _>("generation")).map_err(|_| unavailable())?;
    let session_id: Uuid = row.get("session_id");
    let body: String = row.get("body");
    let cursor: i64 = row.get("cursor");
    let at: i64 = row.get("created_at_ms");
    let anchor: Option<i64> = row.get("anchor");
    let message_id: Uuid = row.get("message_id");
    let attachments = match row.get::<Value, _>("attachments") {
        Value::Array(items)
            if items.len() <= reply::MAX_ATTACHMENTS
                && items.iter().all(|a| a["message_id"] == json!(message_id)) =>
        {
            items
        }
        _ => return Err(unavailable()),
    };
    let expected = json!({"kind":"central_supervisor","actor_id":actor_id,"generation":generation,"session_id":session_id});
    if sender != expected
        || row.get::<Value, _>("recipient") != reply::reply_recipient(id)
        || row.get::<Uuid, _>("thread_id") != id
        || body.trim().is_empty()
        || body.len() > 16384
        || cursor <= 0
        || at <= 0
    {
        return Err(unavailable());
    }
    let anchor = anchor.filter(|a| *a > 0).ok_or_else(unavailable)?;
    Ok((
        (anchor, cursor),
        Reply {
            message_id,
            thread_id: id,
            conversation_id: id,
            cursor: render((anchor, cursor)),
            author: SupervisorAuthor {
                kind: "supervisor",
                actor_id,
                generation,
                session_id,
            },
            body_digest: hash(body.as_bytes()),
            body,
            created_at_ms: at as u64,
            dispatch: DispatchRef {
                dispatch_id: row.get("dispatch_id"),
                host_operation_id: row.get("host_operation_id"),
                batch_digest: row.get("batch_digest"),
            },
            attachments,
            logical_admission: "queued",
            host_delivery: "not_attempted",
            agent_receipt: None,
        },
    ))
}
