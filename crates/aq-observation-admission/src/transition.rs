use super::*;

/// Current owner and immutable operation receipt read under the adapter's locks.
pub struct Retained<'a> {
    owner: Option<&'a Owner>,
    receipt: Option<&'a Receipt>,
}
impl<'a> Retained<'a> {
    pub fn new(owner: Option<&'a Owner>, receipt: Option<&'a Receipt>) -> Self {
        Self { owner, receipt }
    }
    fn owner(&self) -> Result<&Owner, ContractError> {
        self.owner.ok_or(ContractError::Invalid("owner absent"))
    }
    fn validate(&self, node_id: Uuid) -> Result<(), ContractError> {
        if let Some(owner) = self.owner {
            owner.validate()?;
            ensure(owner.marker.node_id == node_id, "owner node mismatch")?;
        }
        Ok(())
    }
}
impl Access {
    fn validate_node(&self, request: &Request) -> Result<(), ContractError> {
        valid_id(self.node_id)?;
        valid_id(self.boot_id)?;
        ensure(
            request.node_id == self.node_id,
            "authenticated node mismatch",
        )?;
        ensure(
            self.accepted_through < i64::MAX as u64
                && (1..=MAX_PENDING_DISTANCE).contains(&self.max_pending_distance),
            "owner cursor bounds",
        )
    }
    fn reply(&self, receipt: Receipt, current_owner: Owner) -> Reply {
        Reply {
            receipt,
            current_owner,
            accepted_through: self.accepted_through,
            admitted_node_boot_id: self.boot_id,
        }
    }
    fn admits_sequence(&self, next: u64) -> bool {
        next > self.accepted_through
            && next - self.accepted_through - 1 <= self.max_pending_distance
    }
}
impl Request {
    fn prepare_owner(
        &self,
        boot_id: Uuid,
        store_id: Uuid,
        allocated: Option<Uuid>,
    ) -> Result<Owner, ContractError> {
        let owner_id = allocated.ok_or(ContractError::Invalid("owner allocation missing"))?;
        valid_id(owner_id)?;
        Ok(Owner {
            marker: Marker {
                version: VERSION,
                node_id: self.node_id,
                store_id,
                owner_id,
                initial_boot_id: boot_id,
                prepare_operation_id: self.operation_id,
                prepare_digest: self.digest()?,
            },
            phase: Phase::Prepared,
            current_boot_id: boot_id,
            revision: 1,
            boot_transitions: 0,
        })
    }
    fn next_owner(
        &self,
        retained: &Retained,
        access: &Access,
        allocated: Option<Uuid>,
    ) -> Result<Owner, ContractError> {
        match &self.operation {
            Operation::Prepare { boot_id, store_id } => {
                ensure(
                    retained.owner.is_none()
                        && access.accepted_through == 0
                        && *boot_id == access.boot_id,
                    "existing owner or history; recovery required",
                )?;
                self.prepare_owner(*boot_id, *store_id, allocated)
            }
            Operation::Activate { marker } => retained.owner()?.activate(marker, access),
            Operation::AdvanceBoot {
                marker,
                expected_boot_id,
                expected_revision,
                new_boot_id,
                next_sequence,
            } => {
                let old = retained.owner()?;
                ensure(
                    old.matches_active(marker, *expected_boot_id, *expected_revision)
                        && !access.new_boot_previously_owned
                        && *new_boot_id != old.marker.initial_boot_id
                        && *new_boot_id == access.boot_id,
                    "boot owner fence",
                )?;
                ensure(
                    access.admits_sequence(*next_sequence),
                    "pending observation sequence bound",
                )?;
                Ok(Owner {
                    current_boot_id: *new_boot_id,
                    revision: old.revision + 1,
                    boot_transitions: old.boot_transitions + 1,
                    ..old.clone()
                })
            }
        }
    }
    fn receipt(&self, owner: &Owner) -> Result<Receipt, ContractError> {
        Ok(Receipt {
            version: VERSION,
            operation_id: self.operation_id,
            request_digest: self.digest()?,
            resulting_owner: owner.clone(),
        })
    }
}
impl Owner {
    fn activate(&self, marker: &Marker, access: &Access) -> Result<Self, ContractError> {
        ensure(
            self.phase == Phase::Prepared
                && &self.marker == marker
                && self.current_boot_id == access.boot_id
                && access.accepted_through == 0,
            "activation fence",
        )?;
        Ok(Self {
            phase: Phase::Active,
            revision: 2,
            ..self.clone()
        })
    }
    fn matches_active(&self, marker: &Marker, boot: Uuid, revision: u64) -> bool {
        self.phase == Phase::Active
            && &self.marker == marker
            && self.current_boot_id == boot
            && self.revision == revision
    }
}
/// Reducer only: the adapter atomically stores owner + immutable receipt. An
/// existing receipt is looked up by node/operation ID, never provided by a worker.
pub fn apply(
    retained: Retained,
    request: &Request,
    access: &Access,
    allocated_owner_id: Option<Uuid>,
) -> Result<Transition, ContractError> {
    request.validate()?;
    access.validate_node(request)?;
    retained.validate(access.node_id)?;
    let reply = if let Some(receipt) = retained.receipt {
        receipt.validate_against(request)?;
        let current = retained
            .owner
            .ok_or(ContractError::Invalid("missing retained owner"))?;
        access.reply(receipt.clone(), current.clone())
    } else {
        let owner = request.next_owner(&retained, access, allocated_owner_id)?;
        access.reply(request.receipt(&owner)?, owner)
    };
    reply.validate_against(request)?;
    Ok(Transition {
        reply,
        changed: retained.receipt.is_none(),
    })
}
