//! Close expired attachment generations after proving physical absence.

use super::{Error, Owner, Result, db, files, media::UploadState, now, sql};
use uuid::Uuid;

const CANDIDATES: &str = "SELECT operation_id FROM ((SELECT operation_id FROM result_media_intents WHERE storage_id=$1 AND state IN ('reserved','received','finalizing','expired') AND cleanup_state<>'done' AND upload_expires_at_ms<=$2 ORDER BY upload_expires_at_ms,operation_id LIMIT $3) UNION ALL (SELECT operation_id FROM result_media_intents WHERE storage_id=$1 AND state='rejected' AND cleanup_state<>'done' AND transition_at_ms<=$2 ORDER BY upload_expires_at_ms,operation_id LIMIT $3) UNION ALL (SELECT operation_id FROM result_media_intents WHERE storage_id=$1 AND state='retained' AND cleanup_state<>'done' AND retain_until_ms<=$2 ORDER BY retain_until_ms,operation_id LIMIT $3)) eligible LIMIT $3";
const CLAIM: &str = "UPDATE result_media_intents SET state=CASE WHEN state IN ('reserved','received','finalizing') THEN 'expired' ELSE state END,cleanup_state='running',cleanup_generation=$2,lease_id=NULL,lease_expires_at_ms=NULL,transition_at_ms=$3 WHERE operation_id=$1";
const COMPLETE: &str = "UPDATE result_media_intents SET cleanup_state='done',cleanup_completed_at_ms=$3,reserved_bytes=0,retained_bytes=0,transition_at_ms=$3 WHERE operation_id=$1 AND cleanup_state='running' AND cleanup_generation=$2";
const BLOCK: &str = "UPDATE result_media_intents SET cleanup_state='blocked',transition_at_ms=$3 WHERE operation_id=$1 AND cleanup_state='running' AND cleanup_generation=$2";

impl Owner {
    /// Bounded store-owner maintenance. It borrows no human identity. An expired
    /// SQL lease never overrides a still-live physical reader/writer guard.
    pub async fn cleanup(&self, limit: usize) -> Result<usize> {
        if !(1..=16).contains(&limit) {
            return Err(Error::Invalid);
        }
        self.root.check()?;
        let at = now(&self.state)?;
        let ids: Vec<Uuid> = sqlx::query_scalar(CANDIDATES)
            .bind(self.root.id)
            .bind(at as i64)
            .bind(limit as i64)
            .fetch_all(&self.state.store.pool)
            .await
            .map_err(sql)?;
        let mut done = 0;
        for id in ids {
            done += usize::from(self.cleanup_one(id).await?);
        }
        Ok(done)
    }

    async fn cleanup_one(&self, id: Uuid) -> Result<bool> {
        let guard = match self.root.active(id, true) {
            Ok(guard) => guard,
            Err(Error::Busy) => return Ok(false),
            Err(error) => return Err(error),
        };
        let Some(generation) = self.claim_cleanup(id).await? else {
            return Ok(false);
        };
        let (absent, guard) = self.remove_generations(id, guard).await?;
        let applied = self.record_cleanup(id, generation, absent).await?;
        drop(guard);
        Ok(absent && applied)
    }

    async fn claim_cleanup(&self, id: Uuid) -> Result<Option<Uuid>> {
        let mut tx = db::transaction(&self.state).await?;
        db::budgets(&mut tx, None).await?;
        let row = db::load(&mut tx, id).await?;
        db::budgets(&mut tx, Some(row.binding.project_id.as_uuid())).await?;
        let at = now(&self.state)?;
        if !eligible(&row, self.root.id, at) {
            return Ok(None);
        }
        let generation = Uuid::new_v4();
        // Expired unretained work gives back its slot. Its bytes remain
        // charged until both generations are confirmed absent.
        sqlx::query(CLAIM)
            .bind(id)
            .bind(generation)
            .bind(at as i64)
            .execute(&mut *tx)
            .await
            .map_err(sql)?;
        tx.commit().await.map_err(sql)?;
        Ok(Some(generation))
    }

    async fn remove_generations(
        &self,
        id: Uuid,
        guard: files::ActivityGuard,
    ) -> Result<(bool, files::ActivityGuard)> {
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || {
            let absent = root
                .remove(id, false)
                .and_then(|()| root.remove(id, true))
                .is_ok();
            (absent, guard)
        })
        .await
        .map_err(|_| Error::Storage)
    }

    async fn record_cleanup(&self, id: Uuid, generation: Uuid, absent: bool) -> Result<bool> {
        let mut tx = db::transaction(&self.state).await?;
        db::budgets(&mut tx, None).await?;
        let row = db::load(&mut tx, id).await?;
        db::budgets(&mut tx, Some(row.binding.project_id.as_uuid())).await?;
        self.root.check()?;
        let changed = sqlx::query(if absent { COMPLETE } else { BLOCK })
            .bind(id)
            .bind(generation)
            .bind(now(&self.state)? as i64)
            .execute(&mut *tx)
            .await
            .map_err(sql)?
            .rows_affected();
        tx.commit().await.map_err(sql)?;
        Ok(changed == 1)
    }
}

fn eligible(row: &db::Intent, storage: Uuid, at: u64) -> bool {
    let expires = match row.state {
        UploadState::Retained => row.retain,
        UploadState::Rejected => row.transition,
        _ => row.upload,
    };
    let local_generation = row.storage == storage && row.cleanup != "done";
    let expired = at >= expires && row.lease_expires.is_none_or(|end| at >= end);
    local_generation && expired
}
