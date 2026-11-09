//! Private material delivery for an existing accepted preparation. No issuance,
//! installation receipt, provider access or execution permission is produced.
use super::{material, registration};
use crate::{
    AppState,
    error::AppError,
    execution::{
        authority::{self, Kind, Policy, hot::HotState},
        profiled::{Status, Stop},
    },
    worker_api::AuthenticatedNode,
};
use aq_admission::{
    MaterialLease,
    transport::{DeliveryClock, Feature, MaterialDelivery, MaterialFetch, TransportDocument},
};
use axum::{
    Router,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, HeaderValue, header},
    response::{IntoResponse, Response},
    routing::post,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::Value;
use sqlx::Row;
use std::sync::{Arc, OnceLock};

pub const METADATA_HEADER: &str = "aq-material-delivery";
/// C5b: the subscription-login version these bytes are (decimal). Echoed as
/// `MaterialRefresh.base_version` on write-back. Absent for other credentials.
pub const VERSION_HEADER: &str = "aq-material-version";
pub const MAX_METADATA_HEADER_BYTES: usize = 4096;
pub const MAX_REQUEST_BYTES: usize = 16 * 1024;

#[track_caller]
fn refused() -> AppError {
    tracing::debug!(target: "aq_refusal", at = %std::panic::Location::caller(), "profiled refusal");
    AppError::Conflict("private material delivery is not currently authorized".into())
}
use crate::error::db;
fn same(a: &impl serde::Serialize, b: &impl serde::Serialize) -> Result<bool, AppError> {
    Ok(serde_json::to_value(a).map_err(|_| AppError::Internal)?
        == serde_json::to_value(b).map_err(|_| AppError::Internal)?)
}

/// Kept opaque to avoid accidental logging/serialization of secret bytes.
pub struct Delivery {
    pub metadata: MaterialDelivery,
    /// Login version of the delivered bytes, for subscription logins.
    pub version: Option<i64>,
    material: material::ProtectedMaterial,
}
impl Delivery {
    pub fn bytes(&self) -> &[u8] {
        self.material.bytes()
    }
}

pub async fn fetch(
    state: &AppState,
    auth: &AuthenticatedNode,
    request: &MaterialFetch,
) -> Result<Delivery, AppError> {
    TransportDocument::MaterialFetch(request.clone())
        .validate()
        .map_err(|_| AppError::BadRequest("invalid material fetch".into()))?;
    if auth.id() != request.identity.node_id {
        return Err(AppError::Forbidden("material node mismatch".into()));
    }
    let registry = state.config.intelligence.clone().ok_or_else(|| refused())?;
    // This first read is only a bounded candidate for private file I/O. It grants
    // no delivery permission. All mutable owners are rechecked afterward.
    let candidate: Value = sqlx::query_scalar(
        "SELECT lease FROM admission_material_leases WHERE lease_id=$1 AND node_id=$2 AND boot_id=$3 AND admitted_attempt_id=$4 AND state='admitted'",
    )
    .bind(request.lease_id)
    .bind(auth.id())
    .bind(request.identity.boot_id)
    .bind(request.binding.attempt_id)
    .fetch_optional(&state.store.pool)
    .await
    .map_err(db)?
    .ok_or_else(|| refused())?;
    let lease: MaterialLease = serde_json::from_value(candidate).map_err(|_| refused())?;
    lease.validate().map_err(|_| refused())?;
    if lease.lease_id != request.lease_id || lease.binding != request.binding {
        return Err(refused());
    }
    static READS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    let permit = READS
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(2)))
        .clone()
        .try_acquire_owned()
        .map_err(|_| refused())?;
    let read_lease = lease.clone();
    let node_id = auth.id();
    let (source, version) =
        super::login_refresh::lease_versioned(&state.store.pool, &registry, &lease).await?;
    let bytes = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            material::read_for_lease_from(&registry, &read_lease, node_id, source.as_ref())
        }),
    )
    .await
    .map_err(|_| refused())?
    .map_err(|_| AppError::Internal)?
    .map_err(|_| refused())?;

    let mut tx = state.store.pool.begin().await.map_err(db)?;
    sqlx::query("SET LOCAL lock_timeout='2s'")
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    sqlx::query("SET LOCAL statement_timeout='3s'")
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    crate::worker_api::lock_current_node(&mut tx, auth).await?;
    let runtime =
        registration::current_runtime_locked(state, &mut tx, auth.id(), request.identity.boot_id)
            .await?;
    let qualified = state
        .config
        .profiled_execution
        .as_ref()
        .and_then(|c| {
            c.qualified_runtimes
                .iter()
                .find(|r| r.runtime == runtime.runtime)
        })
        .ok_or_else(|| refused())?;
    if !qualified.features.contains(&Feature::ServerMaterialV1)
        || qualified.material_delivery.is_none()
    {
        return Err(refused());
    }
    let view = authority::load_locked(
        state,
        &mut tx,
        &request.binding.session,
        Policy::IdentityOnly,
    )
    .await?;
    if view.kind != Kind::ProfiledV1
        || !view.holds_task
        || view.offer_id != request.binding.offer_id
        || view.launch_manifest_digest.as_ref() != Some(&request.binding.launch_manifest_digest)
        || view.execution_spec_digest != request.binding.execution_spec_digest.as_str()
    {
        return Err(refused());
    }
    let hot: Value = sqlx::query_scalar(
        "SELECT state FROM profiled_authority_hot WHERE attempt_id=$1 FOR UPDATE",
    )
    .bind(request.binding.attempt_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db)?
    .ok_or_else(|| refused())?;
    let hot: HotState = serde_json::from_value(hot).map_err(|_| refused())?;
    hot.validate_against(&request.binding)
        .map_err(|_| refused())?;
    if hot.status != Status::Preparing
        || hot.revoked
        || hot.cancellation_id.is_some()
        || !matches!(hot.stop, Stop::Unknown)
    {
        return Err(refused());
    }
    let original = super::accepted_preparation::load_preparing_locked(
        state,
        &mut tx,
        auth,
        &request.binding,
        &hot,
    )
    .await?;
    if original.materials().len() != 1
        || !same(&original.materials()[0], &lease)?
        || original.window().1 != lease.expires_at_ms
    {
        return Err(refused());
    }
    crate::intelligence::fence_current_registry(state, &mut tx).await?;
    let row = sqlx::query(
        "SELECT m.* FROM admission_material_leases m JOIN profiled_launch_materials l ON l.lease_id=m.lease_id AND l.attempt_id=$2 WHERE m.lease_id=$1 FOR UPDATE OF m",
    )
    .bind(request.lease_id)
    .bind(request.binding.attempt_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db)?
    .ok_or_else(|| refused())?;
    let identity = format!(
        "{}@{}",
        lease.credential_profile.id.as_str(),
        lease.credential_profile.version.as_str()
    );
    if row.get::<uuid::Uuid, _>("node_id") != auth.id()
        || row.get::<uuid::Uuid, _>("boot_id") != request.identity.boot_id
        || row.get::<uuid::Uuid, _>("prospective_attempt_id") != request.binding.attempt_id
        || row.get::<Option<uuid::Uuid>, _>("admitted_attempt_id")
            != Some(request.binding.attempt_id)
        || row.get::<uuid::Uuid, _>("offer_id") != request.binding.offer_id
        || row.get::<String, _>("state") != "admitted"
        || row.get::<String, _>("credential_identity") != identity
        || row.get::<i64, _>("credential_revision") as u64 != lease.credential_revision
        || row.get::<String, _>("purpose") != lease.purpose.as_str()
        || row.get::<i64, _>("issued_at_ms") as u64 != lease.issued_at_ms
        || row.get::<i64, _>("expires_at_ms") as u64 != lease.expires_at_ms
        || !same(&row.get::<Value, _>("lease"), &lease)?
    {
        return Err(refused());
    }
    // A delegated caller may fetch only the lease bound to its own permit, and
    // only while that permit's row is still unrevoked.
    if let Some(delegate) = auth.delegated() {
        let record = &delegate.current.record;
        let bound: Option<uuid::Uuid> = row.get("permit_id");
        let live: Option<bool> = sqlx::query_scalar(
            "SELECT NOT revoked FROM delegated_permits WHERE permit_id=$1 AND node_id=$2 AND attempt_id=$3 FOR SHARE",
        )
        .bind(record.permit_id)
        .bind(auth.id())
        .bind(request.binding.attempt_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db)?;
        if bound != Some(record.permit_id)
            || record.binding.attempt_id != request.binding.attempt_id
            || live != Some(true)
        {
            return Err(refused());
        }
    }
    let selected: Value = sqlx::query_scalar(
        "SELECT selection FROM intelligence_attempt_selections WHERE attempt_id=$1",
    )
    .bind(request.binding.attempt_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(db)?;
    let selected: aq_selection::ResolvedSelection =
        serde_json::from_value(selected).map_err(|_| refused())?;
    if selected.credential_revision != lease.credential_revision
        || selected.choice.credential_profile != lease.credential_profile
        || selected.choice.provider != lease.provider.as_str()
        || selected.choice.account_reference != lease.account_reference.as_str()
        || selected.choice.endpoint_reference != lease.endpoint_reference.as_str()
        || lease.purpose.as_str() != "model-authentication"
    {
        return Err(refused());
    }
    // The original expiry is immutable. Every retry charges time spent waiting
    // for node, hot, registry and lease locks before reporting its remaining TTL.
    let now = u64::try_from((state.clock)().timestamp_millis()).map_err(|_| refused())?;
    if now < original.window().0 || now >= original.window().1 {
        return Err(refused());
    }
    lease
        .verify_material(&request.binding, bytes.bytes(), now)
        .map_err(|_| refused())?;
    let metadata = MaterialDelivery {
        clock: DeliveryClock::new(now, lease.expires_at_ms).map_err(|_| refused())?,
        lease,
    };
    metadata_header(&metadata)?;
    tx.commit().await.map_err(db)?;
    Ok(Delivery {
        metadata,
        version,
        material: bytes,
    })
}

fn metadata_header(metadata: &MaterialDelivery) -> Result<HeaderValue, AppError> {
    let encoded =
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(metadata).map_err(|_| AppError::Internal)?);
    if encoded.len() > MAX_METADATA_HEADER_BYTES {
        return Err(refused());
    }
    HeaderValue::from_str(&encoded).map_err(|_| AppError::Internal)
}
pub fn routes(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/v1/worker/profiled/material", post(report))
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
        .with_state(state)
}
async fn report(
    State(state): State<AppState>,
    headers: HeaderMap,
    crate::admin_api::ApiJson(request): crate::admin_api::ApiJson<MaterialFetch>,
) -> Result<Response, AppError> {
    let auth = crate::delegated_permits::runtime::authenticate_runner(
        &state,
        &headers,
        aq_admission_sandbox::sandbox::RunnerOperation::FetchPreparationMaterial,
        None,
    )
    .await?;
    crate::delegated_permits::runtime::require_binding(&auth, &request.binding)?;
    let delivery = fetch(&state, &auth, &request).await?;
    // The HTTP stack owns this one response copy; zeroization of the private
    // reader cannot promise erasure of kernel/network/client buffers.
    let mut response = delivery.bytes().to_vec().into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
        .headers_mut()
        .insert(METADATA_HEADER, metadata_header(&delivery.metadata)?);
    if let Some(version) = delivery.version {
        response
            .headers_mut()
            .insert(VERSION_HEADER, HeaderValue::from(version));
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn valid_public_metadata_can_exceed_the_explicit_http_header_profile() {
        let fixtures: Vec<Value> = serde_json::from_str(include_str!(
            "../../../aq-admission/artifacts/fixtures.json"
        ))
        .unwrap();
        let fixture = fixtures
            .iter()
            .find(|v| v["kind"] == "material_lease")
            .unwrap();
        let mut lease: MaterialLease = serde_json::from_value(fixture["body"].clone()).unwrap();
        lease.provider = aq_admission::Label::new("p".repeat(512)).unwrap();
        lease.account_reference = aq_admission::Label::new("a".repeat(512)).unwrap();
        lease.endpoint_reference = aq_admission::Label::new("e".repeat(512)).unwrap();
        lease.credential_profile.id = aq_admission::Text::new("i".repeat(512)).unwrap();
        lease.credential_profile.version = aq_admission::Text::new("v".repeat(512)).unwrap();
        lease.validate().unwrap();
        let metadata = MaterialDelivery {
            clock: DeliveryClock::new(lease.issued_at_ms, lease.expires_at_ms).unwrap(),
            lease,
        };
        assert!(metadata_header(&metadata).is_err());
    }
}
