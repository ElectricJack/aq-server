use super::*;
fn receipt(
    a: &Actor,
    user: &User,
    operation: Uuid,
    digest: Digest,
    disposition: Disposition,
    reason: Option<Reason>,
    reservation: Option<ReservationView>,
) -> Receipt {
    Receipt {
        contract: CONTRACT.into(),
        actor_id: a.id,
        conversation_id: a.conversation,
        operation_id: operation,
        initiating_user_id: user.id,
        request_digest: digest,
        actor_revision: a.revision as u64 + 1,
        policy_revision: a.policy_revision as u64,
        disposition,
        reason,
        reservation,
    }
}
fn result_bytes(receipt: &Receipt) -> Result<String> {
    let bytes = canonical(receipt)?;
    if bytes.len() > 65536 {
        return Err(unavailable());
    }
    String::from_utf8(bytes).map_err(|_| unavailable())
}
fn charge(request: &str, result: &str, receipt: &Receipt) -> Result<i64> {
    let extra = if receipt.disposition == Disposition::Reserved {
        let n = receipt
            .reservation
            .as_ref()
            .ok_or_else(unavailable)?
            .batch
            .messages
            .len();
        2048 + n * 256
    } else {
        0
    };
    i64::try_from(request.len() + result.len() + 8192 + extra).map_err(|_| capacity())
}
async fn insert_operation(
    tx: &mut Transaction<'_, Postgres>,
    kind: &str,
    user: &User,
    request: &str,
    result: &str,
    receipt: &Receipt,
    charge: i64,
) -> Result<()> {
    sqlx::query("INSERT INTO central_supervisor_operations(actor_id,operation_id,kind,actor_revision,initiating_user_id,request_bytes,request_digest,result_bytes,charged_bytes) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)").bind(receipt.actor_id).bind(receipt.operation_id).bind(kind).bind(positive(receipt.actor_revision)?).bind(user.id).bind(request).bind(receipt.request_digest.as_str()).bind(result).bind(charge).execute(&mut **tx).await.map_err(db)?;
    Ok(())
}
pub(super) struct Mutation<'a> {
    pub(super) kind: &'a str,
    pub(super) request: &'a str,
    pub(super) receipt: &'a Receipt,
}
impl Mutation<'_> {
    pub(super) async fn finish(
        self,
        tx: &mut Transaction<'_, Postgres>,
        a: &Actor,
        user: &User,
        current: Option<Uuid>,
        policy: Option<&LocalPolicy>,
    ) -> Result<()> {
        let Self {
            kind,
            request,
            receipt,
        } = self;
        let result = result_bytes(receipt)?;
        let charged = charge(request, &result, receipt)?;
        if charged > CONTROL_BYTES {
            return Err(capacity());
        }
        let held = i64::from(current.is_some());
        let next_bytes = a.bytes.checked_add(charged).ok_or_else(capacity)?;
        if a.operations + 1 > MAX_OPS - held || next_bytes > MAX_BYTES - held * CONTROL_BYTES {
            return Err(capacity());
        }
        let updated = if let Some(policy) = policy {
            let bytes = canonical(policy)?;
            if bytes.len() > 4096 {
                return Err(invalid());
            }
            sqlx::query("UPDATE central_supervisor_actors SET revision=$2,policy_revision=$3,policy_bytes=$4,policy_digest=$5,approved_by=$6,approved_identity=$7,current_reservation=$8,retained_operations=retained_operations+1,retained_bytes=$9 WHERE actor_id=$1 AND revision=$10")
     .bind(a.id).bind(positive(receipt.actor_revision)?).bind(positive(receipt.policy_revision)?).bind(String::from_utf8(bytes.clone()).map_err(|_|invalid())?).bind(Digest::of(&bytes).as_str()).bind(user.id).bind(identity(user)).bind(current).bind(next_bytes).bind(a.revision).execute(&mut **tx).await.map_err(db)?.rows_affected()
        } else {
            sqlx::query("UPDATE central_supervisor_actors SET revision=$2,current_reservation=$3,retained_operations=retained_operations+1,retained_bytes=$4 WHERE actor_id=$1 AND revision=$5").bind(a.id).bind(positive(receipt.actor_revision)?).bind(current).bind(next_bytes).bind(a.revision).execute(&mut **tx).await.map_err(db)?.rows_affected()
        };
        if updated != 1 {
            return Err(conflict());
        }
        insert_operation(tx, kind, user, request, &result, receipt, charged).await?;
        authorize(tx, user).await?;
        Ok(())
    }
}
async fn replay(
    tx: &mut Transaction<'_, Postgres>,
    a: &Actor,
    operation: Uuid,
    request: &str,
) -> Result<Option<Receipt>> {
    let old = sqlx::query(
        "SELECT * FROM central_supervisor_operations WHERE actor_id=$1 AND operation_id=$2",
    )
    .bind(a.id)
    .bind(operation)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db)?;
    let Some(row) = old else { return Ok(None) };
    if row.get::<String, _>("request_bytes") != request {
        return Err(conflict());
    }
    Ok(Some(retained(tx, a, row).await?.receipt))
}
fn request_reservation(
    a: &Actor,
    kind: &str,
    operation: Uuid,
    req: &Value,
) -> Result<Option<Uuid>> {
    Ok(match kind {
        "configure" => {
            let r: Configure = serde_json::from_value(req.clone()).map_err(|_| unavailable())?;
            r.policy.validate().map_err(|_| unavailable())?;
            if r.operation_id != operation
                || r.actor_id != a.id
                || r.policy.actor_id != a.id
                || r.policy.conversation_id != a.conversation
            {
                return Err(unavailable());
            }
            None
        }
        "reserve" => {
            let r: Reserve = serde_json::from_value(req.clone()).map_err(|_| unavailable())?;
            if r.operation_id != operation || r.actor_id != a.id {
                return Err(unavailable());
            }
            Some(r.reservation_id)
        }
        "cancel" => {
            let r: Cancel = serde_json::from_value(req.clone()).map_err(|_| unavailable())?;
            if r.operation_id != operation || r.actor_id != a.id {
                return Err(unavailable());
            }
            Some(r.reservation_id)
        }
        _ => return Err(unavailable()),
    })
}
pub(super) async fn retained(
    tx: &mut Transaction<'_, Postgres>,
    a: &Actor,
    row: sqlx::postgres::PgRow,
) -> Result<Lookup> {
    let request: String = row.get("request_bytes");
    let result: String = row.get("result_bytes");
    let payload = aq_agent_api::parse(request.as_bytes()).map_err(|_| unavailable())?;
    let raw = aq_agent_api::parse(result.as_bytes()).map_err(|_| unavailable())?;
    let receipt: Receipt = serde_json::from_value(raw.clone()).map_err(|_| unavailable())?;
    let kind: String = row.get("kind");
    let operation: Uuid = row.get("operation_id");
    if canonical(&payload)? != request.as_bytes()
        || canonical(&raw)? != result.as_bytes()
        || receipt.contract != CONTRACT
        || receipt.actor_id != a.id
        || receipt.conversation_id != a.conversation
        || receipt.operation_id != operation
        || receipt.initiating_user_id != row.get::<Uuid, _>("initiating_user_id")
        || receipt.request_digest != Digest::of(request.as_bytes())
        || receipt.request_digest.as_str() != row.get::<String, _>("request_digest")
        || receipt.actor_revision != row.get::<i64, _>("actor_revision") as u64
        || receipt.actor_revision == 0
        || receipt.actor_revision > a.revision as u64
        || receipt.policy_revision == 0
        || receipt.policy_revision > a.policy_revision as u64
        || payload["contract"] != CONTRACT
        || payload["kind"] != kind
        || payload["owner"]["id"] != json!(receipt.initiating_user_id)
        || payload["owner"]["role"] != "owner"
        || payload.as_object().is_none_or(|o| o.len() != 4)
    {
        return Err(unavailable());
    }
    let request_reservation = request_reservation(a, &kind, operation, &payload["request"])?;
    let success = matches!(
        receipt.disposition,
        Disposition::Configured | Disposition::Reserved | Disposition::CancelledBeforePlan
    );
    if success != receipt.reason.is_none()
        || !matches!(
            (kind.as_str(), receipt.disposition),
            (
                "configure",
                Disposition::Configured | Disposition::NotConfigured
            ) | ("reserve", Disposition::Reserved | Disposition::NotReserved)
                | (
                    "cancel",
                    Disposition::CancelledBeforePlan | Disposition::NotCancelled
                )
        )
    {
        return Err(unavailable());
    }
    let mut current = None;
    if let Some(saved) = &receipt.reservation {
        if request_reservation != Some(saved.id) {
            return Err(unavailable());
        }
        let actual = reads::reservation(tx, a, saved.id, false)
            .await?
            .ok_or_else(unavailable)?;
        let mut expected = actual.clone();
        if receipt.disposition == Disposition::Reserved {
            expected.revision = 1;
            expected.state = ReservationState::AwaitingPreparedPlan;
            expected.cancel_operation_id = None;
            if expected.reserve_operation_id != operation
                || expected.admission_actor_revision != receipt.actor_revision
            {
                return Err(unavailable());
            }
        } else if receipt.disposition == Disposition::CancelledBeforePlan {
            if actual.cancel_operation_id != Some(operation)
                || actual.state != ReservationState::CancelledBeforePlan
            {
                return Err(unavailable());
            }
        } else {
            return Err(unavailable());
        }
        if *saved != expected {
            return Err(unavailable());
        }
        current = Some(actual);
    } else if matches!(
        receipt.disposition,
        Disposition::Reserved | Disposition::CancelledBeforePlan
    ) {
        return Err(unavailable());
    }
    Ok(Lookup {
        receipt,
        current_reservation: current,
    })
}
enum ReserveSelection {
    Selected(Preview),
    Refused(Reason),
}
async fn reserve_selection(
    tx: &mut Transaction<'_, Postgres>,
    a: &Actor,
    r: &Reserve,
) -> Result<ReserveSelection> {
    use ReserveSelection::{Refused, Selected};
    if r.expected_actor_revision != a.revision as u64
        || r.expected_policy_revision != a.policy_revision as u64
    {
        return Ok(Refused(Reason::RevisionChanged));
    }
    if !a.policy.enabled {
        return Ok(Refused(Reason::PolicyDisabled));
    }
    if a.current.is_some() {
        return Ok(Refused(Reason::ReservationOutstanding));
    }
    let preview = reads::selection(
        tx,
        a,
        &PreviewRequest {
            actor_id: a.id,
            expected_actor_revision: r.expected_actor_revision,
            scan_after: r.scan_after,
            through_cursor: Some(r.through_cursor),
        },
    )
    .await?;
    if preview.selection_digest != r.selection_digest {
        return Ok(Refused(Reason::SelectionChanged));
    }
    if preview.batch.is_none() {
        return Ok(Refused(Reason::NoEligibleMail));
    }
    let used: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM central_supervisor_reservations WHERE reservation_id=$1)",
    )
    .bind(r.reservation_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(db)?;
    Ok(if used {
        Refused(Reason::ReservationIdConflict)
    } else {
        Selected(preview)
    })
}
impl ReservationOwner {
    pub(super) async fn configure(&self, user: &User, bytes: &[u8]) -> Result<Receipt> {
        let r: Configure = parse(bytes)?;
        uuid(r.actor_id)?;
        uuid(r.operation_id)?;
        cursor(r.expected_actor_revision)?;
        r.policy.validate()?;
        if r.policy.actor_id != r.actor_id {
            return Err(invalid());
        }
        let (request, digest) = command("configure", user, &r)?;
        let mut tx = self.service.tx().await?;
        authorize(&mut tx, user).await?;
        // Initial absent-actor registration takes UPDATE immediately; no lock upgrade.
        let c = definition(&mut tx, Some(r.policy.conversation_id), true).await?;
        if let Some(a) = actor(&mut tx, r.actor_id, true).await? {
            if a.conversation != c.id {
                return Err(conflict());
            }
            if let Some(old) = replay(&mut tx, &a, r.operation_id, &request).await? {
                tx.commit().await.map_err(db)?;
                return Ok(old);
            }
            let matches = r.expected_actor_revision == a.revision as u64;
            let mut result = receipt(
                &a,
                user,
                r.operation_id,
                digest,
                Disposition::NotConfigured,
                Some(Reason::RevisionChanged),
                None,
            );
            if matches {
                result.disposition = Disposition::Configured;
                result.reason = None;
                result.policy_revision += 1;
            }
            Mutation {
                kind: "configure",
                request: &request,
                receipt: &result,
            }
            .finish(
                &mut tx,
                &a,
                user,
                a.current,
                if matches { Some(&r.policy) } else { None },
            )
            .await?;
            tx.commit().await.map_err(db)?;
            return Ok(result);
        }
        if r.expected_actor_revision != 0 {
            return Err(conflict());
        }
        let other: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM central_supervisor_actors WHERE conversation_id=$1)",
        )
        .bind(c.id)
        .fetch_one(&mut *tx)
        .await
        .map_err(db)?;
        if other {
            return Err(conflict());
        }
        let policy = canonical(&r.policy)?;
        if policy.len() > 4096 {
            return Err(invalid());
        }
        let result = Receipt {
            contract: CONTRACT.into(),
            actor_id: r.actor_id,
            conversation_id: c.id,
            operation_id: r.operation_id,
            initiating_user_id: user.id,
            request_digest: digest,
            actor_revision: 1,
            policy_revision: 1,
            disposition: Disposition::Configured,
            reason: None,
            reservation: None,
        };
        let raw = result_bytes(&result)?;
        let charged = charge(&request, &raw, &result)?;
        sqlx::query("INSERT INTO central_supervisor_actors(actor_id,conversation_id,revision,policy_revision,policy_bytes,policy_digest,approved_by,approved_identity,retained_operations,retained_bytes) VALUES($1,$2,1,1,$3,$4,$5,$6,1,$7)").bind(r.actor_id).bind(c.id).bind(String::from_utf8(policy.clone()).map_err(|_|invalid())?).bind(Digest::of(&policy).as_str()).bind(user.id).bind(identity(user)).bind(charged).execute(&mut *tx).await.map_err(db)?;
        insert_operation(&mut tx, "configure", user, &request, &raw, &result, charged).await?;
        authorize(&mut tx, user).await?;
        tx.commit().await.map_err(db)?;
        Ok(result)
    }
    pub(super) async fn reserve(&self, user: &User, bytes: &[u8]) -> Result<Receipt> {
        let r: Reserve = parse(bytes)?;
        uuid(r.operation_id)?;
        uuid(r.reservation_id)?;
        if r.operation_id == r.reservation_id {
            return Err(invalid());
        }
        positive(r.expected_actor_revision)?;
        positive(r.expected_policy_revision)?;
        cursor(r.scan_after)?;
        cursor(r.through_cursor)?;
        let (request, digest) = command("reserve", user, &r)?;
        let mut tx = self.service.tx().await?;
        let a = bound_actor(&mut tx, user, r.actor_id, true).await?;
        if let Some(old) = replay(&mut tx, &a, r.operation_id, &request).await? {
            tx.commit().await.map_err(db)?;
            return Ok(old);
        }
        let selection = reserve_selection(&mut tx, &a, &r).await?;
        let reason = match &selection {
            ReserveSelection::Selected(_) => None,
            ReserveSelection::Refused(reason) => Some(*reason),
        };
        let mut result = receipt(
            &a,
            user,
            r.operation_id,
            digest,
            Disposition::NotReserved,
            reason,
            None,
        );
        let mut current = a.current;
        if let ReserveSelection::Selected(preview) = selection {
            let batch = preview.batch.ok_or_else(unavailable)?;
            let batch_digest = batch.digest().map_err(|_| unavailable())?;
            sqlx::query("INSERT INTO central_supervisor_reservations(reservation_id,actor_id,reserve_operation_id,admission_actor_revision,policy_revision,revision,state,through_cursor,batch_digest,message_count,body_bytes) VALUES($1,$2,$3,$4,$5,1,'awaiting_prepared_plan',$6,$7,$8,$9)").bind(r.reservation_id).bind(a.id).bind(r.operation_id).bind(positive(result.actor_revision)?).bind(a.policy_revision).bind(positive(batch.through_cursor)?).bind(batch_digest.as_str()).bind(batch.messages.len() as i16).bind(positive(preview.body_bytes)?).execute(&mut *tx).await.map_err(db)?;
            let mailbox = key(a.conversation).map_err(|_| unavailable())?;
            for (index, m) in batch.messages.iter().enumerate() {
                sqlx::query("INSERT INTO central_supervisor_mail_members(actor_id,reservation_id,ordinal,message_id,mailbox_key,cursor,payload_digest) VALUES($1,$2,$3,$4,$5,$6,$7)").bind(a.id).bind(r.reservation_id).bind(index as i16).bind(m.message_id).bind(&mailbox).bind(positive(m.cursor)?).bind(m.payload_digest.as_str()).execute(&mut *tx).await.map_err(db)?;
            }
            result.disposition = Disposition::Reserved;
            result.reservation = Some(ReservationView {
                id: r.reservation_id,
                actor_id: a.id,
                conversation_id: a.conversation,
                revision: 1,
                admission_actor_revision: result.actor_revision,
                reserve_operation_id: r.operation_id,
                cancel_operation_id: None,
                policy_revision: a.policy_revision as u64,
                batch,
                batch_digest,
                body_bytes: preview.body_bytes,
                state: ReservationState::AwaitingPreparedPlan,
                runtime: Unavailable::Unavailable,
                prepared_plan: (),
                session: (),
                execution_history: Unavailable::Unavailable,
                history_scope: ReservationOnly::ReservationOnly,
            });
            current = Some(r.reservation_id);
        }
        Mutation {
            kind: "reserve",
            request: &request,
            receipt: &result,
        }
        .finish(&mut tx, &a, user, current, None)
        .await?;
        tx.commit().await.map_err(db)?;
        Ok(result)
    }
    pub(super) async fn cancel(&self, user: &User, bytes: &[u8]) -> Result<Receipt> {
        let r: Cancel = parse(bytes)?;
        uuid(r.operation_id)?;
        uuid(r.reservation_id)?;
        if r.operation_id == r.reservation_id {
            return Err(invalid());
        }
        positive(r.expected_reservation_revision)?;
        let (request, digest) = command("cancel", user, &r)?;
        let mut tx = self.service.tx().await?;
        let a = bound_actor(&mut tx, user, r.actor_id, true).await?;
        if let Some(old) = replay(&mut tx, &a, r.operation_id, &request).await? {
            tx.commit().await.map_err(db)?;
            return Ok(old);
        }
        let current = reads::reservation(&mut tx, &a, r.reservation_id, true).await?;
        let reason = if a.current != Some(r.reservation_id)
            || current
                .as_ref()
                .is_none_or(|c| c.state != ReservationState::AwaitingPreparedPlan)
        {
            Some(Reason::ReservationNotCurrent)
        } else if current
            .as_ref()
            .is_none_or(|c| c.revision != r.expected_reservation_revision)
        {
            Some(Reason::RevisionChanged)
        } else {
            None
        };
        let mut result = receipt(
            &a,
            user,
            r.operation_id,
            digest,
            Disposition::NotCancelled,
            reason,
            None,
        );
        let mut next = a.current;
        if result.reason.is_none() {
            let mut saved = current.ok_or_else(unavailable)?;
            saved.revision = 2;
            saved.state = ReservationState::CancelledBeforePlan;
            saved.cancel_operation_id = Some(r.operation_id);
            let n=sqlx::query("UPDATE central_supervisor_reservations SET state='cancelled_before_plan',revision=2,cancel_operation_id=$3,cancel_actor_revision=$4 WHERE actor_id=$1 AND reservation_id=$2 AND revision=1 AND state='awaiting_prepared_plan'").bind(a.id).bind(r.reservation_id).bind(r.operation_id).bind(positive(result.actor_revision)?).execute(&mut *tx).await.map_err(db)?.rows_affected();
            if n != 1 {
                return Err(conflict());
            }
            result.disposition = Disposition::CancelledBeforePlan;
            result.reservation = Some(saved);
            next = None;
        }
        Mutation {
            kind: "cancel",
            request: &request,
            receipt: &result,
        }
        .finish(&mut tx, &a, user, next, None)
        .await?;
        tx.commit().await.map_err(db)?;
        Ok(result)
    }
}
#[cfg(test)]
pub(super) async fn disable_for_test(
    tx: &mut Transaction<'_, Postgres>,
    a: &Actor,
    user: &User,
) -> Result<()> {
    let mut policy = a.policy.clone();
    policy.enabled = false;
    let r = Configure {
        operation_id: Uuid::new_v4(),
        actor_id: a.id,
        expected_actor_revision: a.revision as u64,
        policy,
    };
    let (raw, digest) = command("configure", user, &r)?;
    let mut result = receipt(
        a,
        user,
        r.operation_id,
        digest,
        Disposition::Configured,
        None,
        None,
    );
    result.policy_revision += 1;
    Mutation {
        kind: "configure",
        request: &raw,
        receipt: &result,
    }
    .finish(tx, a, user, a.current, Some(&r.policy))
    .await
}
