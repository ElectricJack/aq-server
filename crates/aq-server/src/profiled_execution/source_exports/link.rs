use super::{Request, ReservedExport, database, unavailable};
use crate::{
    AppState,
    error::AppError,
    execution::{
        authority::{self, AuthorityView, Kind, Policy},
        profiled::Status,
    },
    source_retention::profiled_base::{RetentionObservation, VerifiedExport},
};
use aq_profiled_state::source_export::stored::{LaunchWindow, VerificationRow};
use aq_session_protocol::SessionIdentity;
use aq_worker_protocol::{SourceBinding, SourceRole, TransferManifest, Validate};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;
type Tx<'a> = Transaction<'a, Postgres>;
/// The reservation, exported bytes, and fresh retention inspection for one source link.
pub struct LinkEvidence<'a> {
    token: &'a ReservedExport,
    fact: &'a VerifiedExport,
    retention: &'a RetentionObservation,
}
impl<'a> LinkEvidence<'a> {
    pub fn new(
        token: &'a ReservedExport,
        fact: &'a VerifiedExport,
        retention: &'a RetentionObservation,
    ) -> Self {
        Self {
            token,
            fact,
            retention,
        }
    }
}
/// The producer holds its current native node/configuration and project fences,
/// and calls this inside the same transaction as the initial sealed offer. The
/// common loader reacquires node→attempt→hot and exact sealed identity checks;
/// this does not substitute for the producer's source/task/selection fence.
pub async fn link_locked(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    session: &SessionIdentity,
    evidence: LinkEvidence<'_>,
) -> Result<TransferManifest, AppError> {
    let LinkEvidence {
        token,
        fact,
        retention,
    } = evidence;
    fact.validate_for(token)?;
    retention.validate_for(token)?;
    let view = authority::load_locked(state, tx, session, Policy::IdentityOnly)
        .await
        .map_err(AppError::from)?;
    let request = token.request();
    let source = offered_source(&view, request)?;
    let launch = LaunchWindow::load(tx, view.attempt.attempt_id, request)
        .await
        .map_err(database)?
        .ok_or_else(unavailable)?;
    check_stored(tx, token, fact).await?;
    let manifest = base_manifest(&view, source, request.operation_id, fact)?;
    if !request
        .bind_base_locked(tx, view.identity.node_id, &manifest)
        .await
        .map_err(database)?
    {
        return Err(unavailable());
    }
    let now = u64::try_from((state.clock)().timestamp_millis()).map_err(|_| unavailable())?;
    if !launch.current(now, retention.observed_at_ms()..retention.expires_at_ms()) {
        return Err(unavailable());
    }
    Ok(manifest)
}
fn offered_source<'a>(
    view: &'a AuthorityView,
    request: &Request,
) -> Result<&'a SourceBinding, AppError> {
    let status = view.kind == Kind::ProfiledV1 && view.status == Status::Offered;
    let claimed = view.holds_task && !view.revoked && view.grant_id.is_none();
    if !status || !claimed {
        return Err(unavailable());
    }
    let source = view.source.as_ref().ok_or_else(unavailable)?;
    if !request.matches_source(view.project_id, source) {
        return Err(unavailable());
    }
    Ok(source)
}
async fn check_stored(
    tx: &mut Tx<'_>,
    token: &ReservedExport,
    fact: &VerifiedExport,
) -> Result<(), AppError> {
    let row = VerificationRow::load(tx, token.request().operation_id)
        .await
        .map_err(database)?
        .ok_or_else(unavailable)?;
    if !row.matches(token.request_digest().as_str(), &fact.canonical_bytes()?) {
        return Err(unavailable());
    }
    Ok(())
}
fn base_manifest(
    view: &AuthorityView,
    source: &SourceBinding,
    operation: Uuid,
    fact: &VerifiedExport,
) -> Result<TransferManifest, AppError> {
    let manifest = TransferManifest {
        transfer_id: operation,
        role: SourceRole::Base,
        checkpoint: source.base.clone(),
        attempt: view.attempt.clone(),
        resolved_spec_digest: view.execution_spec_digest.clone(),
        source: source.clone(),
        artifact_id: operation,
        manifest_digest: fact.manifest_digest().as_str().to_owned(),
        bundle_digest: Some(fact.bundle_digest().as_str().to_owned()),
        manifest_bytes: fact.manifest_bytes().len() as u64,
        bundle_bytes: fact.bundle_bytes(),
    };
    manifest.validate().map_err(|_| unavailable())?;
    Ok(manifest)
}
