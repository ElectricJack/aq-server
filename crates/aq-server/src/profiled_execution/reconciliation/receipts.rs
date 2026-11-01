use super::*;
use aq_admission::Text;
pub(super) async fn preparation(
    tx: &mut Transaction<'_, Postgres>,
    binding: &LaunchBinding,
    hot: &HotState,
    node: Uuid,
) -> Result<(PreparationPlan, Digest), AppError> {
    let operation = hot.preparation_operation_id.ok_or_else(invalid)?;
    let value:serde_json::Value=sqlx::query_scalar("SELECT response FROM profiled_execution_receipts WHERE node_id=$1 AND operation_id=$2 AND kind='accept'").bind(node).bind(operation).fetch_optional(&mut **tx).await.map_err(db)?.ok_or_else(invalid)?;
    let doc: TransportDocument = serde_json::from_value(value).map_err(|_| invalid())?;
    doc.validate().map_err(|_| invalid())?;
    let TransportDocument::PreparationPlan(plan) = doc else {
        return Err(invalid());
    };
    let owner = hot.owner.as_ref().ok_or_else(invalid)?;
    if plan.binding != *binding
        || plan.operation_id != operation
        || plan.guardian_id != owner.guardian_id
        || plan.launch_id != owner.launch_id
        || plan.reservation.reservation_id != owner.reservation_id
    {
        return Err(invalid());
    }
    let digest = aq_selection::digest(&plan).map_err(|_| invalid())?;
    Ok((plan, digest))
}
async fn started_request(
    tx: &mut Transaction<'_, Postgres>,
    binding: &LaunchBinding,
    hot: &HotState,
) -> Result<Option<aq_admission::started::Request>, AppError> {
    let row=sqlx::query("SELECT operation_id,request_bytes,request_digest,receipt_bytes FROM profiled_started_receipts WHERE attempt_id=$1").bind(binding.attempt_id).fetch_optional(&mut **tx).await.map_err(db)?;
    let Some(row) = row else { return Ok(None) };
    let bytes: Vec<u8> = row.get("request_bytes");
    if Digest::of(&bytes).as_str() != row.get::<String, _>("request_digest") {
        return Err(invalid());
    }
    let request = aq_admission::started::decode_request(&bytes).map_err(|_| invalid())?;
    aq_admission::started::decode_receipt(&row.get::<Vec<u8>, _>("receipt_bytes"), &request)
        .map_err(|_| invalid())?;
    let owner = hot.owner.as_ref().ok_or_else(invalid)?;
    if request.binding != *binding
        || request.operation_id != row.get::<Uuid, _>("operation_id")
        || hot
            .grant
            .as_ref()
            .is_none_or(|g| g.grant_id != request.grant_id)
        || request.guardian_id != owner.guardian_id
        || request.launch_id != owner.launch_id
        || request.reservation_id != owner.reservation_id
    {
        return Err(invalid());
    }
    Ok(Some(request))
}
pub(super) async fn started(
    tx: &mut Transaction<'_, Postgres>,
    binding: &LaunchBinding,
    hot: &HotState,
) -> Result<Option<Uuid>, AppError> {
    Ok(started_request(tx, binding, hot)
        .await?
        .map(|r| r.operation_id))
}
pub(super) async fn stopped(
    tx: &mut Transaction<'_, Postgres>,
    binding: &LaunchBinding,
    hot: &HotState,
) -> Result<ArtifactRef, AppError> {
    let started = started_request(tx, binding, hot)
        .await?
        .ok_or_else(invalid)?;
    let row=sqlx::query("SELECT operation_id,request_bytes,request_digest,receipt_bytes FROM profiled_stopped_receipts WHERE attempt_id=$1").bind(binding.attempt_id).fetch_optional(&mut **tx).await.map_err(db)?.ok_or_else(invalid)?;
    let bytes: Vec<u8> = row.get("request_bytes");
    if Digest::of(&bytes).as_str() != row.get::<String, _>("request_digest") {
        return Err(invalid());
    }
    let request = aq_admission::stopped::decode_request(&bytes).map_err(|_| invalid())?;
    request.validate_after(&started).map_err(|_| invalid())?;
    if request.evidence.binding != *binding
        || request.evidence.stop_operation_id != row.get::<Uuid, _>("operation_id")
    {
        return Err(invalid());
    }
    let receipt =
        aq_admission::stopped::decode_receipt(&row.get::<Vec<u8>, _>("receipt_bytes"), &request)
            .map_err(|_| invalid())?;
    let reference = receipt.artifact().map_err(|_| invalid())?;
    if !matches!(&hot.stop,Stop::Stopped {receipt} if *receipt==reference) {
        return Err(invalid());
    }
    Ok(reference)
}
pub(super) async fn never_granted(
    tx: &mut Transaction<'_, Postgres>,
    binding: &LaunchBinding,
) -> Result<ArtifactRef, AppError> {
    // The actual owning SQL predicate checks retained closure, original binding,
    // absence of issued authority/effects and revoked preparation materials.
    let row=sqlx::query("SELECT closure_id,operation_id,proof_bytes,proof_digest,closed_at_ms,reason,observed_record_revision FROM profiled_never_granted_closures WHERE attempt_id=$1 AND profiled_never_granted_valid($1)").bind(binding.attempt_id).fetch_optional(&mut **tx).await.map_err(db)?.ok_or_else(invalid)?;
    let bytes: Vec<u8> = row.get("proof_bytes");
    let digest = Digest::of(&bytes);
    if bytes.len() > 16_384 || digest.as_str() != row.get::<String, _>("proof_digest") {
        return Err(invalid());
    }
    let proof: super::super::no_grant::ClosureProof =
        serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    if proof.schema_version != 1
        || proof.binding != *binding
        || proof.closure_id != row.get::<Uuid, _>("closure_id")
        || proof.operation_id != row.get::<Uuid, _>("operation_id")
        || proof.closed_at_ms != row.get::<i64, _>("closed_at_ms") as u64
        || proof.observed_record_revision != row.get::<i64, _>("observed_record_revision") as u64
        || proof.reason != row.get::<String, _>("reason")
        || proof.closed_at_ms < proof.applicable_deadline_ms
        || proof.applicable_deadline_ms == 0
    {
        return Err(invalid());
    }
    Ok(ArtifactRef {
        id: Text::new(format!("aq.never-granted/{}", proof.closure_id)).map_err(|_| invalid())?,
        version: Text::new("1").map_err(|_| invalid())?,
        digest,
    })
}
