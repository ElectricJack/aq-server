use super::*;

impl Service {
    pub async fn outbox(
        &self,
        owner: &RuntimeOwner,
        after: i64,
        limit: u32,
    ) -> Result<Vec<(i64, Uuid, EventEnvelope)>> {
        let (page, undecryptable) = self.outbox_page(owner, after, limit).await?;
        if undecryptable.is_some() {
            return Err(Error::Unknown);
        }
        Ok(page.into_iter().map(|(s, id, e, _)| (s, id, e)).collect())
    }
    /// [`Self::outbox`], each row with whether the engine accepted it (its
    /// receipt is recorded). A queued event that cannot be decrypted is not
    /// held (fixing the keyring recovers it): the page ends before it, with
    /// the reason naming the row and its key id.
    pub(in crate::playbooks) async fn outbox_page(
        &self,
        owner: &RuntimeOwner,
        after: i64,
        limit: u32,
    ) -> Result<(Vec<(i64, Uuid, EventEnvelope, bool)>, Option<String>)> {
        if after < 0 || !(1..=aq_role_settings::driver::OUTBOX_EVENTS_MAX).contains(&limit) {
            return Err(Error::Invalid);
        }
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        Self::owner(&mut tx, self.namespace.owner_user_id).await?;
        let rows=sqlx::query("SELECT * FROM playbook_outbox WHERE namespace_id=$1 AND generation=$2 AND sequence>$3 AND (status='pending' OR (status='engine_accepted' AND NOT fanout_complete)) ORDER BY sequence LIMIT $4").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(after).bind(limit as i64).fetch_all(&mut *tx).await?;
        let (mut result, mut undecryptable) = (Vec::new(), None);
        for row in rows {
            let id: Uuid = row.get("event_id");
            let digest: String = row.get("event_digest");
            let sealed = Sealed {
                version: row.get::<i32, _>("sealed_version") as u32,
                key_id: row.get("key_id"),
                nonce: row.get("nonce"),
                ciphertext: row.get("ciphertext"),
            };
            let aad = self.aad("aq.playbooks.outbox", id, &digest)?;
            let producer: String = row.get("producer");
            let Ok(bytes) = self.keys.open(&aad, &sealed) else {
                let key = &sealed.key_id;
                undecryptable = Some(format!(
                    "cannot decrypt queued event {id} (key id {key}): fix the keyring, or hold the event"
                ));
                break;
            };
            let plugin_event: Option<String> = row.get("plugin_event_id");
            let event_id = plugin_event
                .as_deref()
                .map_or_else(|| id.to_string(), str::to_owned);
            let event = (serde_json::from_slice::<EventEnvelope>(&bytes).ok()).filter(|event| {
                crate::playbooks::domain::digest(event).is_ok_and(|d| d == digest)
                    && event.event_id == event_id
                    && event.producer == producer
                    && if plugin_event.is_some() {
                        // Sealed only after validation against the installed contract.
                        // It may have been upgraded since this row was committed.
                        extension_capability(&producer)
                            .is_some_and(|name| command_plugin(&event.contract.name) == Some(name))
                    } else {
                        aq_playbook_host::model::validate_event(event, self.registry()).is_ok()
                    }
            });
            let Some(event) = event else {
                // A row whose event does not match its record is held, not
                // retried forever.
                sqlx::query("UPDATE playbook_outbox SET status='unknown',held_reason='invalid' WHERE namespace_id=$1 AND generation=$2 AND event_id=$3").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(id).execute(&mut *tx).await?;
                continue;
            };
            let accepted = row.get::<Option<Value>, _>("engine_receipt").is_some();
            result.push((row.get("sequence"), id, event, accepted));
        }
        tx.commit().await?;
        Ok((result, undecryptable))
    }
    pub async fn acknowledge_event(
        &self,
        owner: &RuntimeOwner,
        id: Uuid,
        receipt: aq_playbook_host::runner::EventDisposition,
        fanout_complete: bool,
    ) -> Result<()> {
        if receipt.destinations > 10000 {
            return Err(Error::Conflict);
        }
        let stable = json!({"key":receipt.key,"destinations":receipt.destinations});
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        let rows=sqlx::query("SELECT producer,engine_receipt,plugin_event_id FROM playbook_outbox WHERE namespace_id=$1 AND generation=$2 AND event_id=$3 FOR UPDATE").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(id).fetch_all(&mut *tx).await?;
        let row = rows
            .into_iter()
            .find(|row| {
                let producer: String = row.get("producer");
                let event_id: Option<String> = row.get("plugin_event_id");
                digest(&(&producer, 0_u64, event_id.unwrap_or_else(|| id.to_string())))
                    .ok()
                    .is_some_and(|key| stable["key"] == key)
            })
            .ok_or(Error::Conflict)?;
        let producer: String = row.get("producer");
        if extension_capability(&producer).is_none()
            && !self
                .registry()
                .events
                .values()
                .any(|c| c.trusted_producers.contains(&producer))
        {
            return Err(Error::Conflict);
        }
        let old: Option<Value> = row.get("engine_receipt");
        if old.is_some_and(|v| {
            v["key"] != stable["key"] || v["destinations"] != stable["destinations"]
        }) {
            return Err(Error::Conflict);
        }
        sqlx::query("UPDATE playbook_outbox SET status='engine_accepted',engine_receipt=COALESCE(engine_receipt,$4),fanout_complete=fanout_complete OR $5 WHERE namespace_id=$1 AND generation=$2 AND event_id=$3 AND producer=$6").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(id).bind(stable).bind(fanout_complete).bind(&producer).execute(&mut *tx).await?;
        if producer == crate::playbooks::review_domain::GRAPH_PRODUCER {
            // The engine has durably accepted this exact event key; only now is
            // the graph outbox row delivered. The owner route keeps the row.
            sqlx::query("UPDATE agent_graph_outbox SET delivered=true WHERE event_id=$1")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        if producer == crate::playbooks::review_domain::SETTLEMENT_PRODUCER {
            sqlx::query("SELECT set_config('aq.settlement_owner','aq.settlement-owner.1',true)")
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE profiled_task_completions SET outbox_acknowledged=true WHERE operation_id=$1 AND state='confirmed' AND NOT outbox_acknowledged")
                .bind(id).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }
    /// Hold pending work with its reason (an outbox event, else an
    /// admission): it is kept, listed with the reason, no longer delivered,
    /// and can be retried. An admission whose reason is `uncertain` (its
    /// outcome is unknown) stays pending, delivered after the others.
    pub(in crate::playbooks) async fn hold(
        &self,
        owner: &RuntimeOwner,
        item: HeldItem,
        reason: &str,
    ) -> Result<()> {
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        let (id, sql) = match item {
            HeldItem::Event(id) => (
                id,
                "UPDATE playbook_outbox SET status='unknown',held_reason=$4 WHERE namespace_id=$1 AND generation=$2 AND event_id=$3 AND status='pending'",
            ),
            HeldItem::Admission(id) => (
                id,
                "UPDATE playbook_admissions SET status=CASE WHEN $4='uncertain' THEN 'pending' ELSE 'unknown' END,held_reason=$4 WHERE namespace_id=$1 AND generation=$2 AND request_id=$3 AND receipt IS NULL AND status='pending'",
            ),
        };
        sqlx::query(sql)
            .bind(self.namespace.namespace_id)
            .bind(self.namespace.generation)
            .bind(id)
            .bind(reason)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
    /// The engine has started or forgotten every destination of an accepted event.
    pub(in crate::playbooks) async fn complete_event(
        &self,
        owner: &RuntimeOwner,
        id: Uuid,
    ) -> Result<()> {
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        sqlx::query("UPDATE playbook_outbox SET fanout_complete=true WHERE namespace_id=$1 AND generation=$2 AND event_id=$3 AND status='engine_accepted'").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }
}
