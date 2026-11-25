use super::{Result, bad, conflict, internal};
use serde::{Serialize, de::DeserializeOwned};
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

/// One node-scoped operation and the digest of its original request.
pub(crate) struct Operation {
    node: Uuid,
    id: Uuid,
    digest: String,
}
impl Operation {
    pub(crate) fn new(node: Uuid, id: Uuid, digest: String) -> Self {
        Self { node, id, digest }
    }
    pub(crate) async fn replay<T: DeserializeOwned>(
        &self,
        tx: &mut Transaction<'_, Postgres>,
    ) -> Result<Option<T>> {
        if self.id.is_nil() {
            return Err(bad("operation id required"));
        }
        let row = sqlx::query(
        "SELECT payload_digest,response::text AS response FROM execution_operations WHERE node_id=$1 AND operation_id=$2",
    )
    .bind(self.node)
    .bind(self.id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(internal)?;
        if let Some(r) = row {
            if r.get::<&str, _>("payload_digest") != self.digest {
                return Err(conflict("operation id payload conflict"));
            }
            return Ok(Some(
                serde_json::from_str(r.get("response")).map_err(internal)?,
            ));
        }
        Ok(None)
    }
    pub(crate) async fn record<T: Serialize>(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        r: &T,
    ) -> Result<()> {
        sqlx::query("INSERT INTO execution_operations(node_id,operation_id,payload_digest,response) VALUES($1,$2,$3,$4::jsonb)").bind(self.node).bind(self.id).bind(&self.digest).bind(serde_json::to_string(r).map_err(internal)?).execute(&mut **tx).await.map_err(internal)?;
        Ok(())
    }
}
