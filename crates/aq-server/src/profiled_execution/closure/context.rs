//! Captured immutable facts for historical readback, never current eligibility.
use super::{canonical, conflict, database, history, policy::Verifier};
use crate::{error::AppError, profiled_execution::initial_grant};
use aq_admission::{
    Digest,
    control::Versions,
    granted_closure::{GrantedContext, JournalCheckpoint, Request},
    transport::GrantedLaunch,
};
use serde::{Deserialize, Serialize};
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Context {
    pub schema_version: u32,
    pub initial_operation_id: Uuid,
    pub initial_request_digest: Digest,
    pub initial_versions: Versions,
    pub execution_deadline_ms: u64,
    pub grant: GrantedLaunch,
    pub registration_id: Uuid,
    pub prepared: JournalCheckpoint,
    pub verifier: Verifier,
    /// Counter only: renew never provides a physical journal digest.
    pub latest_renewal_revision: Option<u64>,
}
impl Context {
    pub(super) fn validate(&self, request: &Request) -> Result<(), AppError> {
        self.initial_versions.validate().map_err(|_| conflict())?;
        if self.schema_version != 1
            || self.initial_operation_id.is_nil()
            || self.registration_id.is_nil()
            || self.initial_versions.grant_revision == 0
            || self.execution_deadline_ms == 0
            || self.execution_deadline_ms > i64::MAX as u64
            || self
                .latest_renewal_revision
                .is_some_and(|r| r == 0 || r > i64::MAX as u64)
            || !self
                .verifier
                .kinds
                .contains(&super::policy::EvidenceKind::of(&request.evidence))
        {
            return Err(conflict());
        }
        request
            .validate_against(&GrantedContext {
                grant: self.grant.clone(),
                runtime: self.verifier.runtime.clone(),
                qualification: self.verifier.qualification.clone(),
                prepared: self.prepared.clone(),
            })
            .map_err(|_| conflict())?;
        self.check_journal(request)
    }
    fn check_journal(&self, request: &Request) -> Result<(), AppError> {
        let high = self
            .latest_renewal_revision
            .unwrap_or(0)
            .max(self.prepared.revision);
        if request.journal.revision <= high {
            return Err(conflict());
        }
        if let aq_admission::granted_closure::Evidence::Suppressed { record, .. } =
            &request.evidence
            && record.previous.revision < high
        {
            return Err(conflict());
        }
        Ok(())
    }
    pub(super) async fn check_original(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        node: Uuid,
        request: &Request,
    ) -> Result<(), AppError> {
        self.validate(request)?;
        let original =
            initial_grant::load_original_grant_locked(tx, node, &request.binding, request.grant_id)
                .await?;
        if self.initial_operation_id != original.operation_id
            || self.initial_request_digest != original.request_digest
            || canonical(&self.grant)? != canonical(&original.body)?
            || canonical(&self.initial_versions)? != canonical(&original.versions)?
            || self.execution_deadline_ms != original.execution_deadline_ms
        {
            return Err(conflict());
        }
        self.check_registration_locked(tx, node, request).await?;
        self.check_prepared_locked(tx, request).await
    }
    async fn check_registration_locked(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        node: Uuid,
        request: &Request,
    ) -> Result<(), AppError> {
        let history = history::load_locked(tx, self.registration_id).await?;
        if history.metadata.request.identity.node_id != node
            || history.metadata.request.runtime != self.verifier.runtime
            || history.metadata.qualification != self.verifier.qualification
        {
            return Err(conflict());
        }
        let link: Option<Uuid> = sqlx::query_scalar(
            "SELECT registration_id FROM profiled_launch_registrations WHERE attempt_id=$1 AND \
             node_id=$2 AND boot_id=$3 FOR SHARE",
        )
        .bind(request.binding.attempt_id)
        .bind(node)
        .bind(history.metadata.request.identity.boot_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(database)?;
        if link != Some(self.registration_id) {
            return Err(conflict());
        }
        Ok(())
    }
    async fn check_prepared_locked(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        request: &Request,
    ) -> Result<(), AppError> {
        let row = sqlx::query(
            "SELECT phase,revision,digest,binding,grant_id FROM profiled_owner_journals WHERE \
             attempt_id=$1 FOR SHARE",
        )
        .bind(request.binding.attempt_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(database)?
        .ok_or_else(conflict)?;
        if row.get::<String, _>("phase") != "prepared"
            || row.get::<i64, _>("revision") != self.prepared.revision as i64
            || row.get::<String, _>("digest") != self.prepared.digest.as_str()
            || row.get::<serde_json::Value, _>("binding")
                != serde_json::to_value(&request.binding).map_err(|_| AppError::Internal)?
            || row.get::<Option<Uuid>, _>("grant_id") != Some(request.grant_id)
        {
            return Err(conflict());
        }
        Ok(())
    }
}
