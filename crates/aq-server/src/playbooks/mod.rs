//! Optional AQ-owned bridge and explicitly configured protected local owner.
//! Default construction remains inactive; runtime validates ordered review schema.
pub mod actions;
pub mod api;
pub mod artifacts;
pub use aq_playbook_domain::coding;
pub mod domain;
pub mod driver;
pub mod host_adapters;
pub mod internal_judgments;
pub mod service;

pub use aq_playbook_domain::review_domain;

pub mod config;
pub mod runtime;

pub mod inspection;

pub mod triggers;
