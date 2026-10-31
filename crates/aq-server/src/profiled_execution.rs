//! Actual profiled owner, separate from fixture execution wire and providers.
//! Runtime preparation never turns a pure proposal or qualification reference
//! into authority without the locked project/selection/source/material fences.
pub mod api;
pub mod artifact_api;
pub mod artifacts;
pub mod config;
pub mod context_budget;
pub mod context_projection;
pub mod journal;
pub mod material;
pub mod material_api;
pub mod no_grant;
pub mod policies;

pub mod registration;
pub mod source;
pub mod started;

pub mod stopped;

pub mod source_exports;
