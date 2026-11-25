use super::*;
impl Report<'_> {
    pub(super) async fn require_boot(&self, tx: &mut Tx<'_>) -> Result<()> {
        let current: Value =
            sqlx::query_scalar("SELECT state FROM execution_nodes WHERE node_id=$1 FOR SHARE")
                .bind(self.identity.node_id)
                .fetch_optional(&mut **tx)
                .await
                .map_err(db)?
                .ok_or_else(|| conflict("execution boot must be registered"))?;
        if current["identity"]["boot_id"] != self.identity.boot_id.to_string() {
            return Err(conflict("inventory boot is stale"));
        }
        Ok(())
    }

    pub(super) async fn receipt(&self, tx: &mut Tx<'_>) -> Result<Option<Value>> {
        let row=sqlx::query("SELECT kind,payload_digest,response FROM execution_receipts WHERE node_id=$1 AND operation_id=$2")
            .bind(self.identity.node_id).bind(self.request.operation_id).fetch_optional(&mut **tx).await.map_err(db)?;
        let Some(row) = row else {
            return Ok(None);
        };
        if row.get::<String, _>("kind") != "intelligence_inventory"
            || row.get::<String, _>("payload_digest") != self.digest.as_str()
        {
            return Err(conflict("inventory operation ID conflict"));
        }
        Ok(Some(row.get("response")))
    }

    pub(super) async fn next_revision(&self, tx: &mut Tx<'_>) -> Result<u64> {
        let old = sqlx::query(
            "SELECT boot_id,sequence,revision FROM intelligence_node_snapshots WHERE node_id=$1",
        )
        .bind(self.identity.node_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(db)?;
        let Some(row) = old else {
            return Ok(1);
        };
        let same_boot = row.get::<Uuid, _>("boot_id") == self.identity.boot_id;
        if same_boot && row.get::<i64, _>("sequence") as u64 >= self.request.sequence {
            return Err(conflict("inventory sequence must increase"));
        }
        (row.get::<i64, _>("revision") as u64)
            .checked_add(1)
            .filter(|v| *v <= i64::MAX as u64)
            .ok_or(AppError::Internal)
    }

    pub(super) async fn persist(&self, tx: &mut Tx<'_>, snapshot: &Snapshot) -> Result<()> {
        let revision = snapshot.revision;
        let value = &snapshot.value;
        let id = self.identity.node_id;
        let boot = self.identity.boot_id;
        sqlx::query("INSERT INTO intelligence_node_snapshots(node_id,boot_id,sequence,revision,snapshot) VALUES($1,$2,$3,$4,$5) ON CONFLICT(node_id) DO UPDATE SET boot_id=excluded.boot_id,sequence=excluded.sequence,revision=excluded.revision,snapshot=excluded.snapshot")
            .bind(id).bind(boot).bind(self.request.sequence as i64).bind(revision as i64).bind(value).execute(&mut **tx).await.map_err(db)?;
        crate::profiled_execution::presence::record_legacy_inventory_locked(
            tx, id, boot, revision, value,
        )
        .await?;
        if let Some(storage) = &self.request.storage {
            crate::node_storage::record(tx, Some(id), storage, snapshot.window.observed_at_ms)
                .await?;
        }
        sqlx::query("INSERT INTO execution_receipts(node_id,operation_id,kind,payload_digest,response) VALUES($1,$2,'intelligence_inventory',$3,$4)")
            .bind(id).bind(self.request.operation_id).bind(self.digest.as_str()).bind(value).execute(&mut **tx).await.map_err(db)?;
        Ok(())
    }
}
