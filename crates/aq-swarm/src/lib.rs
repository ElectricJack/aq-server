//! Swarm read model (spec 0007): capacity assembly, machine reports and the
//! pure swarm assembly. aq-server gathers the inputs and serves the result.
pub mod assemble;
pub mod capacity;
pub mod class;
pub mod machine;
pub mod management;
pub mod service;

use aq_dashboard_contract::{Accuracy, Timestamp, Total};
use chrono::{DateTime, SecondsFormat, Utc};

/// A millisecond value that is not a representable time.
#[derive(Debug)]
pub struct TimeError;

pub fn timestamp(ms: u64) -> Result<Timestamp, TimeError> {
    let value = DateTime::<Utc>::from_timestamp_millis(ms.try_into().map_err(|_| TimeError)?)
        .ok_or(TimeError)?;
    Ok(Timestamp(
        value.to_rfc3339_opts(SecondsFormat::Millis, true),
    ))
}

pub fn exact(value: u32) -> Total {
    Total {
        value: Some(value),
        accuracy: Accuracy::Exact,
    }
}

#[cfg(test)]
mod tests;
