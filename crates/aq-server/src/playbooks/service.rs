//! AQ-owned transactions. Engine storage remains independent; all mailbox
//! effects and operation events share this transaction and its current grants.
use super::domain::*;
use super::review_domain::CatalogProfile;
use crate::{
    Clock,
    agent_access::messaging,
    protected_storage::{Keyring, Sealed},
};
use aq_playbook_host::{
    model::{CommandContract, ContractRef, EventEnvelope, Registry, canonical_bytes},
    plugins::{Plugins, command_plugin, extension_capability, extension_command},
    runner::{CancellationResult, Intent},
};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Row, Transaction};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use uuid::Uuid;
const SOURCE: &str = "bd1246e8e972ee1627e94598945f398812c89044106b463151a696511412ca39";
pub use super::domain::Namespace;
#[derive(Clone, Copy)]
pub(super) struct GrantRevision {
    id: Uuid,
    revision: i64,
}
impl GrantRevision {
    pub(super) fn new(id: Uuid, revision: i64) -> Self {
        Self { id, revision }
    }
}
pub(super) struct ContractAuthorization<'a> {
    pub grant: GrantRevision,
    pub contract: &'a CommandContract,
    pub authority: &'a aq_playbook_host::runner::Authority,
    pub live: bool,
}
struct GrantCheck {
    owner: bool,
    live: bool,
}
impl GrantCheck {
    fn owner(live: bool) -> Self {
        Self { owner: true, live }
    }
    fn review(live: bool) -> Self {
        Self { owner: false, live }
    }
}
pub(super) enum HeldItem {
    Admission(Uuid),
    Event(Uuid),
}
/// Process-local current-owner context, not a deserializable fresh HTTP permit.
#[derive(Clone, Debug)]
pub struct RuntimeOwner {
    namespace_id: Uuid,
    generation: i64,
    incarnation: Uuid,
    revision: i64,
}
#[derive(Clone)]
pub struct Service {
    pub namespace: Namespace,
    pub(super) pool: PgPool,
    pub(super) keys: Arc<Keyring>,
    clock: Clock,
    pub(crate) profile: CatalogProfile,
    registry: Arc<Registry>,
    pub(super) plugins: Arc<Plugins>,
    pub(super) notifications: crate::notifications::persistence::Service,
    pub(super) reviews: Option<Arc<crate::reviews::Service>>,
    pub(super) host: Option<Arc<super::host_adapters::HostPorts>>,
    /// R1 (Host9): the routing commands serve any project on the router.
    pub installation_routing: bool,
    /// Epic 0: what the server records of results (`aq.work.result_ready`, `work.read`).
    #[cfg(feature = "delivery_playbook")]
    pub(super) delivery: Arc<dyn aq_playbook_domain::delivery_store::DeliveryPort>,
    /// Epic 0: the server's settlement, for `work.complete` and `work.return`.
    #[cfg(feature = "delivery_playbook")]
    pub(super) settlement: Arc<dyn super::host_adapters::DeliverySettlement>,
}
pub use aq_playbook_domain::intent_store::MessageResult;

pub use aq_playbook_domain::manual_admission::{AdmissionStatus, PrepareRequest};
/// Durable recovery data, not fresh write authorization. Only the driver can
/// inspect the retained exact request, and it must separately check current grant.
pub struct RetainedAdmission {
    pub status: AdmissionStatus,
    pub(super) request: aq_playbook_host::runner::KeyedAdmissionRequest,
    pub(super) grant_id: Uuid,
    pub(super) grant_revision: i64,
}

pub(super) use aq_playbook_domain::driver_outcomes::DecisionResult;
fn review_error(error: crate::error::AppError) -> Error {
    use crate::error::AppError;
    match error {
        AppError::BadRequest(_) => Error::Invalid,
        AppError::Forbidden(_) | AppError::Unauthenticated(_) => Error::Forbidden,
        AppError::Conflict(_) => Error::Conflict,
        AppError::PayloadTooLarge(_) => Error::Overloaded,
        _ => Error::Unknown,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecisionLookup {
    kind: String,
    handle: Uuid,
    recipient: Recipient,
}

mod admissions;
mod construction;
mod event_collection;
mod grants;
mod messages;
mod namespace;
mod outbox;
mod review_decisions;

#[path = "inspection/admissions.rs"]
mod inspection_admissions;

#[path = "inspection/catalog_auth.rs"]
mod catalog_auth;

#[path = "settlement_events.rs"]
mod settlement_events;
