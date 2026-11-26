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

pub mod attempts;
#[cfg(feature = "delivery_playbook")]
pub mod delivery;
#[cfg(feature = "project_environments")]
pub mod environments;
pub mod http;
pub mod node;
pub mod observation;
pub mod refusals;
mod store;
pub mod suggestions;
pub mod tasks;
pub use store::{
    activate_stored, active_snapshot, apply, apply_referenced, check_binding, delete, document_of,
    get, install_defaults, list, node_limits, observation_limits, pin, release_finished_pins,
    routing_document, save, save_limits, save_routing_document, save_selection_defaults,
    save_supervisor_document, selection_defaults, settings, snapshot, startup, supervisor_document,
    unpin,
};
#[cfg(feature = "delivery_playbook")]
pub use store::{delivery_document, delivery_limits, save_delivery_document, save_delivery_limits};
#[cfg(feature = "project_environments")]
pub use store::{environment_limits, save_environment_limits};

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
    /// Tasks whose last refused offer is kept for the work read
    /// ([`refusals`]); the oldest go first.
    #[serde(default = "default_refusals_kept")]
    pub refusals_kept: u32,
}
fn default_refusals_kept() -> u32 {
    1000
}
impl Default for StoreLimits {
    fn default() -> Self {
        Self {
            documents: DocumentLimits::default(),
            snapshots_kept: 20,
            operations_kept: 200,
            refusals_kept: default_refusals_kept(),
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
    /// The routing document: the text the routing playbook is authored from.
    pub routing_markdown: String,
    pub routing_revision: i64,
    /// The delivery document: the text the delivery playbook is authored
    /// from (spec 0016 §8). Empty means no delivery policy.
    #[cfg(feature = "delivery_playbook")]
    pub delivery_markdown: String,
    #[cfg(feature = "delivery_playbook")]
    pub delivery_revision: i64,
    /// The delivery limits (spec 0016 §4), a Settings value of their own.
    #[cfg(feature = "delivery_playbook")]
    pub delivery_limits: delivery::DeliveryLimits,
    #[cfg(feature = "delivery_playbook")]
    pub delivery_limits_revision: i64,
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
    /// The routing document. It is not part of a snapshot.
    RoutingDocument(String),
    /// The node observation limits. They are not part of a snapshot.
    ObservationLimits(observation::ObservationLimits),
    /// The node limits. They are not part of a snapshot.
    NodeLimits(node::NodeLimits),
    /// The project supervisor's role text. It is not part of a snapshot.
    SupervisorDocument(String),
    /// The delivery document. It is not part of a snapshot.
    #[cfg(feature = "delivery_playbook")]
    DeliveryDocument(String),
    /// The delivery limits. They are not part of a snapshot.
    #[cfg(feature = "delivery_playbook")]
    DeliveryLimits(delivery::DeliveryLimitsChange),
    /// The environment limits (spec 0017 §6.7). They are not part of a snapshot.
    #[cfg(feature = "project_environments")]
    EnvironmentLimits(environments::EnvironmentLimits),
}
/// Owner and clock for a change in this settings store.
#[derive(Clone, Copy)]
pub struct Writer<'a> {
    pool: &'a sqlx::PgPool,
    actor: Uuid,
    now_ms: i64,
}
impl<'a> Writer<'a> {
    pub fn new(pool: &'a sqlx::PgPool, actor: Uuid, now_ms: i64) -> Self {
        Self {
            pool,
            actor,
            now_ms,
        }
    }
    pub fn edit(self, expected_revision: i64, operation_id: Uuid) -> Edit<'a> {
        Edit {
            writer: self,
            expected_revision,
            operation_id,
        }
    }
}
pub struct Edit<'a> {
    writer: Writer<'a>,
    expected_revision: i64,
    operation_id: Uuid,
}
impl Edit<'_> {
    async fn apply(self, change: Change) -> Result<Receipt> {
        let mutation = Mutation {
            change,
            expected_revision: self.expected_revision,
            operation_id: self.operation_id,
        };
        apply_referenced(self.writer, &mutation, None).await
    }
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
