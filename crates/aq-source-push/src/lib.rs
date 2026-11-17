//! A worker delivers a finished attempt by `git push`. This crate owns the Git
//! side of that: the per-attempt quarantine repository the push lands in, the
//! smart-HTTP receive step, and the aq-git artifact (manifest and bundle) the
//! server then builds locally from the pushed objects. The central pool is
//! never written here; only aq-git's own `artifact.import` admits objects.
//!
//! Authentication, attempt authority and the `source_transfers` row stay in
//! aq-server. Nothing in this crate decides who may push.
mod config;
mod quarantine;
mod wire;

pub use config::{SourceRetentionConfig, default_keep_per_task, default_max_age};
pub use quarantine::{Quarantine, expire, usage};
pub use wire::{GitManifest, artifact_id, digest, hash_file, socket_call};

/// Why a push or its admission was refused. aq-server maps each to a status.
#[derive(Debug)]
pub enum PushError {
    /// The pushed state does not fit the attempt (409).
    Refused(String),
    /// A configured size limit or the staging quota is exhausted (413).
    TooLarge(String),
    /// The request is not a smart-HTTP receive-pack request this server speaks (400).
    Invalid(String),
    Internal(anyhow::Error),
}
impl From<std::io::Error> for PushError {
    fn from(e: std::io::Error) -> Self {
        Self::Internal(e.into())
    }
}
impl From<anyhow::Error> for PushError {
    fn from(e: anyhow::Error) -> Self {
        Self::Internal(e)
    }
}
