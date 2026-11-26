use super::observation::{is_handoff, retained_observation, validate_observation};
use super::{Request, canonical, database, refused, unavailable};
use crate::{
    error::AppError,
    profiled_execution::{
        artifacts,
        config::{ProfiledConfig, ProjectPolicyRecord},
        policies,
    },
};
use aq_admission::SourceAdmission;
use serde_json::Value;
use sqlx::{Postgres, Row, Transaction};
type Tx<'a> = Transaction<'a, Postgres>;

pub(super) struct Admission {
    retained: artifacts::Retained,
    observation: SourceAdmission,
}
impl Admission {
    pub(super) async fn load(
        tx: &mut Tx<'_>,
        config: &ProfiledConfig,
        request: &Request,
    ) -> Result<Self, AppError> {
        fence_project(tx, config, request).await?;
        let configured = config
            .project_policy_record(request.project_id)
            .map_err(|_| unavailable())?;
        let installed: serde_json::Value = sqlx::query_scalar(
            "SELECT policy FROM admission_source_policies WHERE project_id=$1 FOR SHARE",
        )
        .bind(request.project_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(database)?
        .ok_or_else(unavailable)?;
        let policy_handle = allows_handle(tx, &configured, request).await?;
        if !configured.enabled || canonical(&configured)? != canonical(&installed)? {
            return Err(refused("source_export_policy_not_installed"));
        }
        let retained = retained_observation(tx, request).await?;
        let value: Value = serde_json::from_slice(&retained.bytes).map_err(|_| unavailable())?;
        let handoff = is_handoff(tx, request, &value).await?;
        if !policy_handle && !handoff {
            return Err(refused("source_export_handle_not_configured"));
        }
        let observation = validate_observation(request, &configured, &value, handoff)?;
        Ok(Self {
            retained,
            observation,
        })
    }
    pub(super) fn check_fresh(&self, now: u64) -> Result<(), AppError> {
        self.retained
            .descriptor
            .verify(&self.retained.bytes, now)
            .map_err(|_| unavailable())?;
        if now == 0
            || !(self.observation.observed_at_ms..self.observation.expires_at_ms).contains(&now)
        {
            return Err(unavailable());
        }
        Ok(())
    }
}
async fn fence_project(
    tx: &mut Tx<'_>,
    config: &ProfiledConfig,
    request: &Request,
) -> Result<(), AppError> {
    policies::fence_configuration_locked(tx, config).await?;
    let project = sqlx::query(
        "SELECT revision,binding FROM project_execution_bindings WHERE project_id=$1 FOR SHARE",
    )
    .bind(request.project_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(database)?
    .ok_or_else(unavailable)?;
    let binding: crate::project_bindings::domain::ProjectBinding =
        serde_json::from_value(project.get("binding")).map_err(|_| unavailable())?;
    let repository = (binding.repository_id, binding.repository_binding_generation);
    let scope = repository == (request.repository_id, request.binding_generation)
        && project.get::<i64, _>("revision") as u64 == request.project_revision;
    if binding.enabled && scope {
        Ok(())
    } else {
        Err(unavailable())
    }
}
async fn allows_handle(
    tx: &mut Tx<'_>,
    configured: &ProjectPolicyRecord,
    request: &Request,
) -> Result<bool, AppError> {
    let handle = request.source_retention_handle;
    let configured_handle = configured.retention_handle == handle;
    let aq_admission::SourceSelection::NamedRef { reference } = &configured.source else {
        return Ok(configured_handle);
    };
    // D1: a named-ref policy may hydrate from the recorded source head's
    // candidate handle, and then only for exactly that published commit.
    let reference = reference.as_str();
    let head = crate::profiled_execution::settlement::publication::current_head(
        &mut **tx,
        request.project_id,
        request.repository_id,
        request.binding_generation,
        reference,
    )
    .await
    .map_err(|_| unavailable())?;
    let published = head.is_some_and(|h| h.oid == request.base.hex && h.retention_handle == handle);
    let allowed = configured_handle || published;
    // Spec 0016 §4: a handle `repo.retain_ref` returned for this exact head.
    #[cfg(feature = "delivery_playbook")]
    if !allowed {
        let scope = aq_delivery::main_sync::Scope::new(
            request.project_id,
            request.repository_id,
            request.binding_generation,
            reference,
        );
        return aq_delivery::main_sync::accepts(&mut **tx, &scope, handle, &request.base.hex)
            .await
            .map_err(|_| unavailable());
    }
    Ok(allowed)
}
