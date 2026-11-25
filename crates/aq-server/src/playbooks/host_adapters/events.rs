//! Catalog-validated host events and their sealed outbox rows.
use super::*;

pub(super) struct HostEvent<'a> {
    pub contract: &'a str,
    pub event_id: Uuid,
    pub operation: Uuid,
    pub causation: &'a str,
    pub subject: &'a Value,
    pub payload: Value,
}
pub(in crate::playbooks) struct OutboxEntry<'a> {
    pub row_id: Uuid,
    pub event: &'a EventEnvelope,
    pub at: i64,
    pub retention: Option<i64>,
}

impl Service {
    /// Seals one aq.work.changed event into the outbox in the caller's tx.
    pub(super) async fn work_event(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        intent: &Intent,
        receipt: EffectReceipt<'_>,
    ) -> Result<()> {
        let output = receipt.value;
        let event_id = stable_id(
            &format!("aq.playbook.{}.event", intent.command.name),
            self.namespace.namespace_id,
            self.namespace.generation,
            &intent.id,
        )?;
        self.seal_event(tx, HostEvent { contract: "aq.work.changed", event_id, operation: self.operation(intent)?, causation: &intent.id, subject: &output["subject"], payload: json!({"outcome": receipt.status, "reference": output["reference"], "subject": output["subject"]}) })
        .await
    }

    /// Seals one trusted host-bridge event into the outbox in the caller's
    /// tx, validated against the pinned catalog event contract.
    pub(super) async fn seal_event(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        host: HostEvent<'_>,
    ) -> Result<()> {
        let HostEvent {
            contract,
            event_id,
            operation,
            causation,
            subject,
            payload,
        } = host;
        let registry = self.registry();
        let c = registry.events.get(contract).ok_or(Error::Invalid)?;
        let at = self.now()?;
        // `aq.delivery.completed` names no subject or revision: the join
        // correlates it by the operation.
        #[cfg(feature = "delivery_playbook")]
        let (subject_id, revision) = if subject.is_null() {
            (None, None)
        } else {
            (
                Some(text(subject, "id")?.to_string()),
                Some(subject["revision"].as_u64().ok_or(Error::Invalid)?),
            )
        };
        #[cfg(not(feature = "delivery_playbook"))]
        let (subject_id, revision) = (
            Some(text(subject, "id")?.to_string()),
            Some(subject["revision"].as_u64().ok_or(Error::Invalid)?),
        );
        let event = EventEnvelope {
            producer: "aq.host.bridge@1".into(),
            event_id: event_id.to_string(),
            contract: ContractRef {
                name: c.name.clone(),
                fingerprint: c.fingerprint().map_err(|_| Error::Invalid)?,
            },
            scope: "project".into(),
            correlation: BTreeMap::from([("operation".into(), operation.to_string())]),
            subject: subject_id,
            revision,
            causation: Some(causation.to_string()),
            occurred_at_ms: Some(at),
            observed_at_ms: Some(at),
            payload,
        };
        aq_playbook_host::model::validate_event(&event, registry).map_err(|_| Error::Invalid)?;
        self.seal_outbox(
            tx,
            OutboxEntry {
                row_id: event_id,
                event: &event,
                at,
                retention: None,
            },
        )
        .await
    }

    pub(in crate::playbooks) async fn seal_outbox(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        entry: OutboxEntry<'_>,
    ) -> Result<()> {
        let OutboxEntry {
            row_id,
            event,
            at,
            retention,
        } = entry;
        let event_digest = digest(event)?;
        let bytes = canonical_bytes(event).map_err(|_| Error::Invalid)?;
        let aad = self.aad("aq.playbooks.outbox", row_id, &event_digest)?;
        let sealed = self.keys.seal(&aad, &bytes).map_err(|_| Error::Unknown)?;
        sqlx::query("INSERT INTO playbook_outbox(namespace_id,generation,producer,epoch,event_id,event_digest,sealed_version,key_id,nonce,ciphertext,created_at_ms,status,plugin_event_id,plugin_retain_until_ms) VALUES($1,$2,$3,0,$4,$5,$6,$7,$8,$9,$10,'pending',$11,$12)")
            .bind(self.namespace.namespace_id)
            .bind(self.namespace.generation)
            .bind(&event.producer)
            .bind(row_id)
            .bind(event_digest)
            .bind(sealed.version as i32)
            .bind(sealed.key_id)
            .bind(sealed.nonce)
            .bind(sealed.ciphertext)
            .bind(at)
            .bind(retention.map(|_| &event.event_id))
            .bind(retention)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }
}
