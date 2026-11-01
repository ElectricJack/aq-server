//! Optional AQ-owned bridge and explicitly configured protected local owner.
//! Default construction remains inactive; runtime validates ordered review schema.
pub mod api;
pub mod domain;
pub mod driver;
pub mod service;

pub mod review_domain;

pub mod config;
pub mod runtime;

pub mod inspection;
