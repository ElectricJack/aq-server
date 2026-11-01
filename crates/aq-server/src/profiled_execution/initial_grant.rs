//! Prepare is a separate, atomic authority transition. No Accept is reconstructed
//! from its digest. Git/private-file observations run between fenced SQL phases.
mod graph;
mod history;
use super::{artifacts, journal, material, registration, source_exports};
use crate::{
    AppState,
    error::AppError,
    execution::{
        authority::{self, Kind, Policy, hot::HotState},
        profiled::Status,
    },
    intelligence::resolver::{self, Decision},
    worker_api::AuthenticatedNode,
};
use aq_admission::{
    ArtifactRef, Digest, MaterialLease, Text,
    control::{DeliveryStamp, InitialGrantDelivery, Versions},
    startup::NativeOffer,
    transport::{
        ArtifactReceipt, DeliveryClock, Feature, GrantedLaunch, PreparationPlan, Prepare,
        TransportDocument,
    },
};
pub(crate) use history::{OriginalGrant, load_original_grant_locked};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;
fn refused() -> AppError {
    AppError::Conflict("native initial grant is not currently available".into())
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
fn same(a: &impl Serialize, b: &impl Serialize) -> Result<bool, AppError> {
    Ok(serde_json::to_value(a).map_err(|_| AppError::Internal)?
        == serde_json::to_value(b).map_err(|_| AppError::Internal)?)
}
async fn begin(state: &AppState) -> Result<Transaction<'_, Postgres>, AppError> {
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    for sql in [
        "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE",
        "SET LOCAL lock_timeout='2s'",
        "SET LOCAL statement_timeout='3s'",
    ] {
        sqlx::query(sql).execute(&mut *tx).await.map_err(db)?;
    }
    Ok(tx)
}
/// Internal receipt preserves the SQL grant projection and the original delivery
/// metadata. Public TransportDocument bytes and schemas remain unchanged.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredInitialGrant {
    schema_version: u32,
    kind: GrantKind,
    body: GrantedLaunch,
    versions: Versions,
    execution_deadline_ms: u64,
}
impl StoredInitialGrant {
    fn decode(value: Value) -> Result<Self, AppError> {
        let grant: Self = serde_json::from_value(value).map_err(|_| refused())?;
        if grant.schema_version != 1
            || grant.versions.grant_revision == 0
            || grant.execution_deadline_ms == 0
            || grant.body.grant.expires_at_ms > grant.execution_deadline_ms
        {
            return Err(refused());
        }
        grant.body.validate().map_err(|_| refused())?;
        grant.versions.validate().map_err(|_| refused())?;
        Ok(grant)
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum GrantKind {
    Grant,
}
async fn versions(tx: &mut Transaction<'_, Postgres>, attempt: Uuid) -> Result<Versions, AppError> {
    let row=sqlx::query("SELECT record_revision,authority_revision,grant_revision FROM profiled_authority_hot WHERE attempt_id=$1").bind(attempt).fetch_one(&mut **tx).await.map_err(db)?;
    let value = Versions {
        record_revision: row.get::<i64, _>("record_revision") as u64,
        authority_revision: row.get::<i64, _>("authority_revision") as u64,
        grant_revision: row.get::<i64, _>("grant_revision") as u64,
    };
    value.validate().map_err(|_| refused())?;
    Ok(value)
}
struct Locked {
    offer: NativeOffer,
    graph: graph::Input,
    plan: PreparationPlan,
    hot: HotState,
    source: source_exports::LinkedExport,
    original: Option<StoredInitialGrant>,
    decision: Decision,
    material: MaterialLease,
    ceiling: u64,
}
async fn original(
    tx: &mut Transaction<'_, Postgres>,
    request: &Prepare,
) -> Result<Option<StoredInitialGrant>, AppError> {
    let row=sqlx::query("SELECT kind,request_digest,response FROM profiled_execution_receipts WHERE node_id=$1 AND operation_id=$2 FOR SHARE")
        .bind(request.identity.node_id).bind(request.operation_id).fetch_optional(&mut **tx).await.map_err(db)?;
    let Some(row) = row else { return Ok(None) };
    if row.get::<String, _>("kind") != "prepare"
        || row.get::<String, _>("request_digest")
            != aq_selection::digest(request)
                .map_err(|_| AppError::Internal)?
                .as_str()
    {
        return Err(refused());
    }
    let candidate = StoredInitialGrant::decode(row.get("response"))?;
    let original: OriginalGrant = load_original_grant_locked(
        tx,
        request.identity.node_id,
        &request.binding,
        candidate.body.grant.grant_id,
    )
    .await?;
    if original.operation_id != request.operation_id
        || original.request_digest
            != aq_selection::digest(request).map_err(|_| AppError::Internal)?
    {
        return Err(refused());
    }
    let grant = StoredInitialGrant {
        schema_version: 1,
        kind: GrantKind::Grant,
        body: original.body,
        versions: original.versions,
        execution_deadline_ms: original.execution_deadline_ms,
    };
    Ok(Some(grant))
}
async fn locked(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    auth: &AuthenticatedNode,
    request: &Prepare,
) -> Result<Locked, AppError> {
    crate::worker_api::lock_current_node(tx, auth).await?;
    let runtime =
        registration::current_runtime_locked(state, tx, auth.id(), request.identity.boot_id)
            .await?;
    let registered =
        registration::admitted_runtime_locked(tx, auth.id(), request.identity.boot_id).await?;
    let qualified = state
        .config
        .profiled_execution
        .as_ref()
        .and_then(|c| {
            c.qualified_runtimes
                .iter()
                .find(|r| r.runtime == runtime.runtime && r.qualification == runtime.qualification)
        })
        .ok_or_else(refused)?;
    if registered.request.identity != request.identity
        || qualified.material_delivery.is_none()
        || [
            Feature::ArtifactTransferV1,
            Feature::ServerMaterialV1,
            Feature::DurableHostJournalV1,
        ]
        .iter()
        .any(|f| !qualified.features.contains(f) || !registered.request.features.contains(f))
    {
        return Err(refused());
    }
    let view =
        authority::load_locked(state, tx, &request.binding.session, Policy::IdentityOnly).await?;
    if view.kind != Kind::ProfiledV1
        || !view.holds_task
        || view.revoked
        || view.stopped
        || view.cancellation_id.is_some()
        || view.offer_id != request.binding.offer_id
        || view.execution_spec_digest != request.binding.execution_spec_digest.as_str()
        || view.launch_manifest_digest.as_ref() != Some(&request.binding.launch_manifest_digest)
    {
        return Err(refused());
    }
    let offer =
        super::native_offer::load_original_offer_locked(tx, request.binding.attempt_id).await?;
    if offer.offered.offer.binding != request.binding {
        return Err(refused());
    }
    if offer
        .offered
        .required_features
        .iter()
        .any(|f| !qualified.features.contains(f) || !registered.request.features.contains(f))
    {
        return Err(refused());
    }
    for capabilities in [&qualified.capabilities, &registered.request.capabilities] {
        aq_session_protocol::require_capabilities(
            &offer.offered.offer.required_capabilities,
            capabilities,
        )
        .map_err(|_| refused())?;
        aq_session_protocol::require_capabilities(
            &offer.offered.offer.manifest.harness.required_capabilities,
            capabilities,
        )
        .map_err(|_| refused())?;
    }
    let hot: Value = sqlx::query_scalar(
        "SELECT state FROM profiled_authority_hot WHERE attempt_id=$1 FOR UPDATE",
    )
    .bind(request.binding.attempt_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(db)?;
    let hot: HotState = serde_json::from_value(hot).map_err(|_| refused())?;
    hot.validate_against(&request.binding)
        .map_err(|_| refused())?;
    let operation = hot.preparation_operation_id.ok_or_else(refused)?;
    let owner = hot.owner.as_ref().ok_or_else(refused)?;
    let row=sqlx::query("SELECT request_digest,response FROM profiled_execution_receipts WHERE node_id=$1 AND operation_id=$2 AND kind='accept' FOR SHARE").bind(auth.id()).bind(operation).fetch_optional(&mut **tx).await.map_err(db)?.ok_or_else(refused)?;
    let accept_digest: String = row.get("request_digest");
    let doc: TransportDocument =
        serde_json::from_value(row.get("response")).map_err(|_| refused())?;
    doc.validate().map_err(|_| refused())?;
    let TransportDocument::PreparationPlan(plan) = doc else {
        return Err(refused());
    };
    if plan.operation_id != operation
        || plan.binding != request.binding
        || !same(&plan.reservation, &request.reservation)?
        || plan.guardian_id != request.guardian_id
        || plan.launch_id != request.launch_id
        || owner.guardian_id != request.guardian_id
        || owner.launch_id != request.launch_id
        || owner.reservation_id != request.reservation.reservation_id
        || plan.materials.len() != 1
    {
        return Err(refused());
    }
    let old = original(tx, request).await?;
    let decision:Value=sqlx::query_scalar("SELECT d.decision FROM profiled_launches l JOIN intelligence_decisions d USING(decision_id) WHERE l.attempt_id=$1 FOR SHARE OF d").bind(request.binding.attempt_id).fetch_one(&mut **tx).await.map_err(db)?;
    let decision: Decision = serde_json::from_value(decision).map_err(|_| refused())?;
    if let Some(grant) = &old {
        if hot
            .grant
            .as_ref()
            .is_none_or(|g| !same(g, &grant.body.grant).unwrap_or(false))
            || hot.status != Status::Granted
        {
            return Err(refused());
        }
        resolver::fence_for_grant_replay(
            state,
            tx,
            &decision,
            &request.binding,
            grant.body.grant.grant_id,
        )
        .await?;
        view.active_at(now(state)?)?;
    } else {
        if hot.status != Status::Preparing
            || hot.grant.is_some()
            || hot.execution_deadline_ms.is_some()
        {
            return Err(refused());
        }
        resolver::fence_for_preparation(state, tx, &decision, &request.binding).await?;
    }
    let selected = decision.outcome.resolved.as_ref().ok_or_else(refused)?;
    if selected.lineage.project_id != view.project_id || selected.lineage.task_id != view.task_id {
        return Err(refused());
    }
    let lease = &plan.materials[0];
    let row=sqlx::query("SELECT m.* FROM admission_material_leases m JOIN profiled_launch_materials l ON l.lease_id=m.lease_id AND l.attempt_id=$2 WHERE m.lease_id=$1 FOR UPDATE OF m").bind(lease.lease_id).bind(request.binding.attempt_id).fetch_optional(&mut **tx).await.map_err(db)?.ok_or_else(refused)?;
    if row.get::<String, _>("state") != "admitted"
        || row.get::<Uuid, _>("node_id") != auth.id()
        || row.get::<Uuid, _>("boot_id") != request.identity.boot_id
        || row.get::<Uuid, _>("operation_id") != plan.operation_id
        || row.get::<String, _>("request_digest") != accept_digest
        || row.get::<Uuid, _>("prospective_attempt_id") != request.binding.attempt_id
        || row.get::<Option<Uuid>, _>("admitted_attempt_id") != Some(request.binding.attempt_id)
        || row.get::<Uuid, _>("offer_id") != request.binding.offer_id
        || !same(&row.get::<Value, _>("lease"), lease)?
        || row.get::<i64, _>("issued_at_ms") as u64 != lease.issued_at_ms
        || row.get::<i64, _>("expires_at_ms") as u64 != lease.expires_at_ms
        || row.get::<String, _>("credential_identity")
            != format!(
                "{}@{}",
                lease.credential_profile.id.as_str(),
                lease.credential_profile.version.as_str()
            )
        || row.get::<i64, _>("credential_revision") as u64 != lease.credential_revision
        || row.get::<String, _>("purpose") != lease.purpose.as_str()
        || selected.choice.credential_profile != lease.credential_profile
        || selected.credential_revision != lease.credential_revision
        || selected.choice.provider != lease.provider.as_str()
        || selected.choice.account_reference != lease.account_reference.as_str()
        || selected.choice.endpoint_reference != lease.endpoint_reference.as_str()
    {
        return Err(refused());
    }
    if request.materials.len() != 1 {
        return Err(refused());
    }
    let receipt = &request.materials[0];
    receipt
        .validate_against(lease, now(state)?)
        .map_err(|_| refused())?;
    if receipt.private_binding.id.as_str() != format!("aq.private-material/{}", lease.lease_id)
        || receipt.private_binding.version.as_str() != "1"
    {
        return Err(refused());
    }
    let source = source_exports::load_linked_locked(tx, request.binding.attempt_id).await?;
    if source.manifest().attempt != view.attempt
        || source.manifest().source != view.source.ok_or_else(refused)?
        || source.manifest().resolved_spec_digest != view.execution_spec_digest
        || source.token().request().project_id != view.project_id
        || source.token().request().project_revision != selected.project_revision
    {
        return Err(refused());
    }
    let graph_spec = graph::Input::spec(tx, request.binding.attempt_id).await?;
    let mut graph_context = None;
    let mut expected = Vec::new();
    let mut ceiling = plan
        .clock
        .expires_at_ms
        .min(lease.expires_at_ms)
        .min(offer.offered.clock.expires_at_ms)
        .min(selected.expires_at_ms);
    if offer.offered.artifacts.len() > 126 {
        return Err(refused());
    }
    let rows:Vec<Uuid>=sqlx::query_scalar("SELECT artifact_id FROM profiled_launch_artifacts WHERE attempt_id=$1 ORDER BY artifact_id LIMIT 127").bind(request.binding.attempt_id).fetch_all(&mut **tx).await.map_err(db)?;
    if rows.len() != offer.offered.artifacts.len() {
        return Err(refused());
    }
    for id in rows {
        let item = artifacts::load_locked(tx, id).await?;
        if item.project_id != view.project_id
            || !offer
                .offered
                .artifacts
                .iter()
                .any(|a| same(a, &item.descriptor).unwrap_or(false))
        {
            return Err(refused());
        }
        ceiling = ceiling.min(item.descriptor.retain_until_ms);
        if now(state)? < item.descriptor.retained_at_ms {
            return Err(refused());
        }
        if item.descriptor.artifact == graph_spec.task_context {
            graph_context = Some(item.bytes.clone());
        }
        expected.push(ArtifactReceipt {
            artifact: item.descriptor.artifact.clone(),
            bytes: item.bytes.len() as u64,
            digest: item.descriptor.artifact.digest,
        });
    }
    for (name, digest, bytes) in [
        (
            "manifest",
            source.manifest().manifest_digest.as_str(),
            source.manifest().manifest_bytes,
        ),
        (
            "bundle",
            source
                .manifest()
                .bundle_digest
                .as_deref()
                .ok_or_else(refused)?,
            source.manifest().bundle_bytes,
        ),
    ] {
        let digest = serde_json::from_value::<Digest>(Value::String(digest.to_owned()))
            .map_err(|_| refused())?;
        expected.push(ArtifactReceipt {
            artifact: ArtifactRef {
                id: Text::new(format!(
                    "aq.source-{name}/{}",
                    source.manifest().transfer_id
                ))
                .map_err(|_| refused())?,
                version: Text::new("1").map_err(|_| refused())?,
                digest: digest.clone(),
            },
            bytes,
            digest,
        });
    }
    if request.artifacts.len() != expected.len()
        || expected.iter().any(|e| {
            !request
                .artifacts
                .iter()
                .any(|r| same(r, e).unwrap_or(false))
        })
        || now(state)? >= ceiling
    {
        return Err(refused());
    }
    crate::intelligence::fence_current_registry(state, tx).await?;
    Ok(Locked {
        material: lease.clone(),
        offer,
        graph: graph::Input::new(graph_spec, graph_context.ok_or_else(refused)?)?,
        plan,
        hot,
        source,
        original: old,
        decision,
        ceiling,
    })
}
/// Current authenticated runtime claims installed bytes; the daemon verifies
/// exact retained backing bytes and current owners, not remote filesystem state.
pub async fn prepare(
    state: &AppState,
    auth: &AuthenticatedNode,
    request: &Prepare,
    nonce: Uuid,
) -> Result<InitialGrantDelivery, AppError> {
    TransportDocument::Prepare(request.clone())
        .validate()
        .map_err(|_| AppError::BadRequest("invalid native preparation document".into()))?;
    if auth.id() != request.identity.node_id || nonce.is_nil() {
        return Err(AppError::Forbidden(
            "native preparation identity mismatch".into(),
        ));
    }
    let mut tx = begin(state).await?;
    let before = locked(state, &mut tx, auth, request).await?;
    tx.commit().await.map_err(db)?;
    let graph = graph::observe(state, &before.graph, &before.decision).await?;
    let retention = crate::source_retention::profiled_base::inspect_retained(
        state,
        before.source.token(),
        before.source.fact(),
    )
    .await?;
    let registry = state.config.intelligence.clone().ok_or_else(refused)?;
    let lease = before.material.clone();
    let node = auth.id();
    static READS: std::sync::OnceLock<std::sync::Arc<tokio::sync::Semaphore>> =
        std::sync::OnceLock::new();
    let permit = READS
        .get_or_init(|| std::sync::Arc::new(tokio::sync::Semaphore::new(2)))
        .clone()
        .try_acquire_owned()
        .map_err(|_| refused())?;
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            material::read_for_lease(&registry, &lease, node)
        }),
    )
    .await
    .map_err(|_| refused())?
    .map_err(|_| AppError::Internal)?
    .map_err(|_| refused())?;
    let mut tx = begin(state).await?;
    let mut after = locked(state, &mut tx, auth, request).await?;
    after.ceiling = after.ceiling.min(graph.expires_at_ms());
    if before.source.fact().canonical_bytes()? != after.source.fact().canonical_bytes()?
        || !same(&before.material, &after.material)?
    {
        return Err(refused());
    }
    retention.validate_for(after.source.token())?;
    let time = now(state)?;
    graph.validate_for(&after.graph, time)?;
    if time < retention.observed_at_ms()
        || time >= retention.expires_at_ms()
        || time >= after.ceiling
    {
        return Err(refused());
    }
    let grant = if let Some(old) = after.original.take() {
        if old.body.grant.expires_at_ms > after.ceiling {
            return Err(refused());
        }
        old
    } else {
        // Shared retained-response byte budget and lock with Accept. No eviction,
        // quota widening, or new receipt on exact replay.
        sqlx::query(
            "SELECT pg_advisory_xact_lock(hashtextextended('aq-native-preparation-quota-v1',0))",
        )
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        let issued = now(state)?;
        graph.validate_for(&after.graph, issued)?;
        let deadline = issued
            .checked_add(after.offer.offered.offer.manifest.resources.wall_time_ms)
            .filter(|v| *v <= i64::MAX as u64)
            .ok_or_else(refused)?;
        let expiry = after.ceiling.min(deadline);
        let grant = GrantedLaunch {
            schema_version: 1,
            grant: aq_session_protocol::ProfiledGrant {
                binding: request.binding.clone(),
                grant_id: Uuid::new_v4(),
                expires_at_ms: expiry,
            },
            reservation_id: request.reservation.reservation_id,
            guardian_id: request.guardian_id,
            launch_id: request.launch_id,
            clock: DeliveryClock::new(issued, expiry).map_err(|_| refused())?,
            materials: after.plan.materials.clone(),
        };
        grant.validate().map_err(|_| refused())?;
        after.hot.status = Status::Granted;
        after.hot.grant = Some(grant.grant.clone());
        after.hot.execution_deadline_ms = Some(deadline);
        after
            .hot
            .validate_against(&request.binding)
            .map_err(|_| refused())?;
        sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
            .bind(request.binding.attempt_id)
            .bind(serde_json::to_value(&after.hot).map_err(|_| AppError::Internal)?)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        journal::record_prepared(
            &mut tx,
            &request.binding,
            after.hot.owner.as_ref().ok_or_else(refused)?,
            grant.grant.grant_id,
            request.journal_revision,
            &request.journal_digest,
        )
        .await?;
        let receipt = StoredInitialGrant {
            schema_version: 1,
            kind: GrantKind::Grant,
            body: grant,
            versions: versions(&mut tx, request.binding.attempt_id).await?,
            execution_deadline_ms: deadline,
        };
        let response = serde_json::to_value(&receipt).map_err(|_| AppError::Internal)?;
        let incoming: i32 = sqlx::query_scalar("SELECT octet_length($1::jsonb::text)")
            .bind(&response)
            .fetch_one(&mut *tx)
            .await
            .map_err(db)?;
        let incoming = u64::try_from(incoming).map_err(|_| refused())?;
        let quota:(i64,i64)=sqlx::query_as("SELECT COALESCE(sum(octet_length(lease::text)),0)+COALESCE((SELECT sum(octet_length(response::text)) FROM profiled_execution_receipts),0),COALESCE(sum(octet_length(lease::text)) FILTER(WHERE node_id=$1),0)+COALESCE((SELECT sum(octet_length(response::text)) FROM profiled_execution_receipts WHERE node_id=$1),0) FROM admission_material_leases").bind(auth.id()).fetch_one(&mut *tx).await.map_err(db)?;
        if incoming > 16384
            || u64::try_from(quota.0)
                .ok()
                .and_then(|n| n.checked_add(incoming))
                .is_none_or(|n| n > 67108864)
            || u64::try_from(quota.1)
                .ok()
                .and_then(|n| n.checked_add(incoming))
                .is_none_or(|n| n > 4194304)
        {
            return Err(refused());
        }
        sqlx::query("INSERT INTO profiled_execution_receipts(node_id,operation_id,kind,request_digest,response) VALUES($1,$2,'prepare',$3,$4)").bind(auth.id()).bind(request.operation_id).bind(aq_selection::digest(request).map_err(|_|AppError::Internal)?.as_str()).bind(response).execute(&mut *tx).await.map_err(db)?;
        receipt
    };
    let current_versions = versions(&mut tx, request.binding.attempt_id).await?;
    current_versions
        .validate_after(&grant.versions)
        .map_err(|_| refused())?;
    if after.hot.execution_deadline_ms != Some(grant.execution_deadline_ms) {
        return Err(refused());
    }

    let reply = InitialGrantDelivery {
        schema_version: 1,
        operation_id: request.operation_id,
        execution_deadline_ms: grant.execution_deadline_ms,
        versions: grant.versions,
        delivery: DeliveryStamp {
            nonce,
            clock: DeliveryClock::new(now(state)?, grant.body.grant.expires_at_ms)
                .map_err(|_| refused())?,
        },
        original: grant.body,
    };
    reply
        .validate_for(request, &after.plan, &after.offer, nonce)
        .map_err(|_| refused())?;
    resolver::check_time(state, &after.decision)?;
    graph.validate_for(&after.graph, now(state)?)?;
    if now(state)? >= after.ceiling || now(state)? >= retention.expires_at_ms() {
        return Err(refused());
    }
    tx.commit().await.map_err(db)?;
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn initial_grant_history_requires_an_actual_positive_grant_revision() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../aq-admission/artifacts/control.fixture.json"
        ))
        .unwrap();
        let body = &fixture["replies"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["kind"] == "initial_grant")
            .unwrap()["body"];
        let mut receipt = serde_json::json!({"schema_version":1,"kind":"grant","body":body["original"],"versions":body["versions"],"execution_deadline_ms":body["execution_deadline_ms"]});
        assert!(StoredInitialGrant::decode(receipt.clone()).is_ok());
        receipt["versions"]["grant_revision"] = serde_json::json!(0);
        assert!(StoredInitialGrant::decode(receipt).is_err());
    }
}
