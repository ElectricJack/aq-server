//! AQ-owned admission metadata only, never decrypts or returns workflow input.
use super::{Error, Result, Service};
use crate::playbooks::inspection::{AdmissionView, NamespaceView, Page, VERSION};
use aq_playbook_domain::view_store::ViewStore;
use uuid::Uuid;
impl Service {
    pub(crate) async fn authorize_inspection(&self, actor: Uuid) -> Result<()> {
        if actor != self.namespace.owner_user_id {
            return Err(Error::Forbidden);
        }
        let mut tx = self.tx().await?;
        Self::owner(&mut tx, actor).await?;
        self.namespace_locked(&mut tx).await?;
        tx.commit().await?;
        Ok(())
    }
    /// Held outbox events (at most 25, newest first) and why they are held.
    pub(crate) async fn held_events(&self) -> Result<Vec<serde_json::Value>> {
        let rows: Vec<(Uuid, String, i64, Option<String>)> = sqlx::query_as("SELECT event_id,producer,created_at_ms,held_reason FROM playbook_outbox WHERE namespace_id=$1 AND generation=$2 AND status='unknown' ORDER BY sequence DESC LIMIT 25").bind(self.namespace.namespace_id).bind(self.namespace.generation).fetch_all(&self.pool).await?;
        let view = |(id, producer, at, reason)| serde_json::json!({"event_id":id,"producer":producer,"created_at_ms":at,"held_reason":reason});
        Ok(rows.into_iter().map(view).collect())
    }
    /// Whether AQ admitted `run` (its receipt is recorded).
    pub(crate) async fn admitted_run(&self, run: &str) -> Result<bool> {
        let admitted = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM playbook_admissions WHERE namespace_id=$1 AND generation=$2 AND receipt->>'run_id'=$3)").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(run);
        Ok(admitted.fetch_one(&self.pool).await?)
    }
    /// A held outbox event goes back to the queue: a new delivery, unless
    /// the engine accepted it: then it resumes its fan-out, never a new event.
    /// `hold` is the reverse, for a queued event the owner names (one that
    /// cannot be decrypted): held `invalid`, listed, and delivery goes on.
    pub(crate) async fn retry_event(&self, actor: Uuid, id: Uuid, hold: bool) -> Result<()> {
        self.authorize_inspection(actor).await?;
        let retried = sqlx::query("UPDATE playbook_outbox SET status=CASE WHEN $4 THEN 'unknown' WHEN engine_receipt IS NULL THEN 'pending' ELSE 'engine_accepted' END,held_reason=CASE WHEN $4 THEN 'invalid' END WHERE namespace_id=$1 AND generation=$2 AND event_id=$3 AND CASE WHEN $4 THEN status='pending' OR (status='engine_accepted' AND NOT fanout_complete) ELSE status='unknown' END").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(id).bind(hold).execute(&self.pool).await?;
        (retried.rows_affected() == 1)
            .then_some(())
            .ok_or(Error::Conflict)
    }
    pub async fn inspect_admissions(
        &self,
        actor: Uuid,
        after: Option<String>,
        limit: usize,
    ) -> Result<Page<AdmissionView>> {
        if actor != self.namespace.owner_user_id || limit == 0 || limit > 25 {
            return Err(Error::Forbidden);
        }
        let after = after
            .map(|s| Uuid::parse_str(&s).map_err(|_| Error::Invalid))
            .transpose()?;
        let mut tx = self.tx().await?;
        Self::owner(&mut tx, actor).await?;
        self.namespace_locked(&mut tx).await?;
        let (items, next_cursor) = ViewStore(&self.namespace)
            .admissions(&mut tx, after, limit)
            .await?;
        tx.commit().await?;
        Ok(Page {
            api_version: VERSION,
            namespace: NamespaceView::from(&self.namespace),
            server_time_ms: self.current_time(),
            revision: None,
            items,
            next_cursor,
        })
    }
}
