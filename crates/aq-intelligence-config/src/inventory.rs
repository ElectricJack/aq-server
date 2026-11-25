//! Admission of installed capabilities against the owner's current registry.
use super::IntelligenceConfig;
use aq_selection::{ArtifactRef, CredentialBinding, InstalledHarness};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstalledCredential {
    pub profile: ArtifactRef,
    pub credential_revision: u64,
    pub available: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields, bound(deserialize = "S: Deserialize<'de>"))]
pub struct InventoryReport<S> {
    pub operation_id: Uuid,
    pub boot_id: Uuid,
    pub sequence: u64,
    pub ttl_ms: u64,
    pub qualifications: Vec<ArtifactRef>,
    pub credentials: Vec<InstalledCredential>,
    /// SBX25 measured disk accounting; absent from older nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage: Option<S>,
}
impl<S> InventoryReport<S> {
    pub fn valid_bounds(&self) -> bool {
        let ids = !self.operation_id.is_nil() && !self.boot_id.is_nil();
        let sequence = (1..=i64::MAX as u64).contains(&self.sequence);
        let lifetime = (1000..=120_000).contains(&self.ttl_ms);
        let capacity = self.qualifications.len() <= 16 && self.credentials.len() <= 32;
        ids && sequence && lifetime && capacity
    }
}

#[derive(Clone, Copy)]
pub struct ObservedWindow {
    pub observed_at_ms: u64,
    pub expires_at_ms: u64,
}
impl ObservedWindow {
    pub fn new(observed_at_ms: u64, ttl_ms: u64) -> Option<Self> {
        Some(Self {
            observed_at_ms,
            expires_at_ms: observed_at_ms.checked_add(ttl_ms)?,
        })
    }
    pub fn contains(&self, time: u64) -> bool {
        (self.observed_at_ms..self.expires_at_ms).contains(&time)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum AdmissionError {
    Invalid(&'static str),
    Forbidden,
}

pub struct NodeAdmission {
    pub id: Uuid,
    pub enrolled: bool,
    pub window: ObservedWindow,
}

impl IntelligenceConfig {
    pub fn installed_harnesses(
        &self,
        qualifications: &[ArtifactRef],
    ) -> Result<Vec<InstalledHarness>, &'static str> {
        let mut harnesses = Vec::new();
        for qualification in qualifications {
            if harnesses
                .iter()
                .any(|h: &InstalledHarness| &h.qualification == qualification)
            {
                return Err("duplicate harness qualification");
            }
            let harness = self
                .qualified_harnesses
                .iter()
                .find(|h| &h.qualification == qualification)
                .ok_or("unrecognized qualified harness")?;
            harnesses.push(harness.clone());
        }
        Ok(harnesses)
    }

    pub fn installed_bindings(
        &self,
        node: NodeAdmission,
        credentials: &[InstalledCredential],
    ) -> Result<Vec<CredentialBinding>, AdmissionError> {
        let mut bindings = Vec::new();
        for presence in credentials {
            if bindings
                .iter()
                .any(|c: &CredentialBinding| c.credential_profile == presence.profile)
            {
                return Err(AdmissionError::Invalid(
                    "duplicate installed credential profile",
                ));
            }
            bindings.push(self.installed_binding(&node, presence)?);
        }
        Ok(bindings)
    }

    fn installed_binding(
        &self,
        node: &NodeAdmission,
        presence: &InstalledCredential,
    ) -> Result<CredentialBinding, AdmissionError> {
        let configured = self
            .credentials
            .iter()
            .find(|c| {
                c.credential.profile == presence.profile
                    && c.credential.revision == presence.credential_revision
            })
            .ok_or(AdmissionError::Invalid("unknown exact credential revision"))?;
        if !configured.admits(node.id, node.enrolled) {
            return Err(AdmissionError::Forbidden);
        }
        if presence.available && configured.delivery_mode.is_some() {
            return Err(AdmissionError::Invalid(
                "server material installation requires an exact verified delivery receipt",
            ));
        }
        Ok(CredentialBinding {
            credential_revision: presence.credential_revision,
            credential_profile: presence.profile.clone(),
            available: presence.available,
            preparation: None,
            observed_at_ms: node.window.observed_at_ms,
            expires_at_ms: node.window.expires_at_ms,
        })
    }
}

pub fn combine_preparations(
    bindings: &mut Vec<CredentialBinding>,
    preparations: Vec<CredentialBinding>,
) {
    for preparation in preparations {
        let existing = bindings.iter_mut().find(|c| {
            c.credential_profile == preparation.credential_profile
                && c.credential_revision == preparation.credential_revision
        });
        match existing {
            Some(binding) if !binding.available => *binding = preparation,
            None => bindings.push(preparation),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    #[test]
    fn report_roundtrip_preserves_optional_storage_and_bounds() {
        let legacy = json!({
            "operation_id": Uuid::from_u128(7), "boot_id": Uuid::from_u128(9),
            "sequence": 1, "ttl_ms": 1000, "qualifications": [], "credentials": [],
        });
        let mut measured = legacy.clone();
        measured["storage"] = json!({"disk_total_bytes": 4096});
        for value in [legacy, measured] {
            let mut report: InventoryReport<Value> = serde_json::from_value(value.clone()).unwrap();
            assert!(report.valid_bounds());
            assert_eq!(serde_json::to_value(&report).unwrap(), value);
            report.sequence = i64::MAX as u64 + 1;
            assert!(!report.valid_bounds());
        }
    }
}
