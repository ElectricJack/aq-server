//! Actual refresh retrieval provenance; never a legacy or live execution DTO.
include!("../accepted_preparation/authority_core.rs");

pub(crate) struct PreparationAuthority {
    receipt: AcceptedReceipt,
    receipt_digest: aq_admission::Digest,
}
impl PreparationAuthority {
    pub fn origin(&self) -> (&'static str, &aq_admission::Digest) {
        (
            "accepted_preparation_registry_refresh_v1",
            &self.receipt_digest,
        )
    }
    pub fn window(&self) -> (u64, u64) {
        (
            self.receipt.lease.issued_at_ms,
            self.receipt.lease.expires_at_ms,
        )
    }
    pub fn materials(&self) -> &[aq_admission::MaterialLease] {
        &self.receipt.lease.materials
    }
}
pub(crate) async fn load_preparing_locked(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    auth: &AuthenticatedNode,
    binding: &LaunchBinding,
    hot: &HotState,
) -> Result<PreparationAuthority, AppError> {
    if hot.status != Status::Preparing || hot.grant.is_some() {
        return Err(refused());
    }
    let operation = hot.preparation_operation_id.ok_or_else(|| refused())?;
    if !super::policy::enabled(state) {
        return Err(refused());
    }
    let proposal = proposal_locked(tx, binding).await?;
    let raw: serde_json::Value = sqlx::query_scalar("SELECT request FROM profiled_v2_accepts WHERE attempt_id=$1 AND node_id=$2 AND operation_id=$3 FOR SHARE")
        .bind(binding.attempt_id).bind(auth.id()).bind(operation).fetch_optional(&mut **tx).await.map_err(db)?.ok_or_else(|| refused())?;
    let request: AcceptRequest = serde_json::from_value(raw).map_err(|_| refused())?;
    let receipt = accepted_locked(tx, &proposal, &request)
        .await?
        .ok_or_else(|| refused())?;
    live_locked(state, tx, auth, &proposal, &request, &receipt).await?;
    let receipt_digest = receipt.digest().map_err(|_| refused())?;
    Ok(PreparationAuthority {
        receipt,
        receipt_digest,
    })
}
