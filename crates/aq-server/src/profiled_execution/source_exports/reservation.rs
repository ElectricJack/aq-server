use super::{Request, Reservation, ReservedExport, admission::Admission, database, unavailable};
use crate::{AppState, error::AppError};
use aq_admission::Digest;
use aq_history_caps::source_exports::{self as storage, Staging, StoredExport};
use aq_profiled_state::source_export::EncodedRequest;
use sqlx::{Postgres, Transaction};
type Tx<'a> = Transaction<'a, Postgres>;
struct FreshReservation {
    admission: Admission,
    staging: Staging,
}
fn replay(encoded: EncodedRequest, row: StoredExport) -> Result<Reservation, AppError> {
    if !row.matches(&encoded.bytes, encoded.digest.as_str()) {
        return Err(unavailable());
    }
    let token = ReservedExport::from_encoded(encoded, row.reserved_bytes);
    match row.state.as_str() {
        "verified" => {
            let bytes = row.verified_bytes.ok_or_else(unavailable)?;
            if bytes.len() > 4_194_304
                || Some(Digest::of(&bytes).as_str()) != row.verified_digest.as_deref()
            {
                return Err(unavailable());
            }
            Ok(Reservation::Verified {
                reservation: token,
                canonical_verified: bytes,
            })
        }
        // The original durable side-effect intent may be reconciled even after
        // admission policy retirement. It remains charged, fixed-scope and is
        // never offer authority; the Git owner still checks original scope.
        "pending" => Ok(Reservation::Pending(token)),
        _ => Err(unavailable()),
    }
}
impl FreshReservation {
    async fn charge(
        &self,
        tx: &mut Tx<'_>,
        state: &AppState,
        encoded: &EncodedRequest,
    ) -> Result<u64, AppError> {
        // Same shared owner lock as legacy transfers/bases; unknown exports remain
        // fully charged. No I/O or file reading occurs inside this transaction.
        storage::lock_staging(tx).await.map_err(database)?;
        let prior = storage::pending_locked(tx, encoded.request.operation_id)
            .await
            .map_err(database)?;
        let now = u64::try_from((state.clock)().timestamp_millis()).map_err(|_| unavailable())?;
        if let Some(row) = prior {
            if row.state != "pending" || !row.matches(&encoded.bytes, encoded.digest.as_str()) {
                return Err(unavailable());
            }
            return Ok(row.reserved_bytes);
        }
        self.admission.check_fresh(now)?;
        let staging = &self.staging;
        let project = encoded.request.project_id;
        if let Some(reason) = staging.exhausted(tx, now).await.map_err(database)? {
            return Err(AppError::PayloadTooLarge(reason.into()));
        }
        aq_history_caps::insert_export(
            tx,
            (encoded.request.operation_id, project),
            (&encoded.bytes, encoded.digest.as_str()),
            (staging.reserved as i64, now as i64),
        )
        .await
        .map_err(database)?;
        Ok(staging.reserved)
    }
}
pub async fn reserve(state: &AppState, request: Request) -> Result<Reservation, AppError> {
    let encoded = request.encode().map_err(|_| unavailable())?;
    if encoded.bytes.len() > 8192 {
        return Err(unavailable());
    }
    let mut tx = state.store.pool.begin().await.map_err(database)?;
    storage::set_timeouts(&mut tx).await.map_err(database)?;
    // Historical verified facts are read before current admission and quota.
    // They cannot mint a different operation or execute any physical writer.
    if let Some(row) = storage::load(
        &mut tx,
        encoded.request.operation_id,
        storage::Access::Historical,
    )
    .await
    .map_err(database)?
    {
        let reservation = replay(encoded, row)?;
        tx.commit().await.map_err(database)?;
        return Ok(reservation);
    }
    let runtime = state.source_retention.as_ref().ok_or_else(unavailable)?;
    let reserved = runtime
        .config
        .max_bundle_bytes
        .checked_add(1_048_576)
        .ok_or_else(unavailable)?;
    let staging = Staging {
        reserved,
        limit: runtime.config.max_staging_bytes,
        project: encoded.request.project_id,
        request_bytes: encoded.bytes.len(),
    };
    let config = state
        .config
        .profiled_execution
        .as_deref()
        .ok_or_else(unavailable)?;
    let admission = Admission::load(&mut tx, config, &encoded.request).await?;
    let charged = FreshReservation { admission, staging }
        .charge(&mut tx, state, &encoded)
        .await?;
    tx.commit().await.map_err(database)?;
    Ok(Reservation::Pending(ReservedExport::from_encoded(
        encoded, charged,
    )))
}
