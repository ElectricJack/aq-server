use super::*;
pub(super) async fn reservation(
    tx: &mut Transaction<'_, Postgres>,
    a: &Actor,
    id: Uuid,
    write: bool,
) -> Result<Option<ReservationView>> {
    let sql = if write {
        "SELECT * FROM central_supervisor_reservations WHERE actor_id=$1 AND reservation_id=$2 FOR UPDATE"
    } else {
        "SELECT * FROM central_supervisor_reservations WHERE actor_id=$1 AND reservation_id=$2 FOR SHARE"
    };
    let Some(row) = sqlx::query(sql)
        .bind(a.id)
        .bind(id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(db)?
    else {
        return Ok(None);
    };
    let rows=sqlx::query("SELECT b.*,m.mailbox_key AS actual_key,m.cursor AS actual_cursor,m.payload_digest AS actual_digest,octet_length(m.body) AS body_bytes,central_global_mail_source_valid(m.message_id) AS valid FROM central_supervisor_mail_members b JOIN mailbox_messages m ON m.message_id=b.message_id WHERE b.reservation_id=$1 ORDER BY b.ordinal LIMIT 17 FOR SHARE OF m").bind(id).fetch_all(&mut **tx).await.map_err(db)?;
    let mut messages = Vec::new();
    let mut body_bytes = 0u64;
    let mailbox = key(a.conversation).map_err(|_| unavailable())?;
    for (index, m) in rows.into_iter().enumerate() {
        if index >= 16
            || m.get::<i16, _>("ordinal") != index as i16
            || m.get::<Uuid, _>("actor_id") != a.id
            || m.get::<String, _>("mailbox_key") != mailbox
            || m.get::<String, _>("actual_key") != mailbox
            || m.get::<i64, _>("cursor") != m.get::<i64, _>("actual_cursor")
            || m.get::<String, _>("payload_digest") != m.get::<String, _>("actual_digest")
            || !m.get::<bool, _>("valid")
        {
            return Err(unavailable());
        }
        body_bytes += u64::try_from(m.get::<i32, _>("body_bytes")).map_err(|_| unavailable())?;
        messages.push(MailRef {
            message_id: m.get("message_id"),
            cursor: u64::try_from(m.get::<i64, _>("cursor")).map_err(|_| unavailable())?,
            payload_digest: serde_json::from_value(json!(m.get::<String, _>("payload_digest")))
                .map_err(|_| unavailable())?,
        });
    }
    let batch = MailBatch {
        conversation_id: a.conversation,
        through_cursor: u64::try_from(row.get::<i64, _>("through_cursor"))
            .map_err(|_| unavailable())?,
        messages,
    };
    batch.validate().map_err(|_| unavailable())?;
    let digest: Digest = serde_json::from_value(json!(row.get::<String, _>("batch_digest")))
        .map_err(|_| unavailable())?;
    if batch.messages.len() != row.get::<i16, _>("message_count") as usize
        || body_bytes != row.get::<i64, _>("body_bytes") as u64
        || body_bytes > 65536
        || batch.digest().map_err(|_| unavailable())? != digest
    {
        return Err(unavailable());
    }
    let state = match row.get::<String, _>("state").as_str() {
        "awaiting_prepared_plan" => ReservationState::AwaitingPreparedPlan,
        "cancelled_before_plan" => ReservationState::CancelledBeforePlan,
        "consumed" => ReservationState::Consumed,
        _ => return Err(unavailable()),
    };
    let revision = row.get::<i64, _>("revision") as u64;
    let cancel_operation_id = row.get::<Option<Uuid>, _>("cancel_operation_id");
    if (state == ReservationState::Consumed && (revision != 2 || cancel_operation_id.is_some()))
        || (state == ReservationState::AwaitingPreparedPlan
            && (revision != 1 || cancel_operation_id.is_some()))
        || (state == ReservationState::CancelledBeforePlan
            && (revision != 2 || cancel_operation_id.is_none()))
    {
        return Err(unavailable());
    }
    Ok(Some(ReservationView {
        id,
        actor_id: a.id,
        conversation_id: a.conversation,
        revision,
        admission_actor_revision: row.get::<i64, _>("admission_actor_revision") as u64,
        reserve_operation_id: row.get("reserve_operation_id"),
        cancel_operation_id,
        policy_revision: row.get::<i64, _>("policy_revision") as u64,
        batch,
        batch_digest: digest,
        body_bytes,
        state,
        runtime: Unavailable::Unavailable,
        prepared_plan: (),
        session: (),
        execution_history: Unavailable::Unavailable,
        history_scope: ReservationOnly::ReservationOnly,
    }))
}
pub(super) async fn selection(
    tx: &mut Transaction<'_, Postgres>,
    a: &Actor,
    request: &PreviewRequest,
) -> Result<Preview> {
    positive(request.expected_actor_revision)?;
    cursor(request.scan_after)?;
    if request.expected_actor_revision != a.revision as u64 {
        return Err(conflict());
    }
    let mailbox = key(a.conversation).map_err(|_| unavailable())?;
    let sql_key: Option<String> =
        sqlx::query_scalar("SELECT encode(sha256(convert_to($1,'UTF8')),'hex')")
            .bind(format!(
                r#"{{"conversation_id":"{}","kind":"global_supervisor"}}"#,
                a.conversation
            ))
            .fetch_one(&mut **tx)
            .await
            .map_err(db)?;
    if sql_key.as_deref() != Some(&mailbox) {
        return Err(unavailable());
    }
    let head: Option<i64> =
        sqlx::query_scalar("SELECT next_cursor FROM mailbox_heads WHERE mailbox_key=$1 FOR SHARE")
            .bind(&mailbox)
            .fetch_optional(&mut **tx)
            .await
            .map_err(db)?;
    let metadata=sqlx::query("SELECT count(*) AS n,COALESCE(max(cursor),0) AS last FROM mailbox_messages WHERE mailbox_key=$1").bind(&mailbox).fetch_one(&mut **tx).await.map_err(db)?;
    let last: i64 = metadata.get("last");
    let count: i64 = metadata.get("n");
    if count != last
        || head.is_none() && last != 0
        || head.is_some_and(|head| last.checked_add(1) != Some(head))
    {
        return Err(unavailable());
    }
    let through = request
        .through_cursor
        .map(cursor)
        .transpose()?
        .unwrap_or(last);
    if through > last || request.scan_after > through as u64 {
        return Err(invalid());
    }
    let mut result = Preview {
        actor_id: a.id,
        conversation_id: a.conversation,
        actor_revision: a.revision as u64,
        policy_revision: a.policy_revision as u64,
        scan_after: request.scan_after,
        through_cursor: through as u64,
        scanned_through_cursor: request.scan_after,
        batch: None,
        body_bytes: 0,
        coverage: Coverage::Complete,
        has_more: false,
        status: PreviewStatus::NoEligibleMail,
        selection_digest: Digest::of(b"pending"),
        execution_history: Unavailable::Unavailable,
        history_scope: ReservationOnly::ReservationOnly,
    };
    if !a.policy.enabled {
        result.status = PreviewStatus::Disabled;
    } else if let Some(id) = a.current {
        let r = reservation(tx, a, id, false)
            .await?
            .ok_or_else(unavailable)?;
        if r.state != ReservationState::AwaitingPreparedPlan {
            return Err(unavailable());
        }
        result.status = PreviewStatus::ReservationOutstanding;
    } else {
        let active:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM central_supervisor_reservations WHERE actor_id=$1 AND state='awaiting_prepared_plan')").bind(a.id).fetch_one(&mut **tx).await.map_err(db)?;
        if active {
            return Err(unavailable());
        }
        let rows=sqlx::query("SELECT m.message_id,m.cursor,m.payload_digest,octet_length(m.body) AS body_bytes,central_global_mail_source_valid(m.message_id) AS valid,EXISTS(SELECT 1 FROM central_supervisor_mail_members b JOIN central_supervisor_reservations r ON r.reservation_id=b.reservation_id WHERE b.actor_id=$4 AND b.message_id=m.message_id AND r.state='consumed') AS consumed FROM mailbox_messages m WHERE m.mailbox_key=$1 AND m.cursor>$2 AND m.cursor<=$3 ORDER BY m.cursor LIMIT 129 FOR SHARE OF m").bind(&mailbox).bind(cursor(request.scan_after)?).bind(through).bind(a.id).fetch_all(&mut **tx).await.map_err(db)?;
        let mut messages = Vec::new();
        for (index, row) in rows.into_iter().enumerate() {
            if index >= MAX_SCAN {
                break;
            }
            if row.get::<bool, _>("consumed") {
                result.scanned_through_cursor =
                    u64::try_from(row.get::<i64, _>("cursor")).map_err(|_| unavailable())?;
                continue;
            }
            let bytes =
                u64::try_from(row.get::<i32, _>("body_bytes")).map_err(|_| unavailable())?;
            if messages.len() >= 16 || result.body_bytes + bytes > 65536 {
                break;
            }
            if bytes == 0 || bytes > 16384 || !row.get::<bool, _>("valid") {
                return Err(unavailable());
            }
            let message = MailRef {
                message_id: row.get("message_id"),
                cursor: u64::try_from(row.get::<i64, _>("cursor")).map_err(|_| unavailable())?,
                payload_digest: serde_json::from_value(json!(
                    row.get::<String, _>("payload_digest")
                ))
                .map_err(|_| unavailable())?,
            };
            let aliases:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM central_supervisor_mail_members WHERE actor_id=$1 AND (message_id=$2 OR (mailbox_key=$3 AND cursor=$4)) AND (message_id<>$2 OR mailbox_key<>$3 OR cursor<>$4 OR payload_digest<>$5))").bind(a.id).bind(message.message_id).bind(&mailbox).bind(positive(message.cursor)?).bind(message.payload_digest.as_str()).fetch_one(&mut **tx).await.map_err(db)?;
            if aliases {
                return Err(unavailable());
            }
            result.scanned_through_cursor = message.cursor;
            result.body_bytes += bytes;
            messages.push(message);
        }
        if !messages.is_empty() {
            let batch = MailBatch {
                conversation_id: a.conversation,
                through_cursor: through as u64,
                messages,
            };
            batch.validate().map_err(|_| unavailable())?;
            result.batch = Some(batch);
            result.status = PreviewStatus::Ready;
        }
    }
    result.has_more = result.scanned_through_cursor < result.through_cursor;
    if result.has_more {
        result.coverage = Coverage::Partial;
        if result.status == PreviewStatus::NoEligibleMail {
            result.status = PreviewStatus::PartialScan;
        }
    }
    let mut value = json!(result);
    value
        .as_object_mut()
        .ok_or_else(unavailable)?
        .remove("selection_digest");
    result.selection_digest = hash(&value)?;
    Ok(result)
}
impl ReservationOwner {
    pub(super) async fn preview(&self, user: &User, request: PreviewRequest) -> Result<Preview> {
        let mut tx = self.service.tx().await?;
        let a = bound_actor(&mut tx, user, request.actor_id, false).await?;
        let result = selection(&mut tx, &a, &request).await?;
        tx.commit().await.map_err(db)?;
        Ok(result)
    }
    pub(super) async fn inspect(&self, user: &User, id: Uuid) -> Result<ActorView> {
        let mut tx = self.service.tx().await?;
        let a = bound_actor(&mut tx, user, id, false).await?;
        let current = match a.current {
            Some(id) => Some(
                reservation(&mut tx, &a, id, false)
                    .await?
                    .ok_or_else(unavailable)?,
            ),
            None => None,
        };
        let result = ActorView {
            actor_id: a.id,
            conversation_id: a.conversation,
            revision: a.revision as u64,
            policy_revision: a.policy_revision as u64,
            policy: a.policy,
            policy_digest: a.policy_digest,
            retained_operations: a.operations as u64,
            retained_bytes: a.bytes as u64,
            current_reservation: current,
            execution_history: Unavailable::Unavailable,
            history_scope: ReservationOnly::ReservationOnly,
        };
        tx.commit().await.map_err(db)?;
        Ok(result)
    }
    pub(super) async fn lookup(&self, user: &User, id: Uuid, operation: Uuid) -> Result<Lookup> {
        uuid(operation)?;
        let mut tx = self.service.tx().await?;
        let a = bound_actor(&mut tx, user, id, false).await?;
        let row = sqlx::query(
            "SELECT * FROM central_supervisor_operations WHERE actor_id=$1 AND operation_id=$2",
        )
        .bind(id)
        .bind(operation)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db)?
        .ok_or_else(missing)?;
        let result = writes::retained(&mut tx, &a, row).await?;
        tx.commit().await.map_err(db)?;
        Ok(result)
    }
    pub(super) async fn read_history(
        &self,
        user: &User,
        id: Uuid,
        before: Option<u64>,
        limit: u8,
    ) -> Result<History> {
        if !(1..=16).contains(&limit) {
            return Err(invalid());
        }
        let before = before.map(positive).transpose()?.unwrap_or(i64::MAX);
        let mut tx = self.service.tx().await?;
        let a = bound_actor(&mut tx, user, id, false).await?;
        let ids:Vec<Uuid>=sqlx::query_scalar("SELECT reservation_id FROM central_supervisor_reservations WHERE actor_id=$1 AND admission_actor_revision<$2 ORDER BY admission_actor_revision DESC LIMIT $3").bind(id).bind(before).bind(i64::from(limit)+1).fetch_all(&mut *tx).await.map_err(db)?;
        let mut result = History {
            actor_id: id,
            items: vec![],
            next_before: None,
            has_more: ids.len() > usize::from(limit),
            execution_history: Unavailable::Unavailable,
            history_scope: ReservationOnly::ReservationOnly,
        };
        for id in ids.into_iter().take(usize::from(limit)) {
            let item = reservation(&mut tx, &a, id, false)
                .await?
                .ok_or_else(unavailable)?;
            let mut candidate = result.clone();
            candidate.items.push(item);
            if canonical(&candidate)?.len() + 256 > 65536 {
                result.has_more = true;
                break;
            }
            result = candidate;
        }
        if result.has_more {
            result.next_before = Some(
                result
                    .items
                    .last()
                    .ok_or_else(unavailable)?
                    .admission_actor_revision,
            );
        }
        tx.commit().await.map_err(db)?;
        Ok(result)
    }
}
