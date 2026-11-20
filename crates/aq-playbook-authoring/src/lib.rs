//! Playbook authoring (spec 0013): an agent turns a Markdown source into a
//! playbook artifact the engine runs.
//!
//! This crate keeps the bookkeeping. A [`SourceRecord`] is what the server
//! knows about a source besides its Markdown (§4.1): the binding it activates
//! on and what its playbook may use. Each authoring of a source is a row of
//! `playbook_authoring_runs`, recorded before its internal task is submitted,
//! superseded by a newer request, and found again after a restart
//! ([`stranded`]). At most one saved run per source is active (§4.5).
pub mod builtin;
pub mod complete;
pub mod config;
#[cfg(feature = "delivery_playbook")]
pub mod continuity;
pub mod deploy;
#[cfg(feature = "delivery_playbook")]
pub mod document;
pub mod http;
pub mod request;
pub mod runs;
pub mod save;
pub mod source;
pub mod store;
pub mod view;

pub use complete::{
    FACTS_CHANGED, author, author_missed, completed, deploy, reinstall_active, rollback, serve,
    submit_run, tick,
};
pub use config::Config;
pub use deploy::{Deployer, Server};
#[cfg(feature = "delivery_playbook")]
pub use document::{author_document, author_saved, catch_up};
pub use request::{INSTRUCTIONS, OUTCOME_SCHEMA, PreviousPlaybook, workspace_files};
pub use runs::{
    ActiveRun, Ended, Finished, GRACE_MS, Run, active, finish, in_flight, prune, requested,
    run_for_artifact, running, running_for_task, stranded, supersede_older, task_for,
};
pub use save::{Inputs, Refusal, Saved, check_saved};
pub use source::{Host, SourceKind, SourceRecord, facts_digest, record_digest, source_digest};
pub use store::{ensure_builtin, get, register, stored_markdown};
pub use view::view;

/// The engine's model, compiler and simulation, as this crate was built with.
pub use aq_playbooks_compiler as compiler;
pub use aq_playbooks_model as model;
pub use aq_playbooks_simulation as simulation;

/// The expiry of the grant the server holds for a source (ruling R2): never.
/// The row in `playbook_grants` carries it, so the existing expiry checks
/// never refuse a source grant.
pub const SOURCE_GRANT_EXPIRES_AT_MS: i64 = i64::MAX;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    NotFound(String),
    /// The host could not say what a source's Markdown or facts are.
    #[error("{0}")]
    Host(String),
    #[error("database")]
    Database(#[from] sqlx::Error),
    /// A stored row no longer decodes into the types of this crate.
    #[error("stored playbook source is unreadable")]
    Corrupt,
}
pub type Result<T> = std::result::Result<T, Error>;

/// Milliseconds since the Unix epoch, for `created_at_ms` and the like.
pub(crate) fn now_ms() -> i64 {
    let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
    since.map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}
