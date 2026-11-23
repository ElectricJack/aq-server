//! Validate commit identity and publication evidence settings.
use super::*;
/// Commit identity for central candidates and the publication evidence policy.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct IntegrationInput {
    commit_name: String,
    commit_email: String,
    /// Check ids each needing a passing check receipt before publication.
    #[serde(default)]
    checks: Vec<String>,
    #[serde(default)]
    review: bool,
    #[serde(default)]
    judgment_choices: Vec<String>,
}
#[derive(Clone, Debug)]
pub(crate) struct IntegrationSettings {
    pub(crate) commit_name: String,
    pub(crate) commit_email: String,
    pub(crate) evidence: crate::playbooks::host_adapters::EvidencePolicy,
}
impl IntegrationInput {
    pub(super) fn load(self) -> Result<IntegrationSettings> {
        let identities = [&self.commit_name, &self.commit_email]
            .into_iter()
            .all(|v| valid_identity(v));
        let evidence = crate::playbooks::host_adapters::EvidencePolicy {
            checks: self.checks,
            review: self.review,
            judgment_choices: self.judgment_choices,
        };
        let valid = [
            identities,
            valid_names(&evidence.checks),
            valid_names(&evidence.judgment_choices),
            !evidence.names_nothing(),
        ];
        if valid.contains(&false) {
            return Err("invalid playbook integration policy");
        }
        Ok(IntegrationSettings {
            commit_name: self.commit_name,
            commit_email: self.commit_email,
            evidence,
        })
    }
}
fn valid_identity(value: &str) -> bool {
    (1..=256).contains(&value.len()) && !value.contains(['\n', '\r', '\0', '<', '>'])
}
fn valid_names(values: &[String]) -> bool {
    let mut seen = std::collections::BTreeSet::new();
    values.len() <= 16
        && values.iter().all(|value| {
            (1..=64).contains(&value.len()) && !value.contains('\0') && seen.insert(value)
        })
}
