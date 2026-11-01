//! Decode the receipt pair already joined into the authorized offer lookup.
//! No extra query, live-process observation or terminal projection.
use super::super::{Error, Result};
use super::{SessionLifecycle, timestamp};
use aq_session_protocol::LaunchBinding;
use sqlx::{Row, postgres::PgRow};

pub(super) fn from_row(row: &PgRow, binding: &LaunchBinding) -> Result<SessionLifecycle> {
    let mut result = SessionLifecycle {
        started_at: None,
        stopped_at: None,
    };
    let Some(bytes) = row.get::<Option<Vec<u8>>, _>("started_request") else {
        return Ok(result);
    };
    let request = aq_admission::started::decode_request(&bytes).map_err(|_| Error::Unavailable)?;
    let receipt = aq_admission::started::decode_receipt(
        &row.get::<Option<Vec<u8>>, _>("started_receipt")
            .ok_or(Error::Unavailable)?,
        &request,
    )
    .map_err(|_| Error::Unavailable)?;
    if request.binding != *binding
        || request.digest().map_err(|_| Error::Unavailable)?.as_str()
            != row
                .get::<Option<String>, _>("started_digest")
                .ok_or(Error::Unavailable)?
    {
        return Err(Error::Unavailable);
    }
    result.started_at = Some(timestamp(receipt.recorded_at_ms)?);
    if let Some(bytes) = row.get::<Option<Vec<u8>>, _>("stopped_request") {
        let stopped =
            aq_admission::stopped::decode_request(&bytes).map_err(|_| Error::Unavailable)?;
        stopped
            .validate_after(&request)
            .map_err(|_| Error::Unavailable)?;
        let receipt = aq_admission::stopped::decode_receipt(
            &row.get::<Option<Vec<u8>>, _>("stopped_receipt")
                .ok_or(Error::Unavailable)?,
            &stopped,
        )
        .map_err(|_| Error::Unavailable)?;
        if stopped.digest().map_err(|_| Error::Unavailable)?.as_str()
            != row
                .get::<Option<String>, _>("stopped_digest")
                .ok_or(Error::Unavailable)?
        {
            return Err(Error::Unavailable);
        }
        result.stopped_at = Some(timestamp(receipt.recorded_at_ms)?);
    }
    Ok(result)
}
