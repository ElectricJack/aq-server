//! Exact identity and receipt checks for retained human decisions.
use crate::{Error, Result, intent_store::StoredIntent};
use serde_json::{Value, json};
use sqlx::{PgConnection, Row, postgres::PgRow};
use uuid::Uuid;

impl StoredIntent<'_> {
    pub async fn prior_question(
        &self,
        db: &mut PgConnection,
        operation: Uuid,
        receipt: &Value,
    ) -> Result<bool> {
        let row=sqlx::query("SELECT request_digest,operation_id,receipt FROM playbook_effects WHERE namespace_id=$1 AND generation=$2 AND intent_id=$3").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(&self.intent.id).fetch_optional(db).await?;
        let Some(row) = row else {
            return Ok(false);
        };
        self.verify_question(&row, receipt)?;
        if row.get::<Uuid, _>("operation_id") != operation {
            return Err(Error::Conflict);
        }
        Ok(true)
    }
    pub async fn decision_row(&self, db: &mut PgConnection) -> Result<Option<PgRow>> {
        Ok(sqlx::query("SELECT command_name,request_digest,status,receipt FROM playbook_effects WHERE namespace_id=$1 AND generation=$2 AND intent_id=$3").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(&self.intent.id).fetch_optional(db).await?)
    }
    pub fn reconcile_question(&self, row: &PgRow, receipt: &Value) -> Result<()> {
        if row.get::<String, _>("command_name") != "decision.request" {
            return Err(Error::Conflict);
        }
        self.verify_question(row, receipt)
    }
    pub async fn question_exists(&self, db: &mut PgConnection, receipt: &Value) -> Result<bool> {
        let row=sqlx::query("SELECT request_digest,receipt FROM playbook_effects WHERE namespace_id=$1 AND generation=$2 AND intent_id=$3").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(&self.intent.id).fetch_optional(db).await?;
        let Some(row) = row else {
            return Ok(false);
        };
        self.verify_question(&row, receipt)?;
        Ok(true)
    }
    fn verify_question(&self, row: &PgRow, receipt: &Value) -> Result<()> {
        if row.get::<String, _>("request_digest") != self.digest
            || row.get::<Value, _>("receipt") != *receipt
        {
            return Err(Error::Conflict);
        }
        Ok(())
    }
}
pub fn decision_receipt(decision: Uuid, subject: &aq_session_protocol::ArtifactRef) -> Value {
    json!({"handle":decision.to_string(),"subject":subject})
}
