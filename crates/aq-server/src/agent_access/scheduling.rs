//! Fixed public graph observations for future scheduling. These reads never
//! claim a task, reserve capacity, mutate graph state or grant execution.
pub(super) mod parse;
mod reads;
mod transport;
use super::{Runtime, contract};
use aq_admission::TaskRevision;
use aq_profiled_state::native_offer::scheduling::{self, ObservationError};
pub use aq_profiled_state::native_offer::{GraphReadStamp, ReadyPage, ReadyTask, TaskContext};
use reads::{ContextRead, ReadyRead};
use serde_json::Value;
use transport::ReadRequest;
use uuid::Uuid;
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SchedulingReadError {
    #[error("invalid bounded scheduling read")]
    InvalidRequest,
    #[error("graph resource not found")]
    NotFound,
    #[error("graph access forbidden")]
    Forbidden,
    #[error("graph observation unavailable")]
    Unavailable,
    #[error("invalid graph response")]
    InvalidResponse,
    #[error("graph task revision changed")]
    StaleTask,
}
type Result<T> = std::result::Result<T, SchedulingReadError>;
type Clock<'a> = dyn Fn() -> Result<u64> + Sync + 'a;
fn trusted_time(clock: &(dyn Fn() -> chrono::DateTime<chrono::Utc> + Send + Sync)) -> Result<u64> {
    u64::try_from(clock().timestamp_millis())
        .ok()
        .filter(|n| *n > 0)
        .ok_or(SchedulingReadError::Unavailable)
}
fn bounded_cursor(value: &str) -> bool {
    !value.is_empty() && value.len() <= 4096 && !value.chars().any(char::is_control)
}
fn now() -> Result<u64> {
    chrono::Utc::now()
        .timestamp_millis()
        .try_into()
        .map_err(|_| SchedulingReadError::Unavailable)
}
impl From<ObservationError> for SchedulingReadError {
    fn from(error: ObservationError) -> Self {
        match error {
            ObservationError::InvalidResponse => Self::InvalidResponse,
            ObservationError::StaleTask => Self::StaleTask,
        }
    }
}
impl Runtime {
    pub async fn scheduling_ready(
        &self,
        workspace: Uuid,
        limit: u16,
        cursor: Option<&str>,
    ) -> Result<ReadyPage> {
        ReadyRead {
            workspace,
            limit,
            cursor,
        }
        .load(self, &now)
        .await
    }
    /// Trusted daemon clock around a genuinely new ready request/body receive,
    /// parallel to the current-context adapter. No wire-supplied clock or saved
    /// response can mint an observation through this method.
    pub(crate) async fn scheduling_ready_current(
        &self,
        workspace: Uuid,
        limit: u16,
        cursor: Option<&str>,
        clock: &(dyn Fn() -> chrono::DateTime<chrono::Utc> + Send + Sync),
    ) -> Result<ReadyPage> {
        ReadyRead {
            workspace,
            limit,
            cursor,
        }
        .load(self, &|| trusted_time(clock))
        .await
    }
    pub async fn scheduling_context(
        &self,
        expected: &TaskRevision,
        depth: u8,
        limit: u16,
        max_bytes: u32,
    ) -> Result<TaskContext> {
        ContextRead::new(expected, depth, limit, max_bytes)
            .load(self, &now)
            .await
    }
    /// Trusted daemon clock around a genuinely new request/body receive. This
    /// does not retime a saved graph response or accept a clock from HTTP input.
    pub(crate) async fn scheduling_context_current(
        &self,
        expected: &TaskRevision,
        depth: u8,
        limit: u16,
        max_bytes: u32,
        clock: &(dyn Fn() -> chrono::DateTime<chrono::Utc> + Send + Sync),
    ) -> Result<TaskContext> {
        ContextRead::new(expected, depth, limit, max_bytes)
            .load(self, &|| trusted_time(clock))
            .await
    }
}
