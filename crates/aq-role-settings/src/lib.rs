//! Roles, intelligence profiles and execution profiles as Settings documents
//! (spec 0014 §7, §7.1): the stored Markdown, the active snapshot they compile
//! into, and the rolling set of earlier snapshots.
//!
//! Every change goes through one path. It validates the whole resulting set
//! with `aq_profiles::roles::compile` and, in one transaction, stores the
//! change, stores the snapshot and moves the active pointer. An invalid
//! change stores nothing and returns every problem.
use aq_profiles::roles::{DocumentLimits, Kind, Problem, SelectionDefaults, Snapshot};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub mod http;
mod store;
pub mod tasks;
pub use store::{
    activate_stored, active_snapshot, apply, apply_referenced, check_binding, delete, get,
    install_defaults, list, pin, save, save_limits, save_selection_defaults, settings, snapshot,
    startup, unpin,
};

/// Something outside this store that names one of its documents, such as a
/// project binding's default role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    pub kind: Kind,
    pub id: String,
    /// What names it, completing "is …": for example ``the default role of
    /// project `Example` ``.
    pub by: String,
}
/// Reads the current [`Reference`]s on the connection of a change. The
/// server supplies it, so this crate does not depend on what refers to it.
pub type References = for<'c> fn(
    &'c mut sqlx::PgConnection,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<Vec<Reference>>> + Send + 'c>,
>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(String),
    /// The resulting set of documents is not valid; nothing was stored.
    #[error("the documents are not valid")]
    Problems(Vec<Problem>),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    NotFound(String),
    #[error("database")]
    Database(#[from] sqlx::Error),
    #[error("invalid stored role settings")]
    Internal,
}
pub type Result<T> = std::result::Result<T, Error>;

/// Every limit of this store. A Settings value: the store keeps it, and a
/// database that has never saved one uses `Default`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoreLimits {
    /// Bytes per document and for the whole set, documents per kind, and
    /// bytes of a name and a description.
    pub documents: DocumentLimits,
    /// Snapshots kept besides the active one and those an attempt pins
    /// (spec 0014 §7.1's N).
    pub snapshots_kept: u32,
    /// Save receipts kept for recognising a repeated operation id.
    pub operations_kept: u32,
}
impl Default for StoreLimits {
    fn default() -> Self {
        Self {
            documents: DocumentLimits::default(),
            snapshots_kept: 20,
            operations_kept: 200,
        }
    }
}

/// One stored document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Document {
    pub kind: Kind,
    pub id: String,
    pub markdown: String,
    /// The store revision at which this document was last saved.
    pub revision: i64,
    pub updated_at_ms: i64,
}

/// The singleton: the store revision, the active snapshot and the two
/// Settings values, each with the store revision at which it last changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub revision: i64,
    pub active_digest: Option<String>,
    pub selection_defaults: SelectionDefaults,
    pub selection_defaults_revision: i64,
    pub limits: StoreLimits,
    pub limits_revision: i64,
}

/// The active snapshot and the store revision it was read at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Active {
    pub revision: i64,
    pub snapshot: Snapshot,
}

/// What one change does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Change {
    Save {
        kind: Kind,
        id: String,
        markdown: String,
    },
    Delete {
        kind: Kind,
        id: String,
    },
    SelectionDefaults(SelectionDefaults),
    Limits(StoreLimits),
}
/// A change with its fence and its retry identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mutation {
    pub change: Change,
    /// The revision of the thing being changed as the caller last read it;
    /// 0 for a document that does not exist yet.
    pub expected_revision: i64,
    /// Repeating a change with the same id returns the first receipt.
    pub operation_id: Uuid,
}
/// The answer to an applied change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    /// The new store revision, which is also the changed thing's revision.
    pub revision: i64,
    /// The digest of the snapshot that is now active.
    pub digest: String,
}

/// What server start did (see [`startup`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Startup {
    /// The shipped defaults installed by this start.
    pub installed: Vec<(Kind, String)>,
    pub active_digest: Option<String>,
    /// Why the stored set does not compile; the active snapshot was kept.
    pub problems: Vec<Problem>,
}

pub fn invalid(message: &str) -> Error {
    Error::Invalid(message.into())
}
pub fn conflict(message: &str) -> Error {
    Error::Conflict(message.into())
}
