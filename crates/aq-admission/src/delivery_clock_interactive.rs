//! Additive interactive execution clocks outside the byte-frozen transport module.
//! Keep the original delivery constructor, validation and projection unchanged.
use crate::{Invalid, Result, require, transport::DeliveryClock};

impl DeliveryClock {
    /// Explicit long interactive execution only; normal delivery TTLs stay 120s.
    pub fn new_interactive_session(server_time_ms: u64, expires_at_ms: u64) -> Result<Self> {
        let value = Self {
            server_time_ms,
            expires_at_ms,
            valid_for_ms: expires_at_ms
                .checked_sub(server_time_ms)
                .ok_or(Invalid("expired delivery"))?,
        };
        value.validate_interactive_session()?;
        Ok(value)
    }

    pub fn validate_interactive_session(&self) -> Result<()> {
        require(
            self.server_time_ms > 0
                && (1..=3_600_000).contains(&self.valid_for_ms)
                && self.server_time_ms.checked_add(self.valid_for_ms) == Some(self.expires_at_ms),
            "conservative delivery TTL",
        )
    }

    /// Charge the entire request/response interval against the interactive TTL.
    pub fn client_interactive_deadline_ns(&self, sent_ns: u64, received_ns: u64) -> Result<u64> {
        self.validate_interactive_session()?;
        require(received_ns >= sent_ns, "monotonic response interval")?;
        let deadline = sent_ns
            .checked_add(self.valid_for_ms * 1_000_000)
            .ok_or(Invalid("monotonic deadline overflow"))?;
        require(received_ns < deadline, "delivery consumed remaining TTL")?;
        Ok(deadline)
    }
}
