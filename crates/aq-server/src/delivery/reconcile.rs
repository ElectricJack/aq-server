//! aq-git §10.3 ambiguity handling. A dispatched publish without an
//! acknowledgement keeps its destination held. Only a new operation ID may
//! observe (`operation.reconcile`) or, as an explicit audited owner action,
//! resolve (`operation.resolve`) it. Neither path ever resends the publish:
//! the original operation ID stays fenced by `delivery_publish_dispatches`.
use super::*;
use protocol::ReconcileResult;

#[derive(Debug, Clone, Serialize)]
pub struct ReconcileRequest {
    /// A new operation ID; each reconcile UUID freezes one observation.
    pub context: OperationContext,
    pub target_operation_id: Uuid,
    /// Read from `operation_show` of the target, never guessed.
    pub expected_generation: u64,
}
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResolveDisposition {
    Abandon,
    PermitNewOperation,
}
/// Owner-only. The intent row (inputs, reason, evidence) and its receipt are
/// the immutable audit record. Remote effect of the original stays unknown.
#[derive(Debug, Clone, Serialize)]
pub struct ResolveRequest {
    pub context: OperationContext,
    pub target_operation_id: Uuid,
    pub expected_generation: u64,
    pub disposition: ResolveDisposition,
    pub reason: String,
    pub old_publishers_stopped: bool,
    pub evidence: Value,
}
pub(super) fn evidence(v: &Value) -> bool {
    v.as_str().is_some_and(|s| !s.trim().is_empty()) || v.as_object().is_some_and(|o| !o.is_empty())
}
impl DeliveryService {
    pub async fn reconcile(&self, r: ReconcileRequest) -> Result<Receipt<ReconcileResult>> {
        let params = json!({"target_operation_id":r.target_operation_id,"expected_generation":r.expected_generation});
        self.settle(
            &r.context,
            "operation.reconcile",
            serde_json::to_value(&r)?,
            r.target_operation_id,
            r.expected_generation,
            params,
        )
        .await
    }
    pub async fn resolve(&self, r: ResolveRequest) -> Result<Receipt<ReconcileResult>> {
        if !r.old_publishers_stopped
            || r.reason.trim().is_empty()
            || r.reason.len() > 4096
            || !evidence(&r.evidence)
        {
            return Err(DeliveryError::InvalidInput(
                "resolution requires reason, evidence and old_publishers_stopped=true",
            ));
        }
        let params = json!({"target_operation_id":r.target_operation_id,"expected_generation":r.expected_generation,
            "disposition":r.disposition,"reason":r.reason,"old_publishers_stopped":true,"evidence":r.evidence});
        self.settle(
            &r.context,
            "operation.resolve",
            serde_json::to_value(&r)?,
            r.target_operation_id,
            r.expected_generation,
            params,
        )
        .await
    }
    async fn settle(
        &self,
        c: &OperationContext,
        operation: &str,
        inputs: Value,
        target: Uuid,
        generation: u64,
        params: Value,
    ) -> Result<Receipt<ReconcileResult>> {
        if target.is_nil()
            || target == c.operation_id
            || generation == 0
            || generation > i64::MAX as u64
        {
            return Err(DeliveryError::InvalidInput(
                "reconciliation needs its own operation ID and the target generation",
            ));
        }
        let (mut tx, old) = self.prepare(c, operation, &inputs).await?;
        let intent = if let Some(old) = old {
            old
        } else {
            let known = sqlx::query_scalar::<_, i32>("SELECT 1 FROM delivery_intents WHERE operation_id=$1 AND operation='publish' AND repository_id=$2 AND binding_generation=$3")
                .bind(target).bind(c.repository_id).bind(c.binding_generation as i64)
                .fetch_optional(&mut *tx).await?;
            if known.is_none() {
                return Err(DeliveryError::InvalidInput(
                    "target is not a publication of this repository binding",
                ));
            }
            Self::freeze(&mut tx, c, operation, &inputs, params).await?
        };
        tx.commit().await?;
        let receipt: Receipt<ReconcileResult> = self.execute(c, operation, intent).await?;
        // Replays reach here too, so a crash after the receipt still releases.
        // Indeterminate and error replies keep the destination held.
        if receipt
            .outcome
            .as_ref()
            .is_ok_and(ReconcileResult::is_settled)
        {
            sqlx::query("DELETE FROM delivery_destinations WHERE operation_id=$1")
                .bind(target)
                .execute(&self.pool)
                .await?;
        }
        Ok(receipt)
    }
}
