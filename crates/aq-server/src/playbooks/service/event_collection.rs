use super::*;

impl Service {
    /// Bounded copy to the existing engine outbox; owner-copy ACK and insert
    /// share SQL, engine acceptance/fanout ACK is a later independent boundary.
    pub(in crate::playbooks) async fn collect_decision_events(
        &self,
        owner: &RuntimeOwner,
        limit: u32,
    ) -> Result<usize> {
        if !self.profile.has_reviews() {
            return Ok(0);
        }
        if !(1..=2).contains(&limit) {
            return Err(Error::Invalid);
        }
        let reviews = self.reviews.as_ref().ok_or(Error::Forbidden)?;
        let mut tx = self.tx().await?;
        crate::reviews::Service::lock_budget(&mut tx)
            .await
            .map_err(review_error)?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        let rows=sqlx::query("SELECT e.intent_id,e.receipt FROM playbook_effects e JOIN review_owner_outbox o ON o.decision_id=(e.receipt->>'handle')::uuid WHERE e.namespace_id=$1 AND e.generation=$2 AND e.command_name='decision.request' AND NOT o.acknowledged ORDER BY o.event_id LIMIT $3").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(limit as i64).fetch_all(&mut *tx).await?;
        for row in &rows {
            self.copy_decision_event(&mut tx, reviews, row).await?;
        }
        Self::owner(&mut tx, self.namespace.owner_user_id).await?;
        tx.commit().await?;
        Ok(rows.len())
    }
    async fn copy_decision_event(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        reviews: &crate::reviews::Service,
        row: &sqlx::postgres::PgRow,
    ) -> Result<()> {
        let accepted: Value = row.get("receipt");
        let decision: Uuid = serde_json::from_value(accepted["handle"].clone())?;
        let (subject, payload) = reviews
            .completion_in_tx(tx, decision)
            .await
            .map_err(review_error)?;
        if accepted["subject"] != json!(subject.artifact().map_err(|_| Error::Invalid)?) {
            return Err(Error::Conflict);
        }
        if (subject.project_id, subject.task.workspace_id)
            != (self.namespace.project_id, self.namespace.workspace_id)
        {
            return Err(Error::Conflict);
        }
        let c = crate::playbooks::review_domain::decision_event();
        let at = self.now()?;
        let event = EventEnvelope {
            producer: "aq.human.decisions@1".into(),
            event_id: decision.to_string(),
            contract: ContractRef {
                name: c.name.clone(),
                fingerprint: c.fingerprint().map_err(|_| Error::Invalid)?,
            },
            scope: "project".into(),
            correlation: BTreeMap::from([("decision".into(), decision.to_string())]),
            subject: None,
            revision: None,
            causation: Some(row.get("intent_id")),
            occurred_at_ms: None,
            observed_at_ms: Some(at),
            payload: json!({"outcome":payload["outcome"],"output":payload["output"]}),
        };
        aq_playbook_host::model::validate_event(&event, self.registry())
            .map_err(|_| Error::Invalid)?;
        let digest = digest(&event)?;
        let old:Option<String>=sqlx::query_scalar("SELECT event_digest FROM playbook_outbox WHERE namespace_id=$1 AND generation=$2 AND producer='aq.human.decisions@1' AND event_id=$3").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(decision).fetch_optional(&mut **tx).await?;
        if let Some(old) = old {
            if old != digest {
                return Err(Error::Conflict);
            }
        } else {
            let bytes = canonical_bytes(&event).map_err(|_| Error::Invalid)?;
            let sealed = self
                .keys
                .seal(&self.aad("aq.playbooks.outbox", decision, &digest)?, &bytes)
                .map_err(|_| Error::Unknown)?;
            sqlx::query("INSERT INTO playbook_outbox(namespace_id,generation,producer,epoch,event_id,event_digest,sealed_version,key_id,nonce,ciphertext,created_at_ms,status) VALUES($1,$2,'aq.human.decisions@1',0,$3,$4,$5,$6,$7,$8,$9,'pending')").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(decision).bind(digest).bind(sealed.version as i32).bind(sealed.key_id).bind(sealed.nonce).bind(sealed.ciphertext).bind(at).execute(&mut **tx).await?;
        }
        sqlx::query("UPDATE review_owner_outbox SET acknowledged=true WHERE decision_id=$1")
            .bind(decision)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }
    /// Bounded seal of undelivered confirmed graph operations bound to this
    /// namespace's project into the engine outbox. `delivered` is set only by
    /// `acknowledge_event`, after durable engine acceptance; a crash between
    /// seal and ACK replays the same sealed event and the engine dedups on
    /// (producer, epoch 0, event_id).
    pub(in crate::playbooks) async fn collect_graph_events(
        &self,
        owner: &RuntimeOwner,
        limit: u32,
    ) -> Result<usize> {
        if !matches!(
            self.profile,
            CatalogProfile::GraphEvents3
                | CatalogProfile::SettlementEvents4
                | CatalogProfile::Monitoring6
        ) {
            return Ok(0);
        }
        if !(1..=2).contains(&limit) {
            return Err(Error::Invalid);
        }
        let producer = crate::playbooks::review_domain::GRAPH_PRODUCER;
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        let rows = sqlx::query("SELECT o.event_id,g.operation_id,g.attempt_id,b.binding->>'task_id' AS task_id FROM agent_graph_outbox o JOIN agent_graph_operations g ON g.operation_id=o.operation_id JOIN agent_launch_bindings b ON b.attempt_id=g.attempt_id WHERE NOT o.delivered AND g.phase='confirmed' AND b.binding->>'project_id'=$1 AND b.binding->>'workspace_id'=$2 AND NOT EXISTS(SELECT 1 FROM playbook_outbox p WHERE p.namespace_id=$3 AND p.generation=$4 AND p.producer='aq.graph.operations@1' AND p.event_id=o.event_id) ORDER BY g.operation_seq LIMIT $5")
            .bind(self.namespace.project_id.to_string())
            .bind(self.namespace.workspace_id.to_string())
            .bind(self.namespace.namespace_id)
            .bind(self.namespace.generation)
            .bind(limit as i64)
            .fetch_all(&mut *tx)
            .await?;
        let c = crate::playbooks::review_domain::graph_operation_event();
        let fingerprint = c.fingerprint().map_err(|_| Error::Invalid)?;
        for row in &rows {
            let id: Uuid = row.get("event_id");
            let operation: Uuid = row.get("operation_id");
            let attempt: Uuid = row.get("attempt_id");
            let task = row.get::<Option<String>, _>("task_id");
            let task: Option<Uuid> = task.and_then(|t| t.parse().ok());
            let at = self.now()?;
            let event = EventEnvelope {
                producer: producer.into(),
                event_id: id.to_string(),
                contract: ContractRef {
                    name: c.name.clone(),
                    fingerprint: fingerprint.clone(),
                },
                scope: "project".into(),
                correlation: BTreeMap::from([("operation".into(), operation.to_string())]),
                subject: None,
                revision: None,
                causation: None,
                occurred_at_ms: None,
                observed_at_ms: Some(at),
                payload: json!({"operation_id":operation,"task_id":task,"attempt_id":attempt}),
            };
            // A bad source item (no task id, or not this contract) is queued
            // held `invalid`, so it is listed and later items are collected.
            let registry = self.registry();
            let valid = aq_playbook_host::model::validate_event(&event, registry).is_ok();
            let held = (task.is_none() || !valid).then_some("invalid");
            let status = if held.is_some() { "unknown" } else { "pending" };
            let digest = digest(&event)?;
            let bytes = canonical_bytes(&event).map_err(|_| Error::Invalid)?;
            let sealed = self
                .keys
                .seal(&self.aad("aq.playbooks.outbox", id, &digest)?, &bytes)
                .map_err(|_| Error::Unknown)?;
            sqlx::query("INSERT INTO playbook_outbox(namespace_id,generation,producer,epoch,event_id,event_digest,sealed_version,key_id,nonce,ciphertext,created_at_ms,status,held_reason) VALUES($1,$2,$3,0,$4,$5,$6,$7,$8,$9,$10,$11,$12)").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(producer).bind(id).bind(digest).bind(sealed.version as i32).bind(sealed.key_id).bind(sealed.nonce).bind(sealed.ciphertext).bind(at).bind(status).bind(held).execute(&mut *tx).await?;
        }
        Self::owner(&mut tx, self.namespace.owner_user_id).await?;
        tx.commit().await?;
        Ok(rows.len())
    }
}
