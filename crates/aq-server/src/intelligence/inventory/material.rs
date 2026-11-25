use super::*;
use crate::profiled_execution::registration::{NativeRegistration, current_runtime_locked};
use aq_selection::{CredentialBinding, DeliveryCapability};
use std::sync::{Arc, OnceLock};
use tokio::sync::Semaphore;

// Observe private material outside SQL locks and verify the admitted runtime under lock.
pub(super) struct MaterialObservation {
    candidate: Option<NativeRegistration>,
    pub(super) delivery: Option<DeliveryCapability>,
    pub(super) preparations: Vec<CredentialBinding>,
    window: ObservedWindow,
}
impl MaterialObservation {
    pub(super) async fn read(state: &AppState, report: &Report<'_>) -> Result<Self> {
        let candidate = sqlx::query_scalar::<_, Option<Value>>(
            "SELECT state->'native' FROM execution_nodes WHERE node_id=$1",
        )
        .bind(report.identity.node_id)
        .fetch_optional(&state.store.pool)
        .await
        .map_err(db)?
        .flatten()
        .and_then(|v| serde_json::from_value(v).ok());
        let delivery = qualified_delivery(state, candidate.as_ref(), &report.identity);
        let window = window(now(state)?, report.request.ttl_ms)?;
        let preparations = match &delivery {
            Some(mode) => {
                read_bindings(state, report.identity.clone(), mode.clone(), window).await?
            }
            None => Vec::new(),
        };
        Ok(Self {
            candidate,
            delivery,
            preparations,
            window,
        })
    }

    pub(super) async fn verify_runtime(
        &self,
        state: &AppState,
        tx: &mut Tx<'_>,
        identity: &WorkerIdentity,
    ) -> Result<()> {
        if self.delivery.is_none() {
            return Ok(());
        }
        let actual = current_runtime_locked(state, tx, identity.node_id, identity.boot_id).await?;
        let same = self.candidate.as_ref().is_some_and(|c| {
            c.request.runtime == actual.runtime && c.qualification == actual.qualification
        });
        if !same {
            return Err(conflict(
                "native runtime changed during material observation",
            ));
        }
        if now(state)? >= self.window.expires_at_ms {
            return Err(conflict("material observation expired"));
        }
        Ok(())
    }

    pub(super) fn snapshot_window(&self, state: &AppState, ttl: u64) -> Result<ObservedWindow> {
        let current = now(state)?;
        if self.delivery.is_none() {
            return window(current, ttl);
        }
        self.require_window(current)?;
        Ok(self.window)
    }
    pub(super) fn verify_time(&self, state: &AppState) -> Result<()> {
        if self.delivery.is_some() {
            self.require_window(now(state)?)?;
        }
        Ok(())
    }
    fn require_window(&self, time: u64) -> Result<()> {
        if self.window.contains(time) {
            return Ok(());
        }
        Err(conflict(
            "material observation expired or clock moved backward",
        ))
    }
}
fn window(observed: u64, ttl: u64) -> Result<ObservedWindow> {
    ObservedWindow::new(observed, ttl).ok_or_else(|| bad("inventory expiry overflow"))
}
fn qualified_delivery(
    state: &AppState,
    candidate: Option<&NativeRegistration>,
    identity: &WorkerIdentity,
) -> Option<DeliveryCapability> {
    let candidate = candidate.filter(|c| &c.request.identity == identity)?;
    state
        .config
        .profiled_execution
        .as_deref()?
        .qualified(&candidate.request)
        .ok()?
        .material_delivery
        .clone()
}
async fn read_bindings(
    state: &AppState,
    identity: WorkerIdentity,
    delivery: DeliveryCapability,
    window: ObservedWindow,
) -> Result<Vec<CredentialBinding>> {
    static READS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    let permit = READS
        .get_or_init(|| Arc::new(Semaphore::new(4)))
        .clone()
        .try_acquire_owned()
        .map_err(|_| conflict("material observation capacity exhausted"))?;
    let registry = loaded(state)?.clone();
    let read = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        crate::profiled_execution::material::provisioning_bindings(
            &registry,
            aq_intelligence_config::material::ProvisioningSnapshot {
                node_id: identity.node_id,
                boot_id: identity.boot_id,
                delivery: &delivery,
                observed_at_ms: window.observed_at_ms,
                expires_at_ms: window.expires_at_ms,
                backend_kind: None,
            },
        )
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), read)
        .await
        .map_err(|_| conflict("material observation timed out"))?
        .map_err(|_| AppError::Internal)
}
