//! Pre-offer source export ownership. A token is minted only after durable quota
//! reservation; the I/O owner cannot turn an arbitrary request into a token.
use aq_admission::Digest;
pub use aq_profiled_state::source_export::Request;
use serde::Serialize;
use uuid::Uuid;
/// No constructor or deserializer is exposed to production callers. The future
/// reserve service owns construction after its quota transaction commits.
pub struct ReservedExport {
    request: Request,
    request_digest: Digest,
    reserved_bytes: u64,
}
impl ReservedExport {
    fn from_encoded(
        encoded: aq_profiled_state::source_export::EncodedRequest,
        reserved_bytes: u64,
    ) -> Self {
        Self {
            request: encoded.request,
            request_digest: encoded.digest,
            reserved_bytes,
        }
    }
    pub fn request(&self) -> &Request {
        &self.request
    }
    pub fn request_digest(&self) -> &Digest {
        &self.request_digest
    }
    pub fn reserved_bytes(&self) -> u64 {
        self.reserved_bytes
    }
}
/// Explicit component-test capability; not a durable reservation or a route.
#[cfg(any(test, feature = "test-support"))]
pub fn synthetic_reserved_export(
    request: Request,
    reserved_bytes: u64,
) -> Result<ReservedExport, &'static str> {
    let request_digest = request.digest()?;
    if reserved_bytes <= 1_048_576 || reserved_bytes > i64::MAX as u64 {
        return Err("invalid synthetic source reservation bound");
    }
    Ok(ReservedExport {
        request,
        request_digest,
        reserved_bytes,
    })
}

pub enum Reservation {
    Pending(ReservedExport),
    /// Retained original fact only. A fresh owner inspection is required before
    /// an offer; historical readback neither re-exports nor extends freshness.
    Verified {
        reservation: ReservedExport,
        canonical_verified: Vec<u8>,
    },
}
#[track_caller]
fn unavailable() -> crate::error::AppError {
    tracing::debug!(target: "aq_refusal", at = %std::panic::Location::caller(), "profiled refusal");
    crate::error::AppError::Conflict("source export ownership unavailable or inconsistent".into())
}
/// A refusal of a fresh export's policy or observation, logged with its code
/// (live acceptance D10: these refusals were silent).
fn refused(reason: &'static str) -> crate::error::AppError {
    tracing::warn!(reason, "source export refused");
    unavailable()
}
fn database(_: sqlx::Error) -> crate::error::AppError {
    crate::error::AppError::Internal
}
fn canonical(value: &impl Serialize) -> Result<Vec<u8>, crate::error::AppError> {
    let mut value = serde_json::to_value(value).map_err(|_| unavailable())?;
    value.sort_all_objects();
    serde_json::to_vec(&value).map_err(|_| unavailable())
}
/// Trusted central owner only. Commits a finite quota reservation before it can
/// return the opaque capability accepted by the private Git I/O owner.
/// Historical original-ID reconciliation only. The trusted scheduler supplies
/// its owned project; no current admission permission or new scope is minted.
pub(crate) async fn recover_existing(
    state: &crate::AppState,
    operation: Uuid,
    project: Uuid,
) -> Result<Option<Reservation>, crate::error::AppError> {
    if operation.is_nil() || project.is_nil() {
        return Err(unavailable());
    }
    use aq_profiled_state::source_export::stored::OriginalRequest;
    let raw = OriginalRequest::load(&state.store.pool, operation, project)
        .await
        .map_err(database)?;
    let Some(raw) = raw else { return Ok(None) };
    let request = raw
        .into_request(operation, project)
        .ok_or_else(unavailable)?;
    reserve(state, request).await.map(Some)
}
mod admission;
mod observation;
mod reservation;
pub use reservation::reserve;

mod completion;
pub use completion::{LinkedExport, finalize, load_linked_locked};
mod link;
pub use link::{LinkEvidence, link_locked};
