//! The node limits: a Settings value (`node-limits`), stored beside the other
//! Settings values and read at every use. The server sends the node's share in
//! each control reply (`aq_protocol::NodeLimits`), so a change applies at the
//! node's next admission check without a restart.
use serde::{Deserialize, Serialize};

const DAY_MS: u64 = 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeLimits {
    /// Free sandbox storage a node needs before it requests or accepts work.
    /// Zero admits on any free space.
    pub sandbox_headroom_bytes: u64,
    /// Build-cache toolchain keys a node keeps per project; older idle ones are deleted.
    pub build_cache_keys_kept: u64,
    /// How long a node may defer sandbox admission before it needs attention.
    pub admission_deferral_attention_ms: u64,
}
impl Default for NodeLimits {
    fn default() -> Self {
        Self {
            sandbox_headroom_bytes: 32 * 1024 * 1024 * 1024,
            build_cache_keys_kept: 2,
            admission_deferral_attention_ms: 30 * 60 * 1000,
        }
    }
}
impl NodeLimits {
    /// Every value in its range; the message names the first that is not.
    pub fn validate(&self) -> Result<(), String> {
        let fields = [
            (
                "sandbox_headroom_bytes",
                self.sandbox_headroom_bytes,
                0,
                i64::MAX as u64,
            ),
            (
                "build_cache_keys_kept",
                self.build_cache_keys_kept,
                1,
                u32::MAX as u64,
            ),
            (
                "admission_deferral_attention_ms",
                self.admission_deferral_attention_ms,
                1,
                DAY_MS,
            ),
        ];
        for (name, value, least, most) in fields {
            if !(least..=most).contains(&value) {
                return Err(format!(
                    "{name} is {value}; it must be between {least} and {most}"
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid_and_each_range_is_checked() {
        assert_eq!(NodeLimits::default().validate(), Ok(()));
        let free = NodeLimits {
            sandbox_headroom_bytes: 0,
            ..Default::default()
        };
        assert_eq!(free.validate(), Ok(()));
        let none_kept = NodeLimits {
            build_cache_keys_kept: 0,
            ..Default::default()
        };
        assert!(
            none_kept
                .validate()
                .unwrap_err()
                .starts_with("build_cache_keys_kept is 0")
        );
        let never = NodeLimits {
            admission_deferral_attention_ms: DAY_MS + 1,
            ..Default::default()
        };
        assert!(never.validate().is_err());
    }
}
