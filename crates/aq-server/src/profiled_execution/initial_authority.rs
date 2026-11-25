//! Historical protocol origin, separate from current launch authority.
//!
//! Existing readers explicitly accept only their qualified legacy variant. An
//! accepted no-launch receipt is never converted into a legacy preparation or
//! initial execution receipt, even if a hot row happens to contain a Grant.
use super::{accepted_preparation, initial_grant};
use crate::error::AppError;
use aq_admission::{Digest, LaunchBinding};
use aq_session_protocol::ProfiledGrant;
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

pub(crate) enum InitialAuthorityHistory {
    Legacy(Box<initial_grant::OriginalGrant>),
    AcceptedNoLaunch(Box<accepted_preparation::AcceptedHistory>),
    AcceptedRefreshNoLaunch(Box<super::accepted_refresh::AcceptedHistory>),
    PhaseExecution(Box<super::phase_execution::history::PhaseExecutionHistory>),
    PhaseRefreshExecution(Box<super::phase_refresh_execution::history::PhaseExecutionHistory>),
}
/// Receipt kinds are not interchangeable digest namespaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReceiptKind {
    LegacyPrepareV1,
    PhaseExecutionV2,
    PhaseRefreshExecutionV1,
}
pub(crate) struct ReceiptIdentity<'a> {
    pub kind: ReceiptKind,
    pub digest: &'a Digest,
}
/// A borrowed immutable owner record, never a decoded wire authorization.
/// Only the SQL-loaded history constructs this view. It has no Serde impl.
pub(crate) struct InitialAuthorityView<'a> {
    grant: &'a ProfiledGrant,
    kind: ReceiptKind,
    digest: &'a Digest,
}
impl InitialAuthorityHistory {
    pub(crate) fn view(&self) -> Result<InitialAuthorityView<'_>, AppError> {
        let (grant, kind, digest) = match self {
            Self::Legacy(h) => (
                &h.body.grant,
                ReceiptKind::LegacyPrepareV1,
                &h.receipt_digest,
            ),
            Self::PhaseExecution(h) => (
                &h.receipt.grant,
                ReceiptKind::PhaseExecutionV2,
                &h.receipt_digest,
            ),
            Self::PhaseRefreshExecution(h) => (
                &h.receipt.grant,
                ReceiptKind::PhaseRefreshExecutionV1,
                &h.receipt_digest,
            ),
            Self::AcceptedNoLaunch(_) | Self::AcceptedRefreshNoLaunch(_) => {
                return Err(unsupported());
            }
        };
        Ok(InitialAuthorityView {
            grant,
            kind,
            digest,
        })
    }
}
impl InitialAuthorityView<'_> {
    pub(crate) fn grant(&self) -> &ProfiledGrant {
        self.grant
    }
    pub(crate) fn identity(&self) -> ReceiptIdentity<'_> {
        ReceiptIdentity {
            kind: self.kind,
            digest: self.digest,
        }
    }
}
fn unsupported() -> AppError {
    AppError::Conflict("execution protocol origin is not supported by this owner".into())
}
async fn has_accepted_origin_locked(
    tx: &mut Transaction<'_, Postgres>,
    binding: &LaunchBinding,
) -> Result<bool, AppError> {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM profiled_v2_accepts WHERE attempt_id=$1)")
        .bind(binding.attempt_id)
        .fetch_one(&mut **tx)
        .await
        .map_err(|_| AppError::Internal)
}
/// Call under the exact attempt authority lock. This is an origin fence, not an
/// existence or current permission claim; malformed phase state also refuses.
pub(crate) async fn require_legacy_origin_locked(
    tx: &mut Transaction<'_, Postgres>,
    binding: &LaunchBinding,
) -> Result<(), AppError> {
    if has_accepted_origin_locked(tx, binding).await? {
        return Err(unsupported());
    }
    Ok(())
}
pub(crate) async fn load_locked(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    binding: &LaunchBinding,
    grant_id: Uuid,
) -> Result<InitialAuthorityHistory, AppError> {
    if !has_accepted_origin_locked(tx, binding).await? {
        return Ok(InitialAuthorityHistory::Legacy(Box::new(
            initial_grant::load_legacy_original_grant_locked(tx, node, binding, grant_id).await?,
        )));
    }
    if super::phase_profile::for_attempt_locked(tx, binding.attempt_id).await?
        == Some(super::phase_profile::Profile::Refresh)
    {
        return load_refresh_locked(tx, node, binding, grant_id).await;
    }
    let history = accepted_preparation::load_accepted_history_locked(tx, binding).await?;
    if history.request.original.identity.node_id != node
        || history.proposal.original.offered.offer.binding != *binding
    {
        return Err(unsupported());
    }
    if let Some(execution) =
        super::phase_execution::history::load_optional_locked(tx, node, binding, grant_id).await?
    {
        return Ok(InitialAuthorityHistory::PhaseExecution(Box::new(execution)));
    }
    Ok(InitialAuthorityHistory::AcceptedNoLaunch(Box::new(history)))
}
async fn load_refresh_locked(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    binding: &LaunchBinding,
    grant_id: Uuid,
) -> Result<InitialAuthorityHistory, AppError> {
    let history = super::accepted_refresh::load_accepted_history_locked(tx, binding).await?;
    if history.request.original.identity.node_id != node
        || history.proposal.original.offered.offer.binding != *binding
    {
        return Err(unsupported());
    }
    if let Some(execution) =
        super::phase_refresh_execution::history::load_optional_locked(tx, node, binding, grant_id)
            .await?
    {
        return Ok(InitialAuthorityHistory::PhaseRefreshExecution(Box::new(
            execution,
        )));
    }
    Ok(InitialAuthorityHistory::AcceptedRefreshNoLaunch(Box::new(
        history,
    )))
}
pub(crate) async fn load_legacy_initial_grant_locked(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    binding: &LaunchBinding,
    grant_id: Uuid,
) -> Result<initial_grant::OriginalGrant, AppError> {
    let history = load_locked(tx, node, binding, grant_id).await?;
    let view = history.view()?;
    if view.grant().binding != *binding || view.grant().grant_id != grant_id {
        return Err(unsupported());
    }
    let identity = view.identity();
    if identity.kind != ReceiptKind::LegacyPrepareV1 || identity.digest.as_str().len() != 64 {
        return Err(unsupported());
    }
    match history {
        InitialAuthorityHistory::Legacy(grant) => Ok(*grant),
        InitialAuthorityHistory::PhaseExecution(history) => {
            let _original_receipt = history.receipt;
            Err(unsupported())
        }
        InitialAuthorityHistory::AcceptedNoLaunch(history) => {
            // Read the actual typed origin rather than accepting a caller tag.
            let _original_receipt = history.receipt;
            Err(unsupported())
        }
        InitialAuthorityHistory::AcceptedRefreshNoLaunch(history) => {
            let _original_receipt = history.receipt;
            Err(unsupported())
        }
        InitialAuthorityHistory::PhaseRefreshExecution(history) => {
            let _original_receipt = history.receipt;
            Err(unsupported())
        }
    }
}
