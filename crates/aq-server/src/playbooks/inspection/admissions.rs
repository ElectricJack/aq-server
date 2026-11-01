//! AQ-owned admission metadata only, never decrypts or returns workflow input.
use super::{Error, Result, Service};
use crate::playbooks::inspection::{AdmissionView, NamespaceView, Page, VERSION};
use sqlx::Row;
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
        let rows=sqlx::query("SELECT request_id,status,created_at_ms,receipt,engine_key,request_digest,expected_activation_revision FROM playbook_admissions WHERE namespace_id=$1 AND generation=$2 AND ($3::uuid IS NULL OR request_id>$3) ORDER BY request_id LIMIT $4")
            .bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(after).bind((limit+1) as i64).fetch_all(&mut *tx).await?;
        let more = rows.len() > limit;
        let mut items = Vec::with_capacity(limit);
        for row in rows.into_iter().take(limit) {
            let receipt: Option<serde_json::Value> = row.get("receipt");
            let receipt = receipt
                .map(serde_json::from_value::<aq_playbook_host::runner::KeyedAdmissionReceipt>)
                .transpose()
                .map_err(|_| Error::Unknown)?;
            if let Some(r) = &receipt
                && (r.key != row.get::<String, _>("engine_key")
                    || r.request_digest != row.get::<String, _>("request_digest")
                    || i64::try_from(r.activation_revision).ok()
                        != Some(row.get::<i64, _>("expected_activation_revision")))
            {
                return Err(Error::Conflict);
            }
            items.push(AdmissionView {
                request_id: row.get("request_id"),
                status: row.get("status"),
                created_at_ms: row.get("created_at_ms"),
                run_id: receipt.as_ref().map(|r| r.run_id.clone()),
                activation_revision: receipt.as_ref().map(|r| r.activation_revision.to_string()),
                catalog_revision: receipt.as_ref().map(|r| r.catalog_revision.to_string()),
            });
        }
        let next_cursor = if more {
            items.last().map(|r| r.request_id.to_string())
        } else {
            None
        };
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
