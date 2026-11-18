//! Server-owned facts of a recorded result that the settlement reads (moved
//! out of the aq-server lib): the verdict of a required check's evidence and
//! the state of the recorded delivery evidence. Facts only, no decision.
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckVerdict {
    Passed,
    Failed,
    /// No server-owned evidence bound to this exact source and validator.
    Missing,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    Missing,
    Published,
    Unpublished,
    Indeterminate,
    /// The retained source is the admitted base: the attempt changed no file
    /// and there is nothing to deliver (owner decision 2026-11-13).
    NoChange,
}
