use super::*;
pub(crate) mod action;
#[derive(Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Create {
    operation_id: Uuid,
    message_id: Uuid,
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    expected_conversation_revision: String,
    #[schemars(length(min = 1, max = 16384))]
    body: String,
    #[schemars(length(max = 16))]
    context: Vec<ContextRef>,
    /// agile-vault-27: staged uploads pinned by id and content digest.
    #[schemars(length(max = 4))]
    attachments: Vec<super::attachments::AttachmentRef>,
}
pub(super) const MAX_CONTEXT_REFS: usize = 16;
/// X5: a pinned reference to an AQ entity the owner attached to a message.
/// The server checks shape only; the supervisor resolves refs under its own
/// authority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct ContextRef {
    #[serde(rename = "type")]
    kind: ContextKind,
    id: Uuid,
    /// Required, nullable: an absent field is refused like the schema does.
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    #[serde(deserialize_with = "Option::deserialize")]
    revision: Option<String>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(super) enum ContextKind {
    Project,
    Task,
    Result,
    Review,
    Knowledge,
    Session,
    PlaybookRun,
}
impl ContextRef {
    fn valid(&self) -> bool {
        !self.id.is_nil() && self.revision.as_deref().is_none_or(|r| positive(r).is_ok())
    }
    /// One stable line for the supervisor turn text.
    pub(super) fn render(&self) -> String {
        let kind = serde_json::to_value(self.kind)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default();
        match &self.revision {
            Some(r) => format!("{kind} {} revision {r}", self.id),
            None => format!("{kind} {}", self.id),
        }
    }
}
/// Decodes stored context refs, refusing anything the writer could not produce.
pub(super) fn stored_context(raw: Option<Value>) -> Result<Vec<ContextRef>> {
    let Some(raw) = raw else {
        return Ok(vec![]);
    };
    let refs: Vec<ContextRef> = serde_json::from_value(raw).map_err(|_| unavailable())?;
    if refs.is_empty() || !context_valid(&refs) {
        return Err(unavailable());
    }
    Ok(refs)
}
fn context_valid(refs: &[ContextRef]) -> bool {
    refs.len() <= MAX_CONTEXT_REFS
        && refs.iter().all(ContextRef::valid)
        && refs
            .iter()
            .enumerate()
            .all(|(i, a)| refs[..i].iter().all(|b| (b.kind, b.id) != (a.kind, a.id)))
}
impl Create {
    fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() || bytes.len() > 131072 {
            return Err(invalid());
        }
        let raw = aq_agent_api::parse(bytes).map_err(|_| invalid())?;
        let v: Self = serde_json::from_value(raw).map_err(|_| invalid())?;
        if v.operation_id == v.message_id
            || v.operation_id.is_nil()
            || v.message_id.is_nil()
            || positive(&v.expected_conversation_revision).is_err()
            || v.body.trim().is_empty()
            || v.body.len() > 16384
            || v.body.contains('\0')
            || !context_valid(&v.context)
            || !super::attachments::refs_valid(&v.attachments)
        {
            return Err(invalid());
        }
        Ok(v)
    }
    fn request_digest(&self) -> Result<String> {
        Ok(hash(
            &aq_agent_api::canonical(&json!(self)).map_err(|_| unavailable())?,
        ))
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Receipt {
    contract: String,
    conversation: Conversation,
    runtime: Runtime,
    operation_id: Uuid,
    message_id: Uuid,
    actor_id: Uuid,
    request_digest: String,
    body_digest: String,
    state: String,
    cursor: Option<String>,
    reason: Option<String>,
    host_delivery: String,
    agent_receipt: Option<Value>,
}
impl Receipt {
    fn decode(raw: Value, user: Uuid, conversation: Uuid, op: Uuid) -> Result<Self> {
        let r: Self = serde_json::from_value(raw).map_err(|_| unavailable())?;
        let digest_valid = |s: &str| {
            s.strip_prefix("sha256:").is_some_and(|h| {
                h.len() == 64
                    && h.bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
        };
        if r.contract != CONTRACT
            || r.conversation.id != conversation
            || r.conversation.revision != "1"
            || r.conversation.label.is_empty()
            || r.conversation.label.len() > 256
            || r.actor_id != user
            || r.operation_id != op
            || r.operation_id == r.message_id
            || r.operation_id.is_nil()
            || r.message_id.is_nil()
            || !digest_valid(&r.request_digest)
            || !digest_valid(&r.body_digest)
            || r.runtime != runtime()
            || r.host_delivery != "not_attempted"
            || r.agent_receipt.is_some()
        {
            return Err(unavailable());
        }
        match r.state.as_str() {
            "queued"
                if r.cursor.as_deref().is_some_and(|v| positive(v).is_ok())
                    && r.reason.is_none() => {}
            "not_queued"
                if r.cursor.is_none()
                    && matches!(
                        r.reason.as_deref(),
                        Some(
                            "conversation_revision_changed"
                                | "message_id_conflict"
                                | "attachment_unavailable"
                        )
                    ) => {}
            _ => return Err(unavailable()),
        }
        Ok(r)
    }
    fn outcome(self) -> Response {
        let code = if self.state == "queued" {
            axum::http::StatusCode::OK
        } else {
            axum::http::StatusCode::CONFLICT
        };
        (code, Json(self)).into_response()
    }
}
pub(super) async fn enqueue(
    State(state): State<AppState>,
    Extension(service): Extension<Arc<Service>>,
    RequireOwner(user): RequireOwner,
    Path(id): Path<Uuid>,
    bytes: Bytes,
) -> Result<Response> {
    let request = Create::parse(&bytes)?;
    let action = action::request(id, &request);
    let mut registry = crate::dashboard_api::actions::Registry::default();
    registry
        .register(action::SendMessage(service.clone()))
        .map_err(crate::dashboard_api::actions::existing::legacy_error)?;
    crate::dashboard_api::actions::submit_legacy(&state, &registry, &user, action)
        .await
        .map_err(crate::dashboard_api::actions::existing::legacy_error)?;
    let Json(receipt) = lookup(
        Extension(service),
        RequireOwner(user),
        Path((id, request.operation_id)),
    )
    .await
    .map_err(|error| match error {
        AppError::NotFound(_) => {
            AppError::IdentityUnavailable("supervisor enqueue operation is unresolved".into())
        }
        other => other,
    })?;
    if receipt.request_digest != request.request_digest()? {
        return Err(conflict());
    }
    Ok(receipt.outcome())
}
/// Seed historical fixtures through the real mailbox writer without requiring
/// the dashboard action journal introduced after their migration prefix.
#[cfg(test)]
pub(super) async fn enqueue_historical_fixture(
    state: &AppState,
    user: &User,
    id: Uuid,
    bytes: Bytes,
) -> Result<Response> {
    enqueue_domain(state, &Service::new(state), user, id, bytes)
        .await
        .map(Receipt::outcome)
}

async fn enqueue_domain(
    state: &AppState,
    service: &Service,
    user: &User,
    id: Uuid,
    bytes: Bytes,
) -> Result<Receipt> {
    let request = Create::parse(&bytes)?;
    if id.is_nil() {
        return Err(invalid());
    }
    let at = stamp(state)?;
    let mut tx = service.tx().await?;
    owner(&mut tx, user).await?;
    let conversation = definition(&mut tx, Some(id), true).await?;
    let sender = author(user.id);
    let sender_key = sender_key(&sender).map_err(|_| unavailable())?;
    let payload=digest(&json!({"kind":"global_supervisor_send","conversation_id":id,"request":&request,"sender":&sender})).map_err(|_|unavailable())?;
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
        let receipt = Receipt::decode(old, user.id, id, request.operation_id)?;
        tx.commit().await.map_err(db)?;
        return Ok(receipt);
    }
    // Attachment rows are locked here and bound only when the message lands;
    // their bytes count against the conversation's retained capacity.
    let at_ms = i64::try_from(at).map_err(|_| unavailable())?;
    let attached =
        super::attachments::claim(&mut tx, &request.attachments, user.id, id, at_ms).await?;
    let reserved = i64::try_from(request.body.len() + 4096)
        .ok()
        .and_then(|n| n.checked_add(attached.unwrap_or(0)))
        .ok_or_else(invalid)?;
    if !aq_supervisor_control::charge(&mut tx, id, reserved, MAX_OPERATIONS, MAX_BYTES)
        .await
        .map_err(db)?
    {
        return Err(AppError::Maintenance(
            "supervisor mailbox retention capacity exhausted".into(),
        ));
    }
    let mut result = Receipt {
        contract: CONTRACT.into(),
        conversation,
        runtime: runtime(),
        operation_id: request.operation_id,
        message_id: request.message_id,
        actor_id: user.id,
        request_digest: request.request_digest()?,
        body_digest: hash(request.body.as_bytes()),
        state: "not_queued".into(),
        cursor: None,
        reason: None,
        host_delivery: "not_attempted".into(),
        agent_receipt: None,
    };
    if result.conversation.revision != request.expected_conversation_revision {
        result.reason = Some("conversation_revision_changed".into());
    } else if attached.is_none() {
        result.reason = Some("attachment_unavailable".into());
    } else {
        let appended = append(
            &mut tx,
            Mail {
                message_id: request.message_id,
                thread_id: id,
                recipient: recipient(id),
                mailbox_key: key(id)?,
                body: &request.body,
            },
            &sender,
            &payload,
            at,
        )
        .await;
        match appended {
            Ok(cursor) => {
                if !request.context.is_empty() {
                    sqlx::query("INSERT INTO global_supervisor_message_context(message_id,refs) VALUES($1,$2)")
                        .bind(request.message_id)
                        .bind(json!(request.context))
                        .execute(&mut *tx)
                        .await
                        .map_err(db)?;
                }
                super::attachments::bind(&mut tx, &request.attachments, request.message_id).await?;
                result.state = "queued".into();
                result.cursor = Some(cursor.to_string());
            }
            Err(e) if e.0 == "id_conflict" => result.reason = Some("message_id_conflict".into()),
            Err(_) => return Err(unavailable()),
        }
    }
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
pub(super) async fn lookup(
    Extension(service): Extension<Arc<Service>>,
    RequireOwner(user): RequireOwner,
    Path((id, op)): Path<(Uuid, Uuid)>,
) -> Result<Json<Receipt>> {
    if id.is_nil() || op.is_nil() {
        return Err(invalid());
    }
    let mut tx = service.tx().await?;
    owner(&mut tx, &user).await?;
    definition(&mut tx, Some(id), false).await?;
    let sender_key = sender_key(&author(user.id)).map_err(|_| unavailable())?;
    let raw:Value=sqlx::query_scalar("SELECT response FROM mailbox_operations WHERE sender_key=$1 AND operation_id=$2 AND response->>'contract'=$3")
 .bind(sender_key).bind(op).bind(CONTRACT).fetch_optional(&mut *tx).await.map_err(db)?.ok_or_else(missing)?;
    let response = Receipt::decode(raw, user.id, id, op)?;
    tx.commit().await.map_err(db)?;
    Ok(Json(response))
}
