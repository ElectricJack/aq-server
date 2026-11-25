//! Retain followed branches and refresh the cached head under the configured limits.
use super::{MainSync, Retained, Scope, bound, latest, limits, record};
use crate::{DeliveryError, OperationContext, Result, protocol};
use aq_role_settings::delivery::DeliveryLimits;
use serde_json::json;
use sqlx::PgPool;
use std::{path::Path, time::Duration};
use tokio::sync::watch;
use uuid::Uuid;

/// The database, Git service and clock for following a project's named refs.
pub struct MainFollower<'a, F> {
    pub(super) pool: &'a PgPool,
    pub(super) socket: &'a Path,
    now_ms: F,
}
impl<'a, F: Fn() -> u64> MainFollower<'a, F> {
    pub fn new(pool: &'a PgPool, socket: &'a Path, now_ms: F) -> Self {
        Self {
            pool,
            socket,
            now_ms,
        }
    }

    /// Sync `scope`'s ref from the remote and retain its head under one new
    /// operation, waiting at most `wait_ms`, then record the head.
    async fn retain(&self, scope: &Scope, wait_ms: u64) -> Result<MainSync> {
        scope.validate()?;
        let context = OperationContext {
            operation_id: Uuid::new_v4(),
            repository_id: scope.repository_id,
            binding_generation: scope.binding_generation,
        };
        context.validate()?;
        let params = json!({"ref": scope.reference});
        let mut request = serde_json::to_vec(&json!({"protocol_version":1,
        "request_id":context.operation_id,"operation":"repo.retain_ref",
        "repository_id":scope.repository_id,"operation_id":context.operation_id,
        "expected_binding_generation":scope.binding_generation,"params":params,
        "deadline_ms":wait_ms}))?;
        request.push(b'\n');
        let raw = protocol::exchange_within(self.socket, &request, Duration::from_millis(wait_ms))
            .await?;
        let receipt = protocol::decode::<Retained>(raw, &context, "repo.retain_ref", &params)?;
        let retained = receipt.outcome.map_err(|_| DeliveryError::Transport)?;
        let synced = MainSync {
            schema_version: 1,
            project_id: scope.project_id,
            repository_id: scope.repository_id,
            binding_generation: scope.binding_generation,
            reference: retained.reference,
            head: retained.head.hex,
            retention_handle: retained.retention_handle,
            synced_at_ms: retained.synced_at_ms,
        };
        let now = (self.now_ms)();
        if !synced.validate() || synced.synced_at_ms > now {
            return Err(DeliveryError::Protocol);
        }
        record(self.pool, &synced, now).await?;
        Ok(synced)
    }

    /// The head an offer for `scope` starts from: the latest record while it is
    /// younger than `main_freshness_ms`, otherwise one synced now.
    pub async fn current(&self, scope: &Scope) -> Result<MainSync> {
        let limits = limits(self.pool).await?;
        if let Some(synced) = self.cached(scope, &limits).await? {
            return Ok(synced);
        }
        self.refresh(scope, &limits).await
    }

    pub(super) async fn cached(
        &self,
        scope: &Scope,
        limits: &DeliveryLimits,
    ) -> Result<Option<MainSync>> {
        Ok(latest(self.pool, scope)
            .await?
            .filter(|synced| synced.fresh((self.now_ms)(), limits.main_freshness_ms)))
    }

    pub(super) async fn refresh(&self, scope: &Scope, limits: &DeliveryLimits) -> Result<MainSync> {
        let synced = self.retain(scope, limits.git_transfer_timeout_ms).await?;
        if !synced.fresh((self.now_ms)(), limits.main_freshness_ms) {
            return Err(DeliveryError::Protocol);
        }
        Ok(synced)
    }

    /// The background loop: a pass now, then one every `main_sync_interval_ms`
    /// (read from Settings before each wait), until `stop`.
    pub async fn run(&self, projects: Vec<(Uuid, String)>, mut stop: watch::Receiver<bool>) {
        if projects.is_empty() {
            return;
        }
        loop {
            tokio::select! {
                biased;
                _ = stop.wait_for(|stop| *stop) => return,
                result = sync_bound(self.pool, self.socket, &projects, &self.now_ms) => {
                    if let Err(error) = result { tracing::warn!(%error, "main sync pass deferred"); }
                }
            }
            let interval = limits(self.pool)
                .await
                .unwrap_or_default()
                .main_sync_interval_ms;
            tokio::select! {
                biased;
                _ = stop.wait_for(|stop| *stop) => return,
                _ = tokio::time::sleep(Duration::from_millis(interval)) => {}
            }
        }
    }
}

/// One pass of the sync loop: sync every bound scope; returns how many
/// synced. One project's failure does not stop the others.
pub async fn sync_bound(
    pool: &PgPool,
    socket: &Path,
    projects: &[(Uuid, String)],
    now_ms: impl Fn() -> u64,
) -> Result<usize> {
    let follower = MainFollower::new(pool, socket, now_ms);
    let limits = limits(pool).await?;
    let mut synced = 0;
    for scope in bound(pool, projects).await? {
        match follower
            .retain(&scope, limits.git_transfer_timeout_ms)
            .await
        {
            Ok(_) => synced += 1,
            Err(error) => {
                tracing::warn!(project = %scope.project_id, %error, "main sync deferred")
            }
        }
    }
    Ok(synced)
}
