//! Actual immutable phase provenance and retrieval dispatch.
include!("authority_core.rs");
/// Immutable historical origin, with current live retrieval fencing performed by
/// the loader. This is not execution authority and cannot reconstruct bytes.
enum OriginalPreparation {
    Legacy {
        plan: Box<aq_admission::transport::PreparationPlan>,
        receipt_digest: aq_admission::Digest,
    },
    AcceptedV2 {
        receipt: Box<AcceptedReceipt>,
        receipt_digest: aq_admission::Digest,
    },
    Refresh(Box<super::super::accepted_refresh::PreparationAuthority>),
}
/// Opaque loaded provenance: DTOs exposed by accessors remain historical data,
/// not execution authority. Only the authenticated locked loader constructs it.
pub struct PreparationAuthority {
    original: OriginalPreparation,
}
impl PreparationAuthority {
    pub fn origin(&self) -> (&'static str, &aq_admission::Digest) {
        match &self.original {
            OriginalPreparation::Legacy { receipt_digest, .. } => ("accept", receipt_digest),
            OriginalPreparation::AcceptedV2 { receipt_digest, .. } => {
                ("accepted_preparation_v2", receipt_digest)
            }
            OriginalPreparation::Refresh(value) => value.origin(),
        }
    }
    pub fn window(&self) -> (u64, u64) {
        match &self.original {
            OriginalPreparation::Legacy { plan, .. } => {
                (plan.clock.server_time_ms, plan.clock.expires_at_ms)
            }
            OriginalPreparation::AcceptedV2 { receipt, .. } => {
                (receipt.lease.issued_at_ms, receipt.lease.expires_at_ms)
            }
            OriginalPreparation::Refresh(value) => value.window(),
        }
    }
    pub fn materials(&self) -> &[aq_admission::MaterialLease] {
        match &self.original {
            OriginalPreparation::Legacy { plan, .. } => &plan.materials,
            OriginalPreparation::AcceptedV2 { receipt, .. } => &receipt.lease.materials,
            OriginalPreparation::Refresh(value) => value.materials(),
        }
    }
}
/// Caller already holds authenticated node and exact attempt/hot locks. Legacy
/// bytes are never converted into v2, nor v2 into a legacy plan.
pub(crate) async fn load_preparing_locked(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    auth: &AuthenticatedNode,
    b: &LaunchBinding,
    hot: &HotState,
) -> Result<PreparationAuthority, AppError> {
    let operation = hot.preparation_operation_id.ok_or_else(|| refused())?;
    let owner = hot.owner.as_ref().ok_or_else(|| refused())?;
    if hot.status != Status::Preparing || hot.grant.is_some() {
        return Err(refused());
    }
    if let Some(raw)=sqlx::query_scalar::<_,serde_json::Value>("SELECT response FROM profiled_execution_receipts WHERE node_id=$1 AND operation_id=$2 AND kind='accept' FOR SHARE").bind(auth.id()).bind(operation).fetch_optional(&mut **tx).await.map_err(db)?{
  let doc:aq_admission::transport::TransportDocument=serde_json::from_value(raw.clone()).map_err(|_|refused())?;doc.validate().map_err(|_|refused())?;
  let aq_admission::transport::TransportDocument::PreparationPlan(plan)=doc else{return Err(refused())};
  if plan.binding!=*b||plan.operation_id!=operation||plan.guardian_id!=owner.guardian_id||plan.launch_id!=owner.launch_id||plan.reservation.reservation_id!=owner.reservation_id{return Err(refused())}
  return Ok(PreparationAuthority{original:OriginalPreparation::Legacy{plan:Box::new(plan),receipt_digest:aq_selection::digest(&raw).map_err(|_|AppError::Internal)?}})
 }
    if super::super::phase_profile::for_attempt_locked(tx, b.attempt_id).await?
        == Some(super::super::phase_profile::Profile::Refresh)
    {
        let refresh =
            super::super::accepted_refresh::load_preparing_locked(state, tx, auth, b, hot).await?;
        return Ok(PreparationAuthority {
            original: OriginalPreparation::Refresh(Box::new(refresh)),
        });
    }
    if !super::policy::enabled(state) {
        return Err(refused());
    }
    let p = proposal_locked(tx, b).await?;
    let raw:serde_json::Value=sqlx::query_scalar("SELECT request FROM profiled_v2_accepts WHERE attempt_id=$1 AND node_id=$2 AND operation_id=$3 FOR SHARE").bind(b.attempt_id).bind(auth.id()).bind(operation).fetch_optional(&mut **tx).await.map_err(db)?.ok_or_else(|| refused())?;
    let q: AcceptRequest = serde_json::from_value(raw).map_err(|_| refused())?;
    let a = accepted_locked(tx, &p, &q)
        .await?
        .ok_or_else(|| refused())?;
    live_locked(state, tx, auth, &p, &q, &a).await?;
    let digest = a.digest().map_err(|_| refused())?;
    Ok(PreparationAuthority {
        original: OriginalPreparation::AcceptedV2 {
            receipt: Box::new(a),
            receipt_digest: digest,
        },
    })
}
