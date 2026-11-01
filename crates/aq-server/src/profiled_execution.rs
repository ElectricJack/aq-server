//! Actual profiled owner, separate from fixture execution wire and providers.
//! Runtime preparation never turns a pure proposal or qualification reference
//! into authority without the locked project/selection/source/material fences.
pub mod api;
pub mod artifact_api;
pub mod artifacts;
pub mod config;
pub mod context_budget;
pub mod context_projection;
pub mod expiry;
pub mod journal;
pub mod material;
pub mod material_api;
pub mod no_grant;
pub mod policies;

pub mod registration;
pub mod source;
pub mod started;

pub mod stopped;

pub mod results;
pub mod source_exports;

pub mod source_api;

pub mod definitions;

pub mod preparation;

pub mod control_api;

pub mod initial_grant;
pub mod native_offer;
pub mod producer;

pub mod reconciliation;
pub(crate) mod continuation_policy;
