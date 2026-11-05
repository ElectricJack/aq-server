//! Acceptance reserves preparation only. Real private bytes are observed outside
//! SQL locks; the original plan, admitted lease and Reserved journal are atomic.
use super::{journal, material, registration, source_exports};
use crate::{
    AppState,
    error::AppError,
    execution::{
        authority::hot::{HotState, LaunchOwner},
        authority::{self, Policy},
        profiled::Status,
    },
    intelligence::resolver::{self, Decision},
    worker_api::AuthenticatedNode,
};
use aq_admission::{
    Digest, LaunchBinding, MaterialLease,
    control::{DeliveryStamp, PreparationDelivery, Versions},
    transport::{Accept, DeliveryClock, Feature, PreparationPlan, TransportDocument},
};
use aq_session_protocol::ProfiledOffer;
use serde_json::Value;
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;
fn refused() -> AppError {
    AppError::Conflict("native preparation is not currently available".into())
}
fn db(_: sqlx::Error) -> AppError {
    AppError::Internal
}
fn now(state: &AppState) -> Result<u64, AppError> {
    u64::try_from((state.clock)().timestamp_millis())
        .ok()
        .filter(|t| *t > 0)
        .ok_or_else(refused)
}
pub(super) struct Locked {
    pub(super) hot: HotState,
    pub(super) selection: aq_selection::ResolvedSelection,
    pub(super) source: source_exports::LinkedExport,
    pub(super) offer_expiry: u64,
}
pub(super) async fn locked(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    auth: &AuthenticatedNode,
    request: &Accept,
) -> Result<Locked, AppError> {
    crate::worker_api::lock_current_node(tx, auth).await?;
    let current =
        registration::current_runtime_locked(state, tx, auth.id(), request.identity.boot_id)
            .await?;
    let qualified = state
        .config
        .profiled_execution
        .as_ref()
        .and_then(|c| {
            c.qualified_runtimes
                .iter()
                .find(|r| r.runtime == current.runtime && r.qualification == current.qualification)
        })
        .ok_or_else(refused)?;
    let registration =
        registration::admitted_runtime_locked(tx, auth.id(), request.identity.boot_id).await?;
    if request.identity != registration.request.identity
        || request
            .acceptance
            .advertised_capabilities
            .iter()
            .any(|c| !registration.request.capabilities.contains(c))
    {
        return Err(refused());
    }
    if qualified.material_delivery.is_none()
        || [
            Feature::ArtifactTransferV1,
            Feature::ServerMaterialV1,
            Feature::DurableHostJournalV1,
        ]
        .iter()
        .any(|f| !qualified.features.contains(f) || !registration.request.features.contains(f))
    {
        return Err(refused());
    }
    let binding = &request.acceptance.binding;
    let view = authority::load_locked(state, tx, &binding.session, Policy::IdentityOnly).await?;
    let row=sqlx::query("SELECT a.state,l.decision_id,l.offer_expires_at_ms FROM execution_attempts a JOIN profiled_launches l USING(attempt_id) WHERE a.attempt_id=$1").bind(binding.attempt_id).fetch_one(&mut **tx).await.map_err(db)?;
    let value: Value = row.get("state");
    let offer: ProfiledOffer =
        serde_json::from_value(value.get("offer").cloned().ok_or_else(refused)?)
            .map_err(|_| refused())?;
    request
        .acceptance
        .validate_against(&offer)
        .map_err(|_| refused())?;
    aq_session_protocol::require_capabilities(
        &offer.required_capabilities,
        &qualified.capabilities,
    )
    .map_err(|_| refused())?;
    aq_session_protocol::require_capabilities(
        &offer.manifest.harness.required_capabilities,
        &qualified.capabilities,
    )
    .map_err(|_| refused())?;
    let raw: Value = sqlx::query_scalar(
        "SELECT decision FROM intelligence_decisions WHERE decision_id=$1 FOR SHARE",
    )
    .bind(row.get::<Uuid, _>("decision_id"))
    .fetch_one(&mut **tx)
    .await
    .map_err(db)?;
    let decision: Decision = serde_json::from_value(raw).map_err(|_| refused())?;
    resolver::fence_for_preparation(state, tx, &decision, binding).await?;
    let selection = decision.outcome.resolved.ok_or_else(refused)?;
    if offer.manifest.credentials.len() != 1
        || offer.manifest.credentials[0].credential_profile != selection.choice.credential_profile
        || offer.manifest.credentials[0].purpose.as_str() != "model-authentication"
    {
        return Err(refused());
    }

    if view.project_id != selection.lineage.project_id || view.task_id != selection.lineage.task_id
    {
        return Err(refused());
    }
    let hot: Value = sqlx::query_scalar(
        "SELECT state FROM profiled_authority_hot WHERE attempt_id=$1 FOR UPDATE",
    )
    .bind(binding.attempt_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(db)?;
    let hot: HotState = serde_json::from_value(hot).map_err(|_| refused())?;
    let source = source_exports::load_linked_locked(tx, binding.attempt_id).await?;
    if source.manifest().attempt != view.attempt
        || source.manifest().source != view.source.ok_or_else(refused)?
        || source.manifest().resolved_spec_digest != view.execution_spec_digest
        || source.token().request().project_id != view.project_id
        || source.token().request().project_revision != selection.project_revision
    {
        return Err(refused());
    }
    let offer_expiry = u64::try_from(row.get::<i64, _>("offer_expires_at_ms"))
        .map_err(|_| refused())?
        .min(selection.expires_at_ms);
    if now(state)? >= offer_expiry {
        return Err(refused());
    }
    Ok(Locked {
        hot,
        selection,
        source,
        offer_expiry,
    })
}
async fn begin(state: &AppState) -> Result<Transaction<'_, Postgres>, AppError> {
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    sqlx::query("SET LOCAL lock_timeout='2s'")
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    sqlx::query("SET LOCAL statement_timeout='3s'")
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    Ok(tx)
}
async fn original(
    tx: &mut Transaction<'_, Postgres>,
    auth: &AuthenticatedNode,
    request: &Accept,
) -> Result<Option<PreparationPlan>, AppError> {
    super::accepted_preparation::reject_legacy_operation_collision(
        tx,
        auth.id(),
        request.operation_id,
    )
    .await?;
    let row=sqlx::query("SELECT kind,request_digest,response FROM profiled_execution_receipts WHERE node_id=$1 AND operation_id=$2 FOR SHARE").bind(auth.id()).bind(request.operation_id).fetch_optional(&mut **tx).await.map_err(db)?;
    let Some(row) = row else { return Ok(None) };
    if row.get::<String, _>("kind") != "accept"
        || row.get::<String, _>("request_digest")
            != aq_selection::digest(request)
                .map_err(|_| AppError::Internal)?
                .as_str()
    {
        return Err(refused());
    }
    let doc: TransportDocument =
        serde_json::from_value(row.get("response")).map_err(|_| refused())?;
    doc.validate().map_err(|_| refused())?;
    let TransportDocument::PreparationPlan(plan) = doc else {
        return Err(refused());
    };
    Ok(Some(plan))
}
fn same_owner(hot: &HotState, plan: &PreparationPlan) -> bool {
    hot.status == Status::Preparing
        && hot.preparation_operation_id == Some(plan.operation_id)
        && hot.owner.as_ref().is_some_and(|o| {
            o.guardian_id == plan.guardian_id
                && o.launch_id == plan.launch_id
                && o.reservation_id == plan.reservation.reservation_id
        })
}
/// Current authenticated worker only. `nonce` belongs to this physical send and
/// is never inserted into the immutable mutation digest/receipt.
pub async fn accept(
    state: &AppState,
    auth: &AuthenticatedNode,
    request: &Accept,
    nonce: Uuid,
) -> Result<PreparationDelivery, AppError> {
    TransportDocument::Accept(request.clone())
        .validate()
        .map_err(|_| AppError::BadRequest("invalid native accept request".into()))?;
    if auth.id() != request.identity.node_id || nonce.is_nil() {
        return Err(AppError::Forbidden(
            "native preparation identity mismatch".into(),
        ));
    }
    let mut tx = begin(state).await?;
    let before = locked(state, &mut tx, auth, request).await?;
    let old = original(&mut tx, auth, request).await?;
    if old.as_ref().is_some_and(|p| !same_owner(&before.hot, p))
        || (old.is_none() && before.hot.status != Status::Offered)
    {
        return Err(refused());
    }
    let observed_at = now(state)?;
    let expiry = old.as_ref().map(|p| p.clock.expires_at_ms).unwrap_or(
        before.offer_expiry.min(
            observed_at
                .checked_add(request.reservation_remaining_ms)
                .ok_or_else(refused)?,
        ),
    );
    if expiry <= observed_at {
        return Err(refused());
    }
    let lease_id = old
        .as_ref()
        .and_then(|p| p.materials.first())
        .map(|l| l.lease_id)
        .unwrap_or_else(|| {
            Uuid::new_v5(
                &request.operation_id,
                request.acceptance.binding.attempt_id.as_bytes(),
            )
        });
    let issued = old
        .as_ref()
        .and_then(|p| p.materials.first())
        .map(|l| l.issued_at_ms)
        .unwrap_or(observed_at);
    tx.commit().await.map_err(db)?;
    let retention = crate::source_retention::profiled_base::inspect_retained(
        state,
        before.source.token(),
        before.source.fact(),
    )
    .await?;
    let registry = state.config.intelligence.clone().ok_or_else(refused)?;
    let selection = before.selection.clone();
    let binding = request.acceptance.binding.clone();
    let head = super::login_refresh::newest_source(
        &state.store.pool,
        &registry,
        &selection.choice.credential_profile,
        selection.credential_revision,
    )
    .await?;
    static READS: std::sync::OnceLock<std::sync::Arc<tokio::sync::Semaphore>> =
        std::sync::OnceLock::new();
    let permit = READS
        .get_or_init(|| std::sync::Arc::new(tokio::sync::Semaphore::new(2)))
        .clone()
        .try_acquire_owned()
        .map_err(|_| refused())?;
    let lease = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            material::issue_for_selection_from(
                &registry,
                &selection,
                &binding,
                lease_id,
                issued,
                expiry,
                None,
                head.as_ref(),
            )
        }),
    )
    .await
    .map_err(|_| refused())?
    .map_err(|_| AppError::Internal)?
    .map_err(|_| refused())?;
    let mut tx = begin(state).await?;
    let after = locked(state, &mut tx, auth, request).await?;
    if before.source.fact().canonical_bytes()? != after.source.fact().canonical_bytes()? {
        return Err(refused());
    }
    retention.validate_for(after.source.token())?;
    let time = now(state)?;
    if time < retention.observed_at_ms() || time >= retention.expires_at_ms() {
        return Err(refused());
    }
    crate::intelligence::fence_current_registry(state, &mut tx).await?;
    let plan = if let Some(plan) = original(&mut tx, auth, request).await? {
        if !same_owner(&after.hot, &plan)
            || plan.materials.len() != 1
            || !same_material(&plan.materials[0], &lease)
        {
            return Err(refused());
        }
        plan
    } else {
        if after.hot.status != Status::Offered || time >= expiry {
            return Err(refused());
        }
        admit(
            state,
            &mut tx,
            auth,
            request,
            after.hot,
            lease,
            DeliveryClock::new(time, expiry).map_err(|_| refused())?,
        )
        .await?
    };
    let row=sqlx::query("SELECT record_revision,authority_revision,grant_revision FROM profiled_authority_hot WHERE attempt_id=$1").bind(request.acceptance.binding.attempt_id).fetch_one(&mut *tx).await.map_err(db)?;
    let live_expiry = plan.clock.expires_at_ms;
    let reply = PreparationDelivery {
        schema_version: 1,
        original: plan,
        versions: Versions {
            record_revision: row.get::<i64, _>("record_revision") as u64,
            authority_revision: row.get::<i64, _>("authority_revision") as u64,
            grant_revision: row.get::<i64, _>("grant_revision") as u64,
        },
        delivery: DeliveryStamp {
            nonce,
            clock: DeliveryClock::new(now(state)?, live_expiry).map_err(|_| refused())?,
        },
    };
    reply.validate_for(request, nonce).map_err(|_| refused())?;
    tx.commit().await.map_err(db)?;
    Ok(reply)
}
fn same_material(a: &MaterialLease, b: &MaterialLease) -> bool {
    a.lease_id == b.lease_id
        && a.binding == b.binding
        && a.credential_profile == b.credential_profile
        && a.credential_revision == b.credential_revision
        && a.provider == b.provider
        && a.account_reference == b.account_reference
        && a.endpoint_reference == b.endpoint_reference
        && a.purpose == b.purpose
        && a.material_digest == b.material_digest
        && a.material_bytes == b.material_bytes
}
async fn admit(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    auth: &AuthenticatedNode,
    request: &Accept,
    mut hot: HotState,
    mut lease: MaterialLease,
    clock: DeliveryClock,
) -> Result<PreparationPlan, AppError> {
    let expiry = clock.expires_at_ms;
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended('aq-native-preparation-quota-v1',0))",
    )
    .execute(&mut **tx)
    .await
    .map_err(db)?;
    let issued = now(state)?;
    if issued < clock.server_time_ms {
        return Err(refused());
    }
    let clock = DeliveryClock::new(issued, expiry).map_err(|_| refused())?;
    // This is the first immutable publication, not a retiming of an old lease.
    lease.issued_at_ms = issued;
    lease.validate().map_err(|_| refused())?;
    let owner = LaunchOwner {
        guardian_id: request.guardian_id,
        launch_id: request.launch_id,
        reservation_id: request.reservation.reservation_id,
    };
    let plan = PreparationPlan {
        schema_version: 1,
        operation_id: request.operation_id,
        binding: request.acceptance.binding.clone(),
        reservation: request.reservation.clone(),
        guardian_id: request.guardian_id,
        launch_id: request.launch_id,
        clock,
        materials: vec![lease.clone()],
    };
    plan.validate().map_err(|_| refused())?;
    let response = TransportDocument::PreparationPlan(plan.clone());
    // Charge exactly the same JSONB text representation as retained rows.
    let lease_value = serde_json::to_value(&lease).map_err(|_| AppError::Internal)?;
    let response_value = serde_json::to_value(&response).map_err(|_| AppError::Internal)?;
    let (lease_bytes, response_bytes): (i32, i32) =
        sqlx::query_as("SELECT octet_length($1::jsonb::text),octet_length($2::jsonb::text)")
            .bind(&lease_value)
            .bind(&response_value)
            .fetch_one(&mut **tx)
            .await
            .map_err(db)?;
    let lease_bytes = u64::try_from(lease_bytes).map_err(|_| refused())?;
    let response_bytes = u64::try_from(response_bytes).map_err(|_| refused())?;
    if lease_bytes > 8192 || response_bytes > 16384 {
        return Err(refused());
    }
    let quota = retained_quota_locked(tx, auth.id()).await?;
    let incoming = lease_bytes
        .checked_add(response_bytes)
        .ok_or_else(refused)?;
    if quota.0 >= 4096
        || quota.1 >= 256
        || u64::try_from(quota.2)
            .ok()
            .and_then(|n| n.checked_add(incoming))
            .is_none_or(|n| n > 67108864)
        || u64::try_from(quota.3)
            .ok()
            .and_then(|n| n.checked_add(incoming))
            .is_none_or(|n| n > 4194304)
        || now(state)? >= expiry
    {
        return Err(refused());
    }

    super::closure::history::for_attempt_locked(tx, &request.acceptance.binding).await?;
    hot.status = Status::Preparing;
    hot.owner = Some(owner.clone());
    hot.preparation_operation_id = Some(request.operation_id);
    let binding: &LaunchBinding = &request.acceptance.binding;
    let verifier = Digest::of(Uuid::new_v4().as_bytes());
    sqlx::query("INSERT INTO admission_material_leases(lease_id,operation_id,request_digest,node_id,boot_id,prospective_attempt_id,admitted_attempt_id,offer_id,credential_identity,credential_revision,purpose,lease,verifier,state,issued_at_ms,expires_at_ms) VALUES($1,$2,$3,$4,$5,$6,$6,$7,$8,$9,$10,$11,$12,'admitted',$13,$14)")
        .bind(lease.lease_id).bind(request.operation_id).bind(aq_selection::digest(request).map_err(|_|AppError::Internal)?.as_str()).bind(auth.id()).bind(request.identity.boot_id).bind(binding.attempt_id).bind(binding.offer_id).bind(format!("{}@{}",lease.credential_profile.id.as_str(),lease.credential_profile.version.as_str())).bind(lease.credential_revision as i64).bind(lease.purpose.as_str()).bind(serde_json::to_value(&lease).map_err(|_|AppError::Internal)?).bind(verifier.as_str()).bind(lease.issued_at_ms as i64).bind(lease.expires_at_ms as i64).execute(&mut **tx).await.map_err(db)?;
    sqlx::query("INSERT INTO profiled_launch_materials(attempt_id,lease_id) VALUES($1,$2)")
        .bind(binding.attempt_id)
        .bind(lease.lease_id)
        .execute(&mut **tx)
        .await
        .map_err(db)?;
    sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
        .bind(binding.attempt_id)
        .bind(serde_json::to_value(hot).map_err(|_| AppError::Internal)?)
        .execute(&mut **tx)
        .await
        .map_err(db)?;
    journal::record_reserved(
        tx,
        binding,
        &owner,
        request.journal_revision,
        &request.journal_digest,
    )
    .await?;
    sqlx::query("INSERT INTO profiled_execution_receipts(node_id,operation_id,kind,request_digest,response) VALUES($1,$2,'accept',$3,$4)").bind(auth.id()).bind(request.operation_id).bind(aq_selection::digest(request).map_err(|_|AppError::Internal)?.as_str()).bind(serde_json::to_value(response).map_err(|_|AppError::Internal)?).execute(&mut **tx).await.map_err(db)?;
    Ok(plan)
}

/// Shared actual retained JSONB text budget for Accept and initial Grant.
/// Caller holds the global native preparation quota advisory lock.
pub(super) async fn retained_quota_locked(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
) -> Result<(i64, i64, i64, i64), AppError> {
    let mut quota: (i64,i64,i64,i64) = sqlx::query_as("SELECT count(*),count(*) FILTER(WHERE node_id=$1),COALESCE(sum(octet_length(lease::text)),0)+COALESCE((SELECT sum(octet_length(response::text)) FROM profiled_execution_receipts),0)+COALESCE((SELECT sum(octet_length(r.receipt::text)) FROM admission_material_receipts r JOIN admission_material_leases m ON m.lease_id=r.lease_id),0),COALESCE(sum(octet_length(lease::text)) FILTER(WHERE node_id=$1),0)+COALESCE((SELECT sum(octet_length(response::text)) FROM profiled_execution_receipts WHERE node_id=$1),0)+COALESCE((SELECT sum(octet_length(r.receipt::text)) FROM admission_material_receipts r JOIN admission_material_leases m ON m.lease_id=r.lease_id WHERE m.node_id=$1),0) FROM admission_material_leases").bind(node).fetch_one(&mut **tx).await.map_err(db)?;
    let extra = super::accepted_preparation::retained_bytes_locked(tx, node).await?;
    quota.2 = quota.2.checked_add(extra.0).ok_or_else(refused)?;
    quota.3 = quota.3.checked_add(extra.1).ok_or_else(refused)?;
    let phase = super::phase_execution::retained_bytes_locked(tx, node).await?;
    quota.2 = quota.2.checked_add(phase.0).ok_or_else(refused)?;
    quota.3 = quota.3.checked_add(phase.1).ok_or_else(refused)?;
    Ok(quota)
}
