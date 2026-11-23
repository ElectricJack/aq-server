use super::*;
use aq_admission::transport::WorkerIdentity;
use aq_intelligence_config::inventory::{
    AdmissionError, NodeAdmission, ObservedWindow, combine_preparations,
};
use aq_selection::{CapabilitySnapshot, Digest, ExecutionTarget};
use axum::http::HeaderMap;
use sqlx::{Postgres, Transaction};

mod material;
mod persistence;
use material::MaterialObservation;
type InventoryReport =
    aq_intelligence_config::inventory::InventoryReport<crate::node_storage::StorageReport>;
type Tx<'a> = Transaction<'a, Postgres>;

pub(super) async fn report(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<InventoryReport>,
) -> Result<Json<Value>> {
    let auth = crate::worker_api::authenticate_context(&state, &headers).await?;
    let report = Report::new(&state, auth.id(), request)?;
    let mut material = MaterialObservation::read(&state, &report).await?;
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    crate::worker_api::lock_current_node(&mut tx, &auth).await?;
    material
        .verify_runtime(&state, &mut tx, &report.identity)
        .await?;
    report.require_boot(&mut tx).await?;
    if let Some(receipt) = report.receipt(&mut tx).await? {
        return Ok(Json(receipt));
    }
    let installed = loaded(&state)?;
    registry::check_current(&state, &mut tx).await?;
    let snapshot = report.snapshot(&mut tx, installed, &mut material).await?;
    registry::machine_quota(&mut tx, report.identity.node_id, &snapshot.value).await?;
    material.verify_time(&state)?;
    report.persist(&mut tx, &snapshot).await?;
    tx.commit().await.map_err(db)?;
    Ok(Json(snapshot.value))
}

struct Snapshot {
    revision: u64,
    window: ObservedWindow,
    value: Value,
}

struct Report<'a> {
    state: &'a AppState,
    request: InventoryReport,
    identity: WorkerIdentity,
    digest: Digest,
}

impl<'a> Report<'a> {
    fn new(state: &'a AppState, node_id: Uuid, request: InventoryReport) -> Result<Self> {
        if !request.valid_bounds() {
            return Err(bad("invalid inventory identity/bounds"));
        }
        if let Some(storage) = &request.storage {
            storage.validate()?;
        }
        let digest = aq_selection::digest(&request).map_err(|_| bad("inventory digest"))?;
        let identity = WorkerIdentity {
            node_id,
            boot_id: request.boot_id,
        };
        Ok(Self {
            state,
            request,
            identity,
            digest,
        })
    }

    async fn snapshot(
        &self,
        tx: &mut Tx<'_>,
        installed: &config::LoadedRegistry,
        material: &mut MaterialObservation,
    ) -> Result<Snapshot> {
        let revision = self.next_revision(tx).await?;
        let window = material.snapshot_window(self.state, self.request.ttl_ms)?;
        let harnesses = installed
            .config
            .installed_harnesses(&self.request.qualifications)
            .map_err(bad)?;
        let node_id = self.identity.node_id;
        let enrolled = aq_machine_control::enrolled(tx, node_id).await?;
        let admission = NodeAdmission {
            id: node_id,
            enrolled,
            window,
        };
        let mut credential_bindings = installed
            .config
            .installed_bindings(admission, &self.request.credentials)
            .map_err(admission_error)?;
        combine_preparations(
            &mut credential_bindings,
            std::mem::take(&mut material.preparations),
        );
        #[cfg(feature = "project_environments")]
        let harnesses = crate::profiled_execution::definitions::listed(
            self.state, &mut *tx, node_id, harnesses,
        )
        .await?;
        let snapshot = CapabilitySnapshot {
            backend_kind: None,
            material_delivery: material.delivery.clone(),
            target: ExecutionTarget::Worker {
                node_id,
                boot_id: self.identity.boot_id,
            },
            revision,
            observed_at_ms: window.observed_at_ms,
            expires_at_ms: window.expires_at_ms,
            harnesses,
            credential_bindings,
        };
        let value = serde_json::to_value(snapshot).map_err(|_| AppError::Internal)?;
        Ok(Snapshot {
            revision,
            window,
            value,
        })
    }
}

fn admission_error(error: AdmissionError) -> AppError {
    match error {
        AdmissionError::Invalid(message) => bad(message),
        AdmissionError::Forbidden => {
            AppError::Forbidden("credential not authorized for node".into())
        }
    }
}
