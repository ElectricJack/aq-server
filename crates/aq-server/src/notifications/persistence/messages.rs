use super::*;

async fn existing_message(
    tx: &mut Transaction<'_, Postgres>,
    message_id: Uuid,
    logical: &str,
) -> Result<Option<Value>> {
    let existing =
            sqlx::query("SELECT logical_digest,revision,external_state FROM notification_messages WHERE message_id=$1")
                .bind(message_id)
                .fetch_optional(&mut **tx)
                .await?;
    let Some(existing) = existing else {
        return Ok(None);
    };
    if existing.get::<String, _>("logical_digest") != logical {
        return Err(Error::Conflict);
    }
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT delivery_id FROM notification_deliveries WHERE message_id=$1 ORDER BY sequence",
    )
    .bind(message_id)
    .fetch_all(&mut **tx)
    .await?;
    let response = json!({"message_id":message_id,"revision":existing.get::<i64,_>("revision"),"external_state":existing.get::<String,_>("external_state"),"delivery_ids":ids});
    Ok(Some(response))
}

struct Publication {
    message: Message,
    deliveries: Vec<Delivery>,
}
impl Publication {
    async fn plan(
        tx: &mut Transaction<'_, Postgres>,
        message: Message,
        origin: &str,
    ) -> Result<Self> {
        let deliveries = Self::deliveries(tx, &message, origin).await?;
        Ok(Self {
            message,
            deliveries,
        })
    }
    async fn deliveries(
        tx: &mut Transaction<'_, Postgres>,
        message: &Message,
        origin: &str,
    ) -> Result<Vec<Delivery>> {
        let rows=sqlx::query("SELECT r.definition FROM notification_routes r JOIN notification_destinations d ON d.endpoint_key=r.endpoint_key AND d.endpoint_revision=r.endpoint_revision WHERE r.project_id=$1 AND r.workspace_id=$2 AND r.recipient_user_id=$3 AND r.enabled AND d.available AND NOT d.disabled ORDER BY r.route_id LIMIT 9")
            .bind(message.scope.project_id).bind(message.scope.workspace_id).bind(message.recipient_user_id).fetch_all(&mut **tx).await?;
        if rows.len() > 8 {
            return Err(Error::Overloaded);
        }
        let mut deliveries = Vec::new();
        for row in rows {
            let route: Route = serde_json::from_value(row.get("definition"))?;
            let payload = Payload::for_message(message, origin)?;
            let d = Delivery::queued(Uuid::new_v4(), message.clone(), route, payload)?;
            if serde_json::to_vec(&d)?.len() > 32768 {
                return Err(Error::Invalid);
            }
            deliveries.push(d);
        }
        Ok(deliveries)
    }
    async fn store(self, tx: &mut Transaction<'_, Postgres>, logical: &str) -> Result<Value> {
        let Self {
            message,
            deliveries,
        } = self;
        let now = message.created_at_ms;
        Reservation::Message {
            deliveries: deliveries.len(),
        }
        .reserve(tx)
        .await?;
        let external = if deliveries.is_empty() {
            "no_external_route"
        } else {
            "routed"
        };
        sqlx::query("INSERT INTO notification_messages(message_id,revision,project_id,workspace_id,recipient_user_id,definition,digest,external_state,created_at_ms,expires_at_ms,logical_digest) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)")
            .bind(message.message_id).bind(revision(message.revision)?).bind(message.scope.project_id).bind(message.scope.workspace_id).bind(message.recipient_user_id).bind(serde_json::to_value(&message)?).bind(digest(&message)?).bind(external).bind(now as i64).bind(revision(message.expires_at_ms)?).bind(logical).execute(&mut **tx).await?;
        for d in &deliveries {
            sqlx::query("INSERT INTO notification_deliveries(delivery_id,message_id,route_id,route_revision,endpoint_key,endpoint_revision,state,status,due_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7,'queued',$8)")
                .bind(d.delivery_id).bind(message.message_id).bind(d.route.route_id).bind(revision(d.route.revision)?).bind(&d.route.endpoint_key).bind(revision(d.route.endpoint_revision)?).bind(serde_json::to_value(d)?).bind(now as i64).execute(&mut **tx).await?;
        }
        let response = json!({"message_id":message.message_id,"revision":message.revision,"external_state":external,"delivery_ids":deliveries.iter().map(|d|d.delivery_id).collect::<Vec<_>>()});
        Ok(response)
    }
}

impl Service {
    pub async fn enqueue(&self, actor: Uuid, request: SendRequest) -> Result<Value> {
        if request.ttl_ms == 0 || request.ttl_ms > 604_800_000 {
            return Err(Error::Invalid);
        }
        let hash = digest(&("human-message/1", &request))?;
        let operation = Operation {
            actor,
            id: request.operation_id,
            hash: &hash,
        };
        let logical = digest(&(
            "human-message-identity/1",
            request.message_id,
            request.revision,
            &request.scope,
            request.recipient_user_id,
            &request.summary,
            request.ttl_ms,
        ))?;
        let mut tx = self.pool.begin().await?;
        lock_budget(&mut tx).await?;
        scope(&mut tx, &request.scope).await?;
        owner(&mut tx, actor).await?;
        recipient(&mut tx, request.recipient_user_id).await?;
        if let Some(response) = operation.replay(&mut tx).await? {
            return Ok(response);
        }
        let response = if let Some(existing) =
            existing_message(&mut tx, request.message_id, &logical).await?
        {
            Reservation::Operation.reserve(&mut tx).await?;
            existing
        } else {
            let message = request.into_message(self.now()?)?;
            let publication = Publication::plan(&mut tx, message, &self.public_origin).await?;
            publication.store(&mut tx, &logical).await?
        };
        operation.record(&mut tx, &response).await?;
        tx.commit().await?;
        Ok(response)
    }
}
