//! Pure transport ownership. No execution or source admission is conferred.
//! A historical receipt is not permission to create a missing local store.
pub use aq_lifecycle_protocol as wire;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use wire::{ContractError, Digest, Validate, ensure};

pub const VERSION: u32 = 1;
pub const MAX_BYTES: usize = 16384;
pub const MAX_BOOT_TRANSITIONS: u32 = 64;
// The concrete spool may impose a smaller configured bound.
pub const MAX_PENDING_DISTANCE: u64 = 65536;
fn valid_id(id: Uuid) -> Result<(), ContractError> {
    ensure(!id.is_nil(), "nil transport identity")
}
fn hash<T: Serialize>(value: &T) -> Result<Digest, ContractError> {
    let mut json =
        serde_json::to_value(value).map_err(|_| ContractError::Invalid("transport encoding"))?;
    json.sort_all_objects();
    let bytes =
        serde_json::to_vec(&json).map_err(|_| ContractError::Invalid("transport encoding"))?;
    ensure(bytes.len() <= MAX_BYTES, "transport document bound")?;
    Ok(Digest::of(&bytes))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u32,
    pub operation_id: Uuid,
    pub node_id: Uuid,
    pub operation: Operation,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Prepare {
        boot_id: Uuid,
        store_id: Uuid,
    },
    Activate {
        marker: Marker,
    },
    AdvanceBoot {
        marker: Marker,
        expected_boot_id: Uuid,
        expected_revision: u64,
        new_boot_id: Uuid,
        next_sequence: u64,
    },
}
impl Validate for Request {
    fn validate(&self) -> Result<(), ContractError> {
        ensure(self.version == VERSION, "transport version")?;
        valid_id(self.operation_id)?;
        valid_id(self.node_id)?;
        match &self.operation {
            Operation::Prepare { boot_id, store_id } => {
                valid_id(*boot_id)?;
                valid_id(*store_id)?;
            }
            Operation::Activate { marker } => {
                marker.validate()?;
                ensure(marker.node_id == self.node_id, "marker node")?;
            }
            Operation::AdvanceBoot {
                marker,
                expected_boot_id,
                expected_revision,
                new_boot_id,
                next_sequence,
            } => {
                marker.validate()?;
                ensure(
                    (2..=65).contains(expected_revision),
                    "boot expected revision",
                )?;
                ensure(marker.node_id == self.node_id, "marker node")?;
                valid_id(*expected_boot_id)?;
                valid_id(*new_boot_id)?;
                ensure(
                    expected_boot_id != new_boot_id,
                    "boot transition is unchanged",
                )?;
                ensure(
                    *next_sequence > 0 && *next_sequence <= i64::MAX as u64,
                    "next sequence",
                )?;
            }
        }
        hash(self)?;
        Ok(())
    }
}
impl Request {
    pub fn digest(&self) -> Result<Digest, ContractError> {
        self.validate()?;
        hash(self)
    }
}

/// Written durably into the new store before activation. Initial boot never changes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Marker {
    pub version: u32,
    pub node_id: Uuid,
    pub store_id: Uuid,
    pub owner_id: Uuid,
    pub initial_boot_id: Uuid,
    pub prepare_operation_id: Uuid,
    pub prepare_digest: Digest,
}
impl Validate for Marker {
    fn validate(&self) -> Result<(), ContractError> {
        ensure(self.version == VERSION, "store marker version")?;
        for id in [
            self.node_id,
            self.store_id,
            self.owner_id,
            self.initial_boot_id,
            self.prepare_operation_id,
        ] {
            valid_id(id)?;
        }
        Ok(())
    }
}
impl Marker {
    pub fn digest(&self) -> Result<Digest, ContractError> {
        self.validate()?;
        hash(self)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Prepared,
    Active,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Owner {
    pub marker: Marker,
    pub phase: Phase,
    pub current_boot_id: Uuid,
    pub revision: u64,
    pub boot_transitions: u32,
}
impl Validate for Owner {
    fn validate(&self) -> Result<(), ContractError> {
        self.marker.validate()?;
        valid_id(self.current_boot_id)?;
        ensure(
            self.boot_transitions <= MAX_BOOT_TRANSITIONS,
            "boot history exhausted",
        )?;
        let expected = match self.phase {
            Phase::Prepared => 1,
            Phase::Active => 2 + self.boot_transitions as u64,
        };
        ensure(self.revision == expected, "owner revision")?;
        if self.boot_transitions == 0 {
            ensure(
                self.current_boot_id == self.marker.initial_boot_id,
                "initial owner boot",
            )?;
        }
        if self.boot_transitions > 0 {
            ensure(
                self.current_boot_id != self.marker.initial_boot_id,
                "initial boot cannot be reused",
            )?;
        }
        if self.phase == Phase::Prepared {
            ensure(
                self.current_boot_id == self.marker.initial_boot_id && self.boot_transitions == 0,
                "prepared owner mutated",
            )?;
        }
        Ok(())
    }
}
/// Immutable operation receipt. Current state is supplied separately on replay.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub version: u32,
    pub operation_id: Uuid,
    pub request_digest: Digest,
    pub resulting_owner: Owner,
}
impl Receipt {
    pub fn validate_against(&self, request: &Request) -> Result<(), ContractError> {
        ensure(
            self.version == VERSION
                && self.operation_id == request.operation_id
                && self.request_digest == request.digest()?,
            "receipt request mismatch",
        )?;
        self.resulting_owner.validate()?;
        let owner = &self.resulting_owner;
        ensure(owner.marker.node_id == request.node_id, "receipt node")?;
        match &request.operation {
            Operation::Prepare { boot_id, store_id } => ensure(
                owner.phase == Phase::Prepared
                    && owner.marker.initial_boot_id == *boot_id
                    && owner.marker.store_id == *store_id
                    && owner.marker.prepare_operation_id == request.operation_id
                    && owner.marker.prepare_digest == self.request_digest,
                "prepare receipt",
            )?,
            Operation::Activate { marker } => ensure(
                owner.phase == Phase::Active
                    && owner.revision == 2
                    && owner.boot_transitions == 0
                    && owner.current_boot_id == marker.initial_boot_id
                    && &owner.marker == marker,
                "activation receipt",
            )?,
            Operation::AdvanceBoot {
                marker,
                new_boot_id,
                expected_revision,
                ..
            } => ensure(
                owner.phase == Phase::Active
                    && owner.revision == expected_revision + 1
                    && owner.boot_transitions > 0
                    && owner.current_boot_id == *new_boot_id
                    && &owner.marker == marker,
                "boot receipt",
            )?,
        }
        Ok(())
    }
    pub fn digest(&self) -> Result<Digest, ContractError> {
        ensure(self.version == VERSION, "receipt version")?;
        valid_id(self.operation_id)?;
        self.resulting_owner.validate()?;
        hash(self)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    pub receipt: Receipt,
    pub current_owner: Owner,
    pub accepted_through: u64,
    pub admitted_node_boot_id: Uuid,
}
impl Reply {
    pub fn validate_against(&self, request: &Request) -> Result<(), ContractError> {
        self.receipt.validate_against(request)?;
        self.current_owner.validate()?;
        valid_id(self.admitted_node_boot_id)?;
        ensure(
            self.current_owner.marker == self.receipt.resulting_owner.marker
                && self.current_owner.revision >= self.receipt.resulting_owner.revision,
            "current owner does not extend receipt",
        )?;
        if self.current_owner.revision == self.receipt.resulting_owner.revision {
            ensure(
                self.current_owner == self.receipt.resulting_owner,
                "same revision changed owner",
            )?;
        }
        ensure(
            self.accepted_through < i64::MAX as u64,
            "accepted sequence bound",
        )?;
        if self.current_owner.phase == Phase::Prepared {
            ensure(self.accepted_through == 0, "prepared owner has facts")?;
        }
        hash(self)?;
        Ok(())
    }
    /// Only a freshly authenticated reply may be used by the durable local create
    /// adapter. It must also enforce its create intent and missing-store fences.
    pub fn may_create(&self, request: &Request) -> Result<bool, ContractError> {
        self.validate_against(request)?;
        Ok(matches!(request.operation, Operation::Prepare { .. })
            && self.current_owner.phase == Phase::Prepared
            && self.current_owner.current_boot_id == self.admitted_node_boot_id)
    }
}
/// Actual authenticated current node and cursor, read under central locks.
/// Revocation and node incarnation proof are the adapter's responsibility.
pub struct Access {
    pub node_id: Uuid,
    pub boot_id: Uuid,
    pub accepted_through: u64,
    pub max_pending_distance: u64,
    /// Adapter lookup in immutable initial/boot receipts, including the original boot.
    pub new_boot_previously_owned: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transition {
    pub reply: Reply,
    pub changed: bool,
}
/// Reducer only: the adapter atomically stores owner + immutable receipt. An
/// existing receipt is looked up by node/operation ID, never provided by a worker.
pub fn apply(
    current: Option<&Owner>,
    previous: Option<&Receipt>,
    request: &Request,
    access: &Access,
    allocated_owner_id: Option<Uuid>,
) -> Result<Transition, ContractError> {
    request.validate()?;
    valid_id(access.node_id)?;
    valid_id(access.boot_id)?;
    ensure(
        request.node_id == access.node_id,
        "authenticated node mismatch",
    )?;
    ensure(
        access.accepted_through < i64::MAX as u64
            && (1..=MAX_PENDING_DISTANCE).contains(&access.max_pending_distance),
        "owner cursor bounds",
    )?;
    if let Some(owner) = current {
        owner.validate()?;
        ensure(
            owner.marker.node_id == access.node_id,
            "owner node mismatch",
        )?;
    }
    if let Some(receipt) = previous {
        receipt.validate_against(request)?;
        let current = current.ok_or(ContractError::Invalid("missing retained owner"))?;
        let reply = Reply {
            receipt: receipt.clone(),
            current_owner: current.clone(),
            accepted_through: access.accepted_through,
            admitted_node_boot_id: access.boot_id,
        };
        reply.validate_against(request)?;
        return Ok(Transition {
            reply,
            changed: false,
        });
    }
    let next = match &request.operation {
        Operation::Prepare { boot_id, store_id } => {
            ensure(
                current.is_none() && access.accepted_through == 0 && *boot_id == access.boot_id,
                "existing owner or history; recovery required",
            )?;
            let owner_id =
                allocated_owner_id.ok_or(ContractError::Invalid("owner allocation missing"))?;
            valid_id(owner_id)?;
            Owner {
                marker: Marker {
                    version: VERSION,
                    node_id: request.node_id,
                    store_id: *store_id,
                    owner_id,
                    initial_boot_id: *boot_id,
                    prepare_operation_id: request.operation_id,
                    prepare_digest: request.digest()?,
                },
                phase: Phase::Prepared,
                current_boot_id: *boot_id,
                revision: 1,
                boot_transitions: 0,
            }
        }
        Operation::Activate { marker } => {
            let old = current.ok_or(ContractError::Invalid("owner absent"))?;
            ensure(
                old.phase == Phase::Prepared
                    && &old.marker == marker
                    && old.current_boot_id == access.boot_id
                    && access.accepted_through == 0,
                "activation fence",
            )?;
            Owner {
                phase: Phase::Active,
                revision: 2,
                ..old.clone()
            }
        }
        Operation::AdvanceBoot {
            marker,
            expected_boot_id,
            expected_revision,
            new_boot_id,
            next_sequence,
        } => {
            let old = current.ok_or(ContractError::Invalid("owner absent"))?;
            ensure(
                old.phase == Phase::Active
                    && &old.marker == marker
                    && old.current_boot_id == *expected_boot_id
                    && old.revision == *expected_revision
                    && !access.new_boot_previously_owned
                    && *new_boot_id != old.marker.initial_boot_id
                    && *new_boot_id == access.boot_id
                    && old.boot_transitions < MAX_BOOT_TRANSITIONS,
                "boot owner fence",
            )?;
            ensure(
                *next_sequence > access.accepted_through
                    && *next_sequence - access.accepted_through - 1 <= access.max_pending_distance,
                "pending observation sequence bound",
            )?;
            Owner {
                current_boot_id: *new_boot_id,
                revision: old.revision + 1,
                boot_transitions: old.boot_transitions + 1,
                ..old.clone()
            }
        }
    };
    let receipt = Receipt {
        version: VERSION,
        operation_id: request.operation_id,
        request_digest: request.digest()?,
        resulting_owner: next.clone(),
    };
    let reply = Reply {
        receipt,
        current_owner: next,
        accepted_through: access.accepted_through,
        admitted_node_boot_id: access.boot_id,
    };
    reply.validate_against(request)?;
    Ok(Transition {
        reply,
        changed: true,
    })
}

/// A read-only status response is deliberately a different type from Reply.
/// It can authorize resuming an existing matching store through an owner adapter;
/// it cannot be used with may_create or substituted for a preparation receipt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Status {
    pub version: u32,
    pub node_id: Uuid,
    pub admitted_node_boot_id: Uuid,
    pub owner: Option<Owner>,
    pub accepted_through: u64,
}
impl Validate for Status {
    fn validate(&self) -> Result<(), ContractError> {
        ensure(self.version == VERSION, "status version")?;
        valid_id(self.node_id)?;
        valid_id(self.admitted_node_boot_id)?;
        ensure(self.accepted_through < i64::MAX as u64, "status sequence")?;
        if let Some(owner) = &self.owner {
            owner.validate()?;
            ensure(owner.marker.node_id == self.node_id, "status node")?;
            if owner.phase == Phase::Prepared {
                ensure(self.accepted_through == 0, "prepared status has facts")?;
            }
        } else {
            ensure(self.accepted_through == 0, "history without retained owner")?;
        }
        Ok(())
    }
}
pub fn decode_request(bytes: &[u8]) -> Result<Request, ContractError> {
    ensure(bytes.len() <= MAX_BYTES, "request size")?;
    let request: Request =
        serde_json::from_slice(bytes).map_err(|_| ContractError::Invalid("request JSON"))?;
    request.validate()?;
    Ok(request)
}
pub fn decode_reply(bytes: &[u8], request: &Request) -> Result<Reply, ContractError> {
    ensure(bytes.len() <= MAX_BYTES, "reply size")?;
    let reply: Reply =
        serde_json::from_slice(bytes).map_err(|_| ContractError::Invalid("reply JSON"))?;
    reply.validate_against(request)?;
    Ok(reply)
}
pub fn decode_status(bytes: &[u8]) -> Result<Status, ContractError> {
    ensure(bytes.len() <= MAX_BYTES, "status size")?;
    let status: Status =
        serde_json::from_slice(bytes).map_err(|_| ContractError::Invalid("status JSON"))?;
    status.validate()?;
    Ok(status)
}
/// Separate schema roots mirror the mutation request/reply and read-only status.
/// Semantic cross-field constraints remain enforced by the Rust validators.
pub fn schemas() -> serde_json::Value {
    serde_json::json!({"request":schemars::schema_for!(Request),"reply":schemars::schema_for!(Reply),"status":schemars::schema_for!(Status)})
}
