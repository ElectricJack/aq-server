use super::*;
use crate::{
    Digest,
    transport::{Register, TransportDocument},
};
use std::collections::BTreeSet;

pub const DELEGATED_RUNNER_CAPABILITY: &str = "delegated-runner-v1";
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum BackendKind {
    DockerLocal,
    Boat,
    CloudflareContainers,
}
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum BackendCapability {
    VmIsolation,
    NestedDocker,
    SuspendSnapshot,
    PersistentDiskDuringAttempt,
    SharedBuildCache,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Money {
    #[schemars(length(min = 3, max = 3), regex(pattern = "^[A-Z]{3}$"))]
    pub currency: String,
    pub micros: u64,
}
impl Money {
    fn validate(&self) -> Result<()> {
        check(
            self.currency.len() == 3
                && self.currency.bytes().all(|c| c.is_ascii_uppercase())
                && self.micros <= i64::MAX as u64,
            ContractError::InvalidShape,
        )
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HarnessVersion {
    #[schemars(length(min = 1, max = 128))]
    pub harness: String,
    #[schemars(length(min = 1, max = 128))]
    pub version: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TemplateAdvertisement {
    pub digest: Digest,
    #[schemars(length(min = 1, max = 32))]
    pub harnesses: Vec<HarnessVersion>,
    pub estimated_cold_start_ms: u64,
    pub estimated_cold_start_cost: Money,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BackendAdvertisement {
    pub kind: BackendKind,
    #[schemars(length(max = 5))]
    pub capabilities: Vec<BackendCapability>,
    #[schemars(range(min = 1, max = 65535))]
    pub max_concurrent_sandboxes: u32,
    pub spend_ceiling: Option<Money>,
    #[schemars(length(max = 128))]
    pub templates: Vec<TemplateAdvertisement>,
}
impl BackendAdvertisement {
    pub fn validate(&self) -> Result<()> {
        check(
            (1..=65535).contains(&self.max_concurrent_sandboxes)
                && self.capabilities.len() <= 5
                && self.templates.len() <= 128,
            ContractError::InvalidShape,
        )?;
        check(
            self.capabilities.iter().collect::<BTreeSet<_>>().len() == self.capabilities.len(),
            ContractError::InvalidShape,
        )?;
        check(
            self.kind != BackendKind::DockerLocal
                || !self.capabilities.contains(&BackendCapability::VmIsolation),
            ContractError::InvalidShape,
        )?;
        if let Some(ceiling) = &self.spend_ceiling {
            ceiling.validate()?;
        }
        let mut digests = BTreeSet::new();
        for template in &self.templates {
            check(
                digests.insert(template.digest.as_str())
                    && !template.harnesses.is_empty()
                    && template.harnesses.len() <= 32
                    && template.estimated_cold_start_ms <= 86_400_000,
                ContractError::InvalidShape,
            )?;
            template.estimated_cold_start_cost.validate()?;
            let mut harnesses = BTreeSet::new();
            for harness in &template.harnesses {
                label(&harness.harness, 128)?;
                label(&harness.version, 128)?;
                check(
                    harnesses.insert(&harness.harness)
                        && harness.version != "latest"
                        && !harness.version.contains(['*', '^', '~', '>', '<', '='])
                        && !harness.version.chars().any(char::is_whitespace),
                    ContractError::InvalidShape,
                )?;
            }
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Registration {
    pub schema_version: u32,
    pub registration: Register,
    pub backend: BackendAdvertisement,
}
impl Registration {
    pub fn validate(&self) -> Result<()> {
        check(self.schema_version == 2, ContractError::InvalidShape)?;
        TransportDocument::Register(self.registration.clone())
            .validate()
            .map_err(|_| ContractError::InvalidShape)?;
        self.backend.validate()
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CompatibilityReply {
    Compatible,
    UnsupportedRequiredCapability {
        #[schemars(length(min = 1, max = 64))]
        missing: Vec<String>,
    },
}
fn capability_names(values: &[String]) -> Result<()> {
    check(values.len() <= 64, ContractError::InvalidShape)?;
    let mut seen = BTreeSet::new();
    for value in values {
        label(value, 64)?;
        check(
            value
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
                && seen.insert(value),
            ContractError::InvalidShape,
        )?;
    }
    Ok(())
}
impl CompatibilityReply {
    pub fn validate(&self) -> Result<()> {
        if let Self::UnsupportedRequiredCapability { missing } = self {
            capability_names(missing)?;
            check(!missing.is_empty(), ContractError::InvalidShape)?;
        }
        Ok(())
    }
}
/// Unknown requirements always decline, including names a caller falsely puts
/// in `supported`. A legacy peer supplies its own actual supported set.
/// Callers must validate the backend advertisement before deriving its support
/// names. `delegated-runner-v1` is separate protocol support and must be established
/// by the installed adapter. This string-set check cannot validate a backend kind.
pub fn negotiate(required: &[String], supported: &[String]) -> Result<CompatibilityReply> {
    capability_names(required)?;
    capability_names(supported)?;
    let known = [
        DELEGATED_RUNNER_CAPABILITY,
        "vm-isolation",
        "nested-docker",
        "suspend-snapshot",
        "persistent-disk-during-attempt",
        "shared-build-cache",
    ];
    let missing: Vec<_> = required
        .iter()
        .filter(|name| !known.contains(&name.as_str()) || !supported.contains(name))
        .cloned()
        .collect();
    Ok(if missing.is_empty() {
        CompatibilityReply::Compatible
    } else {
        CompatibilityReply::UnsupportedRequiredCapability { missing }
    })
}
