//! Whether a harness runs in a container, as the worker receives it.
use serde::{Deserialize, Serialize};

/// Mirrors aq-worker `runner::sandbox::confinement::ExecutionConfinement`.
/// Default: native confinement only, container confinement denied. An
/// execution profile document states one of the four consistent settings
/// ([`crate::roles::Confinement`]).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionConfinement {
    #[serde(default)]
    pub permit_container: bool,
    #[serde(default)]
    pub require_native: bool,
    /// The profile's harness always runs inside a node-owned sandbox
    /// container, whatever the node's placement facts say. Implies
    /// `permit_container`. Omitted when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub require_container: bool,
}
impl ExecutionConfinement {
    /// Container permission and native requirement exclude each other;
    /// requiring container confinement needs its permission.
    pub fn consistent(&self) -> bool {
        !matches!(
            (
                self.permit_container,
                self.require_native,
                self.require_container
            ),
            (true, true, _) | (false, _, true)
        )
    }
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}
