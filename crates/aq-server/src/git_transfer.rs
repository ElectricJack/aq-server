//! Spec 0016 §5.2 (E0.3) over this server's state: `aq-git-transfer` holds
//! the grant, the token and the `/v1/git` proxy; here are its inputs.
use crate::{AppState, error::AppError, profiled_execution::source::Observation};
use aq_admission::SourceSelection;
use aq_git_transfer::{Mac, OfferFacts, PgGrants, Proxy, StoredGrant};
use std::sync::Arc;
use uuid::Uuid;

/// The grant token MAC under the delegated-permit credential key.
fn mac(state: &AppState) -> Option<Mac> {
    let key = state.config.delegated_permits.clone()?;
    Some(Arc::new(move |attempt, repository| {
        key.git_grant_mac(attempt, repository)
    }))
}
fn refused(e: aq_git_transfer::Refused) -> AppError {
    tracing::debug!(target: "aq_refusal", reason = e.0, "git grant refusal");
    AppError::Conflict("git grant unavailable".into())
}
/// `/v1/git/{repository}/{*path}`, when `[git_http]` is configured.
pub fn routes(state: &AppState) -> axum::Router<AppState> {
    let (Some(loaded), Some(mac)) = (state.config.git_http.clone(), mac(state)) else {
        return axum::Router::new();
    };
    let clock = state.clock.clone();
    let grants = PgGrants {
        pool: state.store.pool.clone(),
        now_ms: Arc::new(move || clock().timestamp_millis()),
    };
    aq_git_transfer::routes(Proxy {
        loaded,
        mac,
        grants: Arc::new(grants),
    })
}
/// The offer's grant, or none without `[git_http]`. The main ref is the
/// followed branch, else the publication target.
pub async fn offer_grant(
    state: &AppState,
    attempt_id: Uuid,
    task_id: Uuid,
    source: &Observation,
    publication_target: Option<&str>,
) -> Result<Option<StoredGrant>, AppError> {
    let Some(loaded) = state.config.git_http.as_deref() else {
        return Ok(None);
    };
    let mac = mac(state).ok_or_else(|| AppError::Conflict("git grant unavailable".into()))?;
    let main_ref = match &source.admission.selection {
        SourceSelection::NamedRef { reference } => Some(reference.as_str()),
        SourceSelection::Immutable { .. } => publication_target,
    };
    let facts = OfferFacts {
        attempt_id,
        task_id,
        repository_id: source.admission.repository_id,
        base: source.admission.resolved_object.clone(),
        main_ref: main_ref.map(str::to_owned),
    };
    let grant = aq_git_transfer::offer_grant(loaded, &mac, facts).await;
    grant.map(Some).map_err(refused)
}
/// The grants of a poll page's offers.
pub async fn page_grants(
    state: &AppState,
    offers: &[aq_admission::startup::NativeOffer],
) -> Result<Vec<aq_admission::git_grant::OfferGitGrant>, AppError> {
    let (Some(_), Some(mac)) = (&state.config.git_http, mac(state)) else {
        return Ok(vec![]);
    };
    let grants = aq_git_transfer::page_grants(&state.store.pool, &mac, offers).await;
    grants.map_err(refused)
}
