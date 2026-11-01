use super::{Error, Owner, Result, db, now, sql};
use uuid::Uuid;
impl Owner {
    /// Bounded store-owner maintenance. It borrows no human identity. An expired
    /// SQL lease never overrides a still-live physical reader/writer guard.
    pub async fn cleanup(&self, limit: usize) -> Result<usize> {
        if !(1..=16).contains(&limit) {
            return Err(Error::Invalid);
        }
        self.root.check()?;
        let at = now(&self.state)?;
        let ids:Vec<Uuid>=sqlx::query_scalar("SELECT operation_id FROM ((SELECT operation_id FROM result_media_intents WHERE storage_id=$1 AND state IN ('reserved','received','finalizing','expired') AND cleanup_state<>'done' AND upload_expires_at_ms<=$2 ORDER BY upload_expires_at_ms,operation_id LIMIT $3) UNION ALL (SELECT operation_id FROM result_media_intents WHERE storage_id=$1 AND state='rejected' AND cleanup_state<>'done' AND transition_at_ms<=$2 ORDER BY upload_expires_at_ms,operation_id LIMIT $3) UNION ALL (SELECT operation_id FROM result_media_intents WHERE storage_id=$1 AND state='retained' AND cleanup_state<>'done' AND retain_until_ms<=$2 ORDER BY retain_until_ms,operation_id LIMIT $3)) eligible LIMIT $3")
            .bind(self.root.id).bind(at as i64).bind(limit as i64).fetch_all(&self.state.store.pool).await.map_err(sql)?;
        let mut done = 0;
        for id in ids {
            let guard = match self.root.active(id, true) {
                Ok(g) => g,
                Err(Error::Busy) => continue,
                Err(e) => return Err(e),
            };
            let mut tx = db::transaction(&self.state).await?;
            db::budgets(&mut tx, None).await?;
            let row = db::load(&mut tx, id).await?;
            db::budgets(&mut tx, Some(row.binding.project_id.as_uuid())).await?;
            let at = now(&self.state)?;
            let eligible = if row.state == super::media::UploadState::Retained {
                row.retain
            } else if row.state == super::media::UploadState::Rejected {
                row.transition
            } else {
                row.upload
            };
            if row.storage != self.root.id
                || row.cleanup == "done"
                || at < eligible
                || row.lease_expires.is_some_and(|v| at < v)
            {
                continue;
            }
            let generation = Uuid::new_v4();
            // Expired unretained work no longer owns an attachment slot. Its
            // byte reservation remains charged until physical absence is
            // confirmed below. Retained receipts/tombstones stay Retained.
            sqlx::query("UPDATE result_media_intents SET state=CASE WHEN state IN ('reserved','received','finalizing') THEN 'expired' ELSE state END,cleanup_state='running',cleanup_generation=$2,lease_id=NULL,lease_expires_at_ms=NULL,transition_at_ms=$3 WHERE operation_id=$1")
                .bind(id).bind(generation).bind(at as i64).execute(&mut *tx).await.map_err(sql)?;
            tx.commit().await.map_err(sql)?;
            let root = self.root.clone();
            let (removed, guard) = tokio::task::spawn_blocking(move || {
                let result = root.remove(id, false).and_then(|()| root.remove(id, true));
                (result, guard)
            })
            .await
            .map_err(|_| Error::Storage)?;
            let mut tx = db::transaction(&self.state).await?;
            db::budgets(&mut tx, None).await?;
            let row = db::load(&mut tx, id).await?;
            db::budgets(&mut tx, Some(row.binding.project_id.as_uuid())).await?;
            self.root.check()?;
            let changed=sqlx::query(if removed.is_ok(){"UPDATE result_media_intents SET cleanup_state='done',cleanup_completed_at_ms=$3,reserved_bytes=0,retained_bytes=0,transition_at_ms=$3 WHERE operation_id=$1 AND cleanup_state='running' AND cleanup_generation=$2"}else{"UPDATE result_media_intents SET cleanup_state='blocked',transition_at_ms=$3 WHERE operation_id=$1 AND cleanup_state='running' AND cleanup_generation=$2"})
                .bind(id).bind(generation).bind(now(&self.state)? as i64).execute(&mut *tx).await.map_err(sql)?.rows_affected();
            tx.commit().await.map_err(sql)?;
            drop(guard);
            if removed.is_ok() && changed == 1 {
                done += 1;
            }
        }
        Ok(done)
    }
}
