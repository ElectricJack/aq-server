//! Mandatory typed provenance claims. Consumers must resolve authenticated owner
//! records; matching status/digest fields alone never certify current fact truth.
use super::*;
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
#[schemars(rename = "PreparationV2FactKind")]
pub enum FactKind {
    Project,
    Registry,
    Principal,
    Inventory,
    Availability,
    Budget,
    Context,
    Git,
    PrivateMaterialBacking,
}
impl FactKind {
    pub fn all() -> [Self; 9] {
        [
            Self::Project,
            Self::Registry,
            Self::Principal,
            Self::Inventory,
            Self::Availability,
            Self::Budget,
            Self::Context,
            Self::Git,
            Self::PrivateMaterialBacking,
        ]
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(rename = "PreparationV2AvailabilityState")]
pub enum AvailabilityState {
    Ready,
    Unavailable,
    Cooldown,
    Unknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(rename = "PreparationV2InventoryState")]
pub enum InventoryState {
    Installed,
    Missing,
    Unknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(rename = "PreparationV2GlobalCredentialSemantics")]
pub enum GlobalCredentialSemantics {
    ProvisionableOnly,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(rename = "PreparationV2RetentionState")]
pub enum RetentionState {
    Active,
    Released,
    Unknown,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    content = "body",
    rename_all = "snake_case",
    deny_unknown_fields
)]
#[schemars(rename = "PreparationV2FactValue")]
pub enum FactValue {
    Project {
        project_id: Uuid,
        revision: u64,
        catalog: ArtifactRef,
        permissions_digest: Digest,
    },
    Registry {
        registry: ArtifactRef,
        credential_profile: ArtifactRef,
        credential_revision: u64,
    },
    Principal {
        principal: ArtifactRef,
        scope: ArtifactRef,
    },
    Inventory {
        node_id: Uuid,
        boot_id: Uuid,
        sequence: u64,
        model: aq_session_protocol::ModelSettings,
        harness: aq_session_protocol::HarnessSettings,
        #[schemars(length(max = 64))]
        capabilities: Vec<aq_session_protocol::Capability>,
        state: InventoryState,
        credential_semantics: GlobalCredentialSemantics,
        credential_profile: ArtifactRef,
        credential_revision: u64,
    },
    Availability {
        provider: Text,
        account_reference: Text,
        endpoint_reference: Text,
        state: AvailabilityState,
        provider_observation: ArtifactRef,
    },
    Budget {
        lineage_id: Uuid,
        elapsed_ms: Option<u64>,
        maximum_lineage_elapsed_ms: u64,
        remaining_cost_microunits: Option<u64>,
    },
    Context {
        task: TaskRevision,
        context: ArtifactRef,
        full_context_digest: Digest,
        graph_policy: ArtifactRef,
    },
    Git {
        repository_id: Uuid,
        binding_generation: u64,
        base: ObjectId,
        policy: ArtifactRef,
        retention: ArtifactRef,
        state: RetentionState,
        manifest: transport::ArtifactReceipt,
        bundle: transport::ArtifactReceipt,
    },
    PrivateMaterialBacking {
        lease_id: Uuid,
        backing_digest: Digest,
        installation_receipt_digest: Digest,
    },
}
impl FactValue {
    pub fn kind(&self) -> FactKind {
        match self {
            Self::Project { .. } => FactKind::Project,
            Self::Registry { .. } => FactKind::Registry,
            Self::Principal { .. } => FactKind::Principal,
            Self::Inventory { .. } => FactKind::Inventory,
            Self::Availability { .. } => FactKind::Availability,
            Self::Budget { .. } => FactKind::Budget,
            Self::Context { .. } => FactKind::Context,
            Self::Git { .. } => FactKind::Git,
            Self::PrivateMaterialBacking { .. } => FactKind::PrivateMaterialBacking,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "PreparationV2FactBody")]
pub struct FactBody {
    pub authority: ArtifactRef,
    pub binding: LaunchBinding,
    pub choice_digest: Digest,
    pub revision: u64,
    pub observed_at_ms: u64,
    pub expires_at_ms: u64,
    pub value: FactValue,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "PreparationV2OwnerFact")]
pub struct OwnerFact {
    pub record: ArtifactRef,
    pub body: FactBody,
}
impl OwnerFact {
    pub fn record_digest(body: &FactBody) -> Result<Digest> {
        match &body.value {
            FactValue::Inventory {
                capabilities,
                harness,
                ..
            } => require(
                capabilities.len() <= 64 && harness.required_capabilities.len() <= 64,
                "phase fact capability bound",
            )?,
            FactValue::Git { base, .. } => base.validate()?,
            _ => {}
        }
        hash("aq.phase.owner-fact.v2", body)
    }
    pub fn validate_for(
        &self,
        p: &Proposal,
        a: &AcceptedReceipt,
        q: &PrepareRequest,
        at: u64,
    ) -> Result<()> {
        reference(&self.record)?;
        reference(&self.body.authority)?;
        revision(self.body.revision)?;
        require(
            self.record.digest == Self::record_digest(&self.body)?
                && self.record.version.as_str() == self.body.revision.to_string()
                && self.body.binding == a.lease.binding
                && self.body.choice_digest == a.lease.choice_digest
                && self.body.observed_at_ms > 0
                && self.body.observed_at_ms <= at
                && at < self.body.expires_at_ms
                && self.body.expires_at_ms <= i64::MAX as u64
                && self
                    .body
                    .expires_at_ms
                    .checked_sub(self.body.observed_at_ms)
                    .is_some_and(|ms| ms <= p.policy.maximum_observation_ms),
            "phase fact provenance/time/binding",
        )?;
        require(
            p.owners
                .iter()
                .any(|o| o.kind == self.body.value.kind() && o.authority == self.body.authority),
            "phase fact owner association",
        )?;
        let c = &p.choice;
        let m = &p.original.offered.offer.manifest;
        match &self.body.value {
            FactValue::Project {
                project_id,
                revision: rev,
                catalog,
                permissions_digest,
            } => require(
                *project_id == p.spec.lifecycle.project_id
                    && *rev == p.spec.project_revision
                    && *catalog == m.catalog
                    && *permissions_digest == c.permission_digest,
                "phase current project/pins",
            )?,
            FactValue::Registry {
                registry,
                credential_profile,
                credential_revision,
            } => require(
                *registry == c.registry
                    && *credential_profile == c.credential_profile
                    && *credential_revision == c.credential_revision,
                "phase current registry/credential",
            )?,
            FactValue::Principal { principal, scope } => require(
                *principal == c.principal && *scope == p.spec.graph_access_profile,
                "phase current principal/scope",
            )?,
            FactValue::Inventory {
                node_id,
                boot_id,
                sequence,
                model,
                harness,
                capabilities,
                state,
                credential_profile,
                credential_revision,
                ..
            } => {
                revision(*sequence)?;
                require(
                    *node_id == a.lease.identity.node_id
                        && *boot_id == a.lease.identity.boot_id
                        && *model == c.model
                        && *harness == c.harness
                        && *state == InventoryState::Installed
                        && *credential_profile == c.credential_profile
                        && *credential_revision == c.credential_revision,
                    "phase exact installed harness/provisionable credential",
                )?;
                aq_session_protocol::require_capabilities(
                    &p.original.offered.offer.required_capabilities,
                    capabilities,
                )
                .map_err(|_| Invalid("phase inventory capabilities"))?;
                aq_session_protocol::require_capabilities(
                    &m.harness.required_capabilities,
                    capabilities,
                )
                .map_err(|_| Invalid("phase harness capabilities"))?;
            }
            FactValue::Availability {
                provider,
                account_reference,
                endpoint_reference,
                state,
                provider_observation,
            } => {
                reference(provider_observation)?;
                require(
                    *provider == c.model.provider
                        && *account_reference == c.account_reference
                        && *endpoint_reference == c.endpoint_reference
                        && *state == AvailabilityState::Ready,
                    "phase initial availability must be fresh known ready",
                )?;
            }
            FactValue::Budget {
                lineage_id,
                elapsed_ms,
                maximum_lineage_elapsed_ms,
                remaining_cost_microunits,
            } => {
                let elapsed = elapsed_ms.ok_or(Invalid("phase initial budget unknown"))?;
                require(
                    *lineage_id == p.spec.lifecycle.lineage_id
                        && *maximum_lineage_elapsed_ms == c.maximum_lineage_elapsed_ms
                        && elapsed < *maximum_lineage_elapsed_ms,
                    "phase current remaining lineage",
                )?;
                if let Some(limit) = &m.model.spending_budget {
                    require(
                        remaining_cost_microunits
                            .is_some_and(|n| n > 0 && n <= limit.limit_microunits),
                        "phase current cost budget unknown/exhausted",
                    )?;
                }
            }
            FactValue::Context {
                task,
                context,
                full_context_digest,
                graph_policy,
            } => require(
                same(task, &p.spec.task)?
                    && *context == p.spec.task_context
                    && *full_context_digest == p.full_context_digest
                    && *graph_policy == p.spec.graph_access_profile,
                "phase full original task context",
            )?,
            FactValue::Git {
                repository_id,
                binding_generation,
                base,
                policy,
                retention,
                state,
                manifest,
                bundle,
            } => {
                base.validate()?;
                reference(retention)?;
                require(
                    *repository_id == p.spec.source.repository_id
                        && *binding_generation == p.spec.source.repository_binding_generation
                        && same(base, &p.spec.source.resolved_object)?
                        && base.hex.bytes().any(|b| b != b'0')
                        && *policy == p.spec.source.policy
                        && *state == RetentionState::Active,
                    "phase original retained base/policy",
                )?;
                let prefix = "aq.source-manifest/";
                let transfer = manifest
                    .artifact
                    .id
                    .as_str()
                    .strip_prefix(prefix)
                    .ok_or(Invalid("phase manifest member identity"))?;
                let id =
                    Uuid::parse_str(transfer).map_err(|_| Invalid("phase transfer identity"))?;
                crate::id(id)?;
                require(
                    manifest.artifact.id.as_str() == format!("aq.source-manifest/{id}"),
                    "phase canonical transfer member",
                )?;
                require(
                    bundle.artifact.id.as_str() == format!("aq.source-bundle/{id}")
                        && manifest.artifact.version.as_str() == "1"
                        && bundle.artifact.version.as_str() == "1"
                        && manifest.bytes > 0
                        && bundle.bytes > 0
                        && manifest.digest == manifest.artifact.digest
                        && bundle.digest == bundle.artifact.digest
                        && q.original
                            .artifacts
                            .iter()
                            .any(|x| same(x, manifest).unwrap_or(false))
                        && q.original
                            .artifacts
                            .iter()
                            .any(|x| same(x, bundle).unwrap_or(false)),
                    "phase exact linked transfer members",
                )?;
            }
            FactValue::PrivateMaterialBacking {
                lease_id,
                backing_digest,
                installation_receipt_digest,
            } => {
                let lease = &a.lease.materials[0];
                let installed = &q.original.materials[0];
                require(
                    *lease_id == lease.lease_id
                        && *backing_digest == lease.material_digest
                        && *installation_receipt_digest == Digest::of(&bytes(installed)?),
                    "phase original installation/current backing",
                )?;
            }
        }
        bound(self)
    }
}
