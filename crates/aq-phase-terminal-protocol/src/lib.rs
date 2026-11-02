//! Structural correlation only. No DTO authenticates an owner, proves a kernel
//! observation, permits execution, renews a lease, or releases retained resources.
#[macro_use]
mod profile_core;
terminal_profile! {
    phase: aq_admission::preparation_v2,
    schema_urn: "urn:aq:phase-terminal:1",
    publication: "aq-phase-terminal-protocol/0.1.0",
    admission: "0.1.9",
    contract_id: "aq.phase-terminal.contract",
    domain: "aq.phase-terminal",
    feature_count: 2,
    extra_features: [],
}
fn profile_manifest(_: &mut serde_json::Value) -> Result<()> {
    Ok(())
}
/// Separately negotiated context for actual registry observation continuity.
pub mod refresh_v1;

/// Exact parser/configuration dispatch only, never authentication or authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    FrozenV1,
    RegistryRefreshV1,
}
impl Profile {
    pub fn for_contract(contract: &aq_admission::ArtifactRef) -> Result<Self> {
        if *contract == contract_artifact()? {
            Ok(Self::FrozenV1)
        } else if *contract
            == refresh_v1::contract_artifact().map_err(|_| Invalid("refresh contract"))?
        {
            Ok(Self::RegistryRefreshV1)
        } else {
            Err(Invalid("unsupported exact terminal profile"))
        }
    }
    pub fn for_preparation_contract(contract: &aq_admission::ArtifactRef) -> Result<Self> {
        if *contract
            == aq_admission::preparation_v2::contract_artifact()
                .map_err(|_| Invalid("admission contract"))?
            || *contract
                == aq_admission::preparation_v2::long_contract_artifact()
                    .map_err(|_| Invalid("long admission contract"))?
        {
            Ok(Self::FrozenV1)
        } else if *contract
            == aq_admission::preparation_refresh_v1::contract_artifact()
                .map_err(|_| Invalid("refresh admission contract"))?
            || *contract
                == aq_admission::preparation_refresh_v1::long_contract_artifact()
                    .map_err(|_| Invalid("long refresh admission contract"))?
        {
            Ok(Self::RegistryRefreshV1)
        } else {
            Err(Invalid("unsupported exact preparation profile"))
        }
    }
}
