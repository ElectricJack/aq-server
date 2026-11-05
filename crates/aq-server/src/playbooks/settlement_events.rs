//! Bounded confirmed-settlement publication through the existing engine outbox.
use super::*;

impl Service {
    pub(in crate::playbooks) async fn collect_settlement_events(
        &self,
        owner: &RuntimeOwner,
        limit: u32,
    ) -> Result<usize> {
        if !matches!(
            self.profile,
            CatalogProfile::SettlementEvents4 | CatalogProfile::Monitoring6
        ) {
            return Ok(0);
        }
        if !(1..=2).contains(&limit) {
            return Err(Error::Invalid);
        }
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        let rows = sqlx::query("SELECT c.operation_id,c.outbox FROM profiled_task_completions c JOIN profiled_launches l USING(attempt_id) WHERE c.state='confirmed' AND NOT c.outbox_acknowledged AND c.workspace_id=$1 AND l.execution_spec#>>'{lifecycle,project_id}'=$2 AND NOT EXISTS(SELECT 1 FROM playbook_outbox p WHERE p.namespace_id=$3 AND p.generation=$4 AND p.producer='aq.task.settlement@1' AND p.event_id=c.operation_id) ORDER BY c.created_at_ms,c.operation_id LIMIT $5")
            .bind(self.namespace.workspace_id)
            .bind(self.namespace.project_id.to_string())
            .bind(self.namespace.namespace_id)
            .bind(self.namespace.generation)
            .bind(limit as i64)
            .fetch_all(&mut *tx).await?;
        let contract = super::super::review_domain::task_completed_event();
        let fingerprint = contract.fingerprint().map_err(|_| Error::Invalid)?;
        for row in &rows {
            let id: Uuid = row.get("operation_id");
            let at = self.now()?;
            let event = EventEnvelope {
                producer: super::super::review_domain::SETTLEMENT_PRODUCER.into(),
                event_id: id.to_string(),
                contract: ContractRef {
                    name: contract.name.clone(),
                    fingerprint: fingerprint.clone(),
                },
                scope: "project".into(),
                correlation: BTreeMap::from([("operation".into(), id.to_string())]),
                subject: None,
                revision: None,
                causation: None,
                occurred_at_ms: None,
                observed_at_ms: Some(at),
                payload: row.get("outbox"),
            };
            aq_playbook_host::model::validate_event(&event, &self.profile.registry())
                .map_err(|_| Error::Invalid)?;
            let digest = digest(&event)?;
            let bytes = canonical_bytes(&event).map_err(|_| Error::Invalid)?;
            let sealed = self
                .keys
                .seal(&self.aad("aq.playbooks.outbox", id, &digest)?, &bytes)
                .map_err(|_| Error::Unknown)?;
            self.budget(&mut tx, 0, 0, 0, 1, sealed.ciphertext.len() as i64 + 4096)
                .await?;
            sqlx::query("INSERT INTO playbook_outbox(namespace_id,generation,producer,epoch,event_id,event_digest,sealed_version,key_id,nonce,ciphertext,created_at_ms,status) VALUES($1,$2,$3,0,$4,$5,$6,$7,$8,$9,$10,'pending')")
                .bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(&event.producer)
                .bind(id).bind(digest).bind(sealed.version as i32).bind(sealed.key_id).bind(sealed.nonce)
                .bind(sealed.ciphertext).bind(at).execute(&mut *tx).await?;
        }
        Self::owner(&mut tx, self.namespace.owner_user_id).await?;
        tx.commit().await?;
        Ok(rows.len())
    }
}
