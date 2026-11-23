use super::*;
use aq_terminal_relay::LedgerSummary;
fn effect_meta(effect: &Effect) -> (Uuid, &'static str, u64, Option<u64>, bool) {
    match effect {
        Effect::Fence(v) => (
            v.fence_id,
            "fence",
            v.generation,
            v.controller.as_ref().map(|c| c.expires_at_ms),
            false,
        ),
        Effect::Capture(v) => (
            v.capture_request_id,
            "capture",
            v.controller_generation,
            Some(v.expires_at_ms),
            false,
        ),
        Effect::Input(v) => (
            v.operation_id,
            "input",
            v.lease.generation,
            Some(v.deadline_ms),
            true,
        ),
        Effect::EpochInput(v) => (
            v.input.operation_id,
            "epoch_input",
            v.input.lease.generation,
            Some(v.input.deadline_ms),
            true,
        ),
        Effect::Resize(v) => (
            v.operation_id,
            "resize",
            v.lease.generation,
            Some(v.deadline_ms),
            false,
        ),
        Effect::Watch(v) => (v.subscription_id, "watch", 0, Some(v.expires_at_ms), false),
    }
}
pub(super) struct Writes<'a> {
    pub terminal: &'a TerminalIdentity,
    pub summary: LedgerSummary,
    pub clock: u64,
    pub config: &'a Loaded,
}
impl Writes<'_> {
    pub(super) async fn replace(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        update: &super::journal::LedgerUpdate<'_>,
        peer_contact: bool,
    ) -> Result<()> {
        let sealed = self
            .config
            .keyring
            .seal(
                &Context {
                    terminal: self.terminal.clone(),
                    ledger_revision: self.summary.ledger_revision,
                    purpose: Purpose::Ledger,
                },
                update.next,
            )
            .map_err(|_| AppError::Internal)?;
        let principal = self
            .summary
            .control
            .lease
            .as_ref()
            .map(|c| Uuid::parse_str(c.principal_id.as_str()))
            .transpose()
            .map_err(|_| AppError::Internal)?;
        let changed=sqlx::query("UPDATE terminal_sessions SET authority_revision=$3,ledger_revision=$4,runtime_id=$5,protection_version=$6,key_id=$7,nonce=$8,ciphertext=$9,controller_generation=$10,controller_principal=$11,pending_bytes=$12,operation_count=$13,observed_at_ms=CASE WHEN $14 THEN $15 ELSE observed_at_ms END WHERE terminal_id=$1 AND ledger_revision=$2")
   .bind(self.terminal.terminal_id).bind(dbtime(update.expected)?).bind(dbtime(self.summary.session_revision)?).bind(dbtime(self.summary.ledger_revision)?).bind(self.config.runtime_id).bind(sealed.version as i32).bind(&sealed.key_id).bind(&sealed.nonce).bind(&sealed.ciphertext).bind(dbtime(self.summary.generation)?).bind(principal).bind(self.summary.pending_bytes as i32).bind(self.summary.mutation_count as i32).bind(peer_contact).bind(dbtime(self.clock)?).execute(&mut **tx).await.map_err(|_|AppError::Internal)?.rows_affected();
        if changed != 1 {
            return Err(AppError::Conflict(
                "terminal ledger revision changed".into(),
            ));
        }
        Ok(())
    }
    pub(super) async fn receipts(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        ledger: &Ledger,
    ) -> Result<()> {
        let metadata = ledger.input_metadata();
        let meta = serde_json::to_string(&metadata).map_err(|_| AppError::Internal)?;
        sqlx::query("DELETE FROM terminal_input_receipts WHERE terminal_id=$1 AND NOT(operation_id=ANY($2))").bind(self.terminal.terminal_id).bind(metadata.iter().map(|v|v.operation_id).collect::<Vec<_>>()).execute(&mut **tx).await.map_err(|_|AppError::Internal)?;
        sqlx::query("INSERT INTO terminal_input_receipts(terminal_id,operation_id,principal_id,request_digest,command_digest,disposition,deadline_ms,dispatched,byte_count,admission_revision,epoch_id,updated_at_ms) SELECT $1,(x->>'operation_id')::uuid,(x->>'principal')::uuid,x->>'request_digest',x->>'command_digest',x->>'disposition',(x->>'deadline_ms')::bigint,(x->>'dispatched')::boolean,(x->>'byte_count')::integer,(x->>'admission_revision')::bigint,(x->>'epoch_id')::uuid,$3 FROM jsonb_array_elements($2::jsonb) x ON CONFLICT(terminal_id,operation_id) DO UPDATE SET principal_id=excluded.principal_id,request_digest=excluded.request_digest,command_digest=excluded.command_digest,disposition=excluded.disposition,deadline_ms=excluded.deadline_ms,dispatched=excluded.dispatched,byte_count=excluded.byte_count,admission_revision=excluded.admission_revision,epoch_id=excluded.epoch_id,updated_at_ms=CASE WHEN terminal_input_receipts.disposition IS DISTINCT FROM excluded.disposition OR terminal_input_receipts.dispatched IS DISTINCT FROM excluded.dispatched THEN excluded.updated_at_ms ELSE terminal_input_receipts.updated_at_ms END")
   .bind(self.terminal.terminal_id).bind(meta).bind(dbtime(self.clock)?).execute(&mut **tx).await.map_err(|_|AppError::Internal)?;
        let inflight: Vec<_> = metadata
            .iter()
            .filter(|m| {
                m.dispatched
                    && matches!(
                        m.disposition,
                        aq_session_protocol::InputDisposition::Queued
                            | aq_session_protocol::InputDisposition::Received
                            | aq_session_protocol::InputDisposition::Injecting
                    )
            })
            .map(|m| m.operation_id)
            .collect();
        sqlx::query("DELETE FROM terminal_outbox WHERE terminal_id=$1 AND kind IN('input','epoch_input') AND NOT(operation_id=ANY($2))").bind(self.terminal.terminal_id).bind(inflight).execute(&mut **tx).await.map_err(|_|AppError::Internal)?;
        sqlx::query("DELETE FROM terminal_outbox WHERE terminal_id=$1 AND kind IN('watch','capture','resize') AND (acknowledged OR expires_at_ms<=$2 OR(kind='resize' AND controller_generation<$3))").bind(self.terminal.terminal_id).bind(dbtime(self.clock)?).bind(dbtime(self.summary.generation)?).execute(&mut **tx).await.map_err(|_|AppError::Internal)?;
        Ok(())
    }
    pub(super) async fn capacity(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        node_id: Uuid,
    ) -> Result<()> {
        if self.summary.control.lease.is_some() {
            let controllers:i64=sqlx::query_scalar("SELECT count(DISTINCT s.terminal_id) FROM terminal_sessions s JOIN terminal_outbox o ON o.terminal_id=s.terminal_id WHERE s.node_id=$1 AND s.terminal_id<>$2 AND o.kind='fence' AND o.controller_generation=s.controller_generation AND (o.expires_at_ms>$3 OR(o.expires_at_ms IS NULL AND NOT o.acknowledged))").bind(node_id).bind(self.terminal.terminal_id).bind(dbtime(self.clock)?).fetch_one(&mut **tx).await.map_err(|_|AppError::Internal)?;
            if controllers >= 32 {
                return Err(AppError::IdentityUnavailable(
                    "node terminal controller capacity exhausted".into(),
                ));
            }
        }
        let other:i64=sqlx::query_scalar("SELECT count(*) FROM terminal_subscriptions WHERE node_id=$1 AND terminal_id<>$2 AND expires_at_ms>$3").bind(node_id).bind(self.terminal.terminal_id).bind(dbtime(self.clock)?).fetch_one(&mut **tx).await.map_err(|_|AppError::Internal)?;
        if other
            + self
                .summary
                .subscriptions
                .iter()
                .filter(|s| self.clock < s.expires_at_ms)
                .count() as i64
            > 32
        {
            return Err(AppError::IdentityUnavailable(
                "node terminal subscription capacity exhausted".into(),
            ));
        }
        Ok(())
    }
    pub(super) async fn subscriptions(&self, tx: &mut Transaction<'_, Postgres>) -> Result<()> {
        sqlx::query("DELETE FROM terminal_subscriptions WHERE terminal_id=$1")
            .bind(self.terminal.terminal_id)
            .execute(&mut **tx)
            .await
            .map_err(|_| AppError::Internal)?;
        if !self.summary.subscriptions.is_empty() {
            let body = serde_json::to_string(&self.summary.subscriptions)
                .map_err(|_| AppError::Internal)?;
            let SessionIdentity::Remote { node_id, .. } = self.terminal.session else {
                return Err(AppError::Conflict(
                    "central terminal authority unavailable".into(),
                ));
            };
            sqlx::query("INSERT INTO terminal_subscriptions(terminal_id,subscription_id,principal_id,node_id,expires_at_ms) SELECT $1,(x->>'subscription_id')::uuid,(x->>'principal_id')::uuid,$3,(x->>'expires_at_ms')::bigint FROM jsonb_array_elements($2::jsonb) x").bind(self.terminal.terminal_id).bind(body).bind(node_id).execute(&mut **tx).await.map_err(|_|AppError::Internal)?;
        }
        Ok(())
    }
    pub(super) async fn recover(&self, tx: &mut Transaction<'_, Postgres>) -> Result<()> {
        sqlx::query("DELETE FROM terminal_outbox WHERE terminal_id=$1 AND kind IN('input','epoch_input','capture','resize','watch')").bind(self.terminal.terminal_id).execute(&mut **tx).await.map_err(|_|AppError::Internal)?;
        sqlx::query("UPDATE terminal_outbox SET acknowledged=FALSE WHERE terminal_id=$1 AND kind='fence' AND controller_generation=$2").bind(self.terminal.terminal_id).bind(dbtime(self.summary.generation)?).execute(&mut **tx).await.map_err(|_|AppError::Internal)?;
        Ok(())
    }
    pub(super) async fn effect(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        ordinal: usize,
        effect: &Effect,
    ) -> Result<()> {
        let (id, kind, generation, expires, claimed) = effect_meta(effect);
        if matches!(effect, Effect::Fence(_)) {
            sqlx::query("DELETE FROM terminal_outbox WHERE terminal_id=$1 AND kind='fence' AND controller_generation<$2").bind(self.terminal.terminal_id).bind(dbtime(generation)?).execute(&mut **tx).await.map_err(|_|AppError::Internal)?;
        }
        let body = serde_json::to_vec(effect).map_err(|_| AppError::Internal)?;
        let sealed = self
            .config
            .keyring
            .seal(
                &Context {
                    terminal: self.terminal.clone(),
                    ledger_revision: self.summary.ledger_revision,
                    purpose: Purpose::Outbox {
                        operation_id: id,
                        ordinal: ordinal as u32,
                    },
                },
                &body,
            )
            .map_err(|_| AppError::Internal)?;
        sqlx::query("INSERT INTO terminal_outbox(terminal_id,operation_id,kind,controller_generation,ledger_revision,expires_at_ms,claimed,protection_version,key_id,nonce,ciphertext,ordinal) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)").bind(self.terminal.terminal_id).bind(id).bind(kind).bind(dbtime(generation)?).bind(dbtime(self.summary.ledger_revision)?).bind(expires.map(dbtime).transpose()?).bind(claimed).bind(sealed.version as i32).bind(sealed.key_id).bind(sealed.nonce).bind(sealed.ciphertext).bind(ordinal as i32).execute(&mut **tx).await.map_err(|_|AppError::Internal)?;
        Ok(())
    }
    pub(super) async fn require_outbox_capacity(
        &self,
        tx: &mut Transaction<'_, Postgres>,
    ) -> Result<()> {
        let counts:(i64,i64)=sqlx::query_as("SELECT count(*),coalesce(sum(octet_length(ciphertext)),0)::bigint FROM terminal_outbox WHERE terminal_id=$1").bind(self.terminal.terminal_id).fetch_one(&mut **tx).await.map_err(|_|AppError::Internal)?;
        if counts.0 > 128 || counts.1 > 262144 {
            return Err(AppError::IdentityUnavailable(
                "terminal outbox capacity exhausted".into(),
            ));
        }
        Ok(())
    }
}
