use super::*;
use aq_playbook_domain::intent_store::{QueuedMail, StoredIntent};

impl Service {
    pub async fn send_message(
        &self,
        owner: &RuntimeOwner,
        id: Uuid,
        revision: i64,
        intent: &Intent,
    ) -> Result<MessageResult> {
        let input = message_input(intent)?;
        let identity = self.identity(intent)?;
        let mail = StoredIntent::new(&self.namespace, intent, &identity);
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        // Historical replay is exact but does not create or refresh any write authority.
        if let Some(row) = mail.effect(&mut tx).await? {
            let scope = self
                .grant_locked(&mut tx, GrantRevision::new(id, revision), false)
                .await?;
            self.intent_authority(intent, GrantRevision::new(id, revision), &scope)?;
            let result = mail.response(row)?;
            tx.commit().await?;
            return Ok(result);
        }
        let scope = self
            .grant_locked(&mut tx, GrantRevision::new(id, revision), true)
            .await?;
        self.intent_authority(intent, GrantRevision::new(id, revision), &scope)?;
        if !scope.permits(&input.recipient) || intent.deadline_at_ms <= self.now()? {
            return Err(Error::Forbidden);
        }
        if mail.cancelled(&mut tx).await? {
            return Ok(MessageResult::Cancelled);
        }
        let queued = self.enqueue_mail(&mut tx, intent, input).await?;
        let event_id = stable_id(
            "aq.playbook.message.queued.event",
            self.namespace.namespace_id,
            self.namespace.generation,
            &intent.id,
        )?;
        let at = self.now()?;
        let event = queued.event(event_id, &intent.id, at)?;
        let QueuedMail { op, response, .. } = queued;
        let event_digest = digest(&event)?;
        let bytes = canonical_bytes(&event).map_err(|_| Error::Invalid)?;
        let aad = self.aad("aq.playbooks.outbox", event_id, &event_digest)?;
        let sealed = self.keys.seal(&aad, &bytes).map_err(|_| Error::Unknown)?;
        sqlx::query("INSERT INTO playbook_effects(namespace_id,generation,intent_id,run_id,command_name,request_digest,command_fingerprint,input_digest,grant_id,grant_revision,operation_id,status,receipt,created_at_ms) VALUES($1,$2,$3,$4,'message.send',$5,$6,$7,$8,$9,$10,'queued',$11,$12)").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(&intent.id).bind(&intent.run).bind(&identity).bind(intent.command.fingerprint().map_err(|_|Error::Invalid)?).bind(&intent.input_digest).bind(id).bind(revision).bind(op).bind(&response).bind(at).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO playbook_outbox(namespace_id,generation,producer,epoch,event_id,event_digest,sealed_version,key_id,nonce,ciphertext,created_at_ms,status) VALUES($1,$2,'aq.host.bridge@1',0,$3,$4,$5,$6,$7,$8,$9,'pending')").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(event_id).bind(event_digest).bind(sealed.version as i32).bind(sealed.key_id).bind(sealed.nonce).bind(sealed.ciphertext).bind(at).execute(&mut *tx).await?;
        self.check_expiry(&mut tx, id).await?;
        if intent.deadline_at_ms <= self.now()? {
            return Err(Error::Forbidden);
        }
        tx.commit().await?;
        Ok(MessageResult::Queued(response))
    }
    pub async fn reconcile(
        &self,
        id: Uuid,
        revision: i64,
        intent: &Intent,
    ) -> Result<Option<MessageResult>> {
        message_input(intent)?;
        let identity = self.identity(intent)?;
        let mail = StoredIntent::new(&self.namespace, intent, &identity);
        let mut tx = self.tx().await?;
        self.namespace_locked(&mut tx).await?;
        let scope = self
            .grant_locked(&mut tx, GrantRevision::new(id, revision), false)
            .await?;
        self.intent_authority(intent, GrantRevision::new(id, revision), &scope)?;
        let result = match mail.effect(&mut tx).await? {
            Some(row) => Some(mail.response(row)?),
            None => mail
                .confirmed_cancellation(&mut tx)
                .await?
                .then_some(MessageResult::Cancelled),
        };
        tx.commit().await?;
        Ok(result)
    }
    pub async fn cancel(
        &self,
        owner: &RuntimeOwner,
        id: Uuid,
        revision: i64,
        intent: &Intent,
    ) -> Result<CancellationResult> {
        let input = message_input(intent)?;
        let identity = self.identity(intent)?;
        let mail = StoredIntent::new(&self.namespace, intent, &identity);
        let key = aq_playbook_host::runner::cancellation_key(intent).map_err(|_| Error::Invalid)?;
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        let scope = self
            .grant_locked(&mut tx, GrantRevision::new(id, revision), false)
            .await?;
        self.intent_authority(intent, GrantRevision::new(id, revision), &scope)?;
        if !scope.permits(&input.recipient) {
            return Err(Error::Forbidden);
        }
        if let Some(retained) = mail.retained_cancel(&mut tx, &key).await? {
            return Ok(retained);
        }
        let result = mail.cancel(&mut tx, &key, || self.now()).await?;
        tx.commit().await?;
        Ok(result)
    }
    async fn enqueue_mail(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        intent: &Intent,
        input: MessageInput,
    ) -> Result<QueuedMail> {
        let op = stable_id(
            "aq.playbook.message.operation",
            self.namespace.namespace_id,
            self.namespace.generation,
            &intent.id,
        )?;
        let message = stable_id(
            "aq.playbook.message.identity",
            self.namespace.namespace_id,
            self.namespace.generation,
            &intent.id,
        )?;
        let recipient: messaging::Recipient = serde_json::from_value(json!(input.recipient))?;
        let response = messaging::owner_send_in_tx(
            tx,
            messaging::Send {
                operation_id: op,
                message_id: message,
                thread_id: input.thread_id,
                recipient,
                body: input.body,
            },
            self.namespace.owner_user_id,
            self.now()? as u64,
        )
        .await
        .map_err(|e| match e {
            messaging::OwnerSendError::Invalid => Error::Invalid,
            messaging::OwnerSendError::NotFound => Error::Forbidden,
            messaging::OwnerSendError::Conflict => Error::Conflict,
            messaging::OwnerSendError::Unknown => Error::Unknown,
        })?;
        message_contract().outcomes["queued"]
            .validate(&response)
            .map_err(|_| Error::Unknown)?;
        Ok(QueuedMail {
            op,
            message,
            subject: input.recipient.scope().2.to_string(),
            response,
        })
    }
}
