use super::writes::Writes;
use super::*;
pub struct SqlJournal<'a> {
    pub state: &'a AppState,
    pub tx: Option<Transaction<'static, Postgres>>,
    pub operation: Option<Operation>,
    pub peer_contact: bool,
    pub recovering: bool,
}
impl AsyncJournal for SqlJournal<'_> {
    fn commit<'a>(
        &'a mut self,
        terminal: &'a TerminalIdentity,
        expected: u64,
        next: &'a [u8],
        effects: &'a [Effect],
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = aq_terminal_relay::Result<()>> + Send + 'a>,
    > {
        Box::pin(async move {
            self.persist(LedgerUpdate {
                terminal,
                expected,
                next,
                effects,
            })
            .await
            .map_err(|_| aq_terminal_relay::Error::Persistence)
        })
    }
}
pub(super) struct LedgerUpdate<'a> {
    pub terminal: &'a TerminalIdentity,
    pub expected: u64,
    pub next: &'a [u8],
    pub effects: &'a [Effect],
}
impl SqlJournal<'_> {
    async fn persist(&mut self, update: LedgerUpdate<'_>) -> Result<()> {
        let config = configured(self.state)?;
        let ledger = Ledger::load(update.next).map_err(relay)?;
        let writes = Writes {
            terminal: update.terminal,
            summary: ledger.summary(),
            clock: now(self.state),
            config: &config,
        };
        let tx = self.tx.as_mut().ok_or(AppError::Internal)?;
        if writes.summary.terminal != *update.terminal {
            return Err(AppError::Internal);
        }
        writes.replace(tx, &update, self.peer_contact).await?;
        writes.receipts(tx, &ledger).await?;
        if let SessionIdentity::Remote { node_id, .. } = update.terminal.session {
            writes.capacity(tx, node_id).await?;
        }
        writes.subscriptions(tx).await?;
        if self.recovering {
            writes.recover(tx).await?;
        }
        for (ordinal, effect) in update.effects.iter().enumerate() {
            writes.effect(tx, ordinal, effect).await?;
        }
        writes.require_outbox_capacity(tx).await?;
        if let Some(op) = &self.operation {
            save_operation(tx, update.terminal.terminal_id, op, writes.clock).await?;
        }
        self.tx
            .take()
            .ok_or(AppError::Internal)?
            .commit()
            .await
            .map_err(|_| AppError::Internal)?;
        Ok(())
    }
}
