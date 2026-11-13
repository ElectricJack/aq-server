//! Classification of a task, produced by the router.

use serde::{Deserialize, Serialize};

use crate::Class;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    Low,
    Medium,
    High,
    VeryHigh,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Classification {
    pub task_type: String,
    pub intelligence_class: Class,
    pub deep_reasoning: bool,
    pub narrow: bool,
    pub test_verified: bool,
    pub risk: Risk,
    pub risk_reason: String,
    pub reason: String,
}
