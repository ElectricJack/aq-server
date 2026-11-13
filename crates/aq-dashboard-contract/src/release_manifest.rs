//! Dashboard release metadata and protocol vocabulary, independent of user grants.
//! The caller supplies its bounded asset-map deserializer as `Files`.
use crate::{ApiVersion, ContractVersion};
use anyhow::{Result, ensure};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest<Files> {
    pub schema_version: u32,
    api_version: ApiVersion,
    pub files: Files,
    version: Option<String>,
    source: Option<Source>,
    toolchain: Option<Toolchain>,
    required_api_versions: Option<Vec<ContractVersion>>,
    required_capabilities: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Source {
    revision: String,
    dirty: bool,
    official: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Toolchain {
    node: String,
    pnpm: String,
    lockfile_sha256: String,
}

// These are implemented operations advertised by bootstrap, including optional
// mounted owners. This is not a promise of an enabled grant for every user.
// supervisor.conversation is deliberately excluded: it is not implemented.
const CAPABILITIES: &[&str] = &[
    "dashboard.overview",
    "sessions.read",
    "session.usage.read",
    "session.events.read",
    "projects.read",
    "status.read",
    "workers.read",
    "notifications.read",
    "human.graph.read",
    "human.overview.tasks.read",
    "tasks.read",
    "tasks.comments.read",
    "tasks.comments.write",
    "tasks.create",
    "tasks.edit",
    "tasks.reprioritize",
    "tasks.dependencies.add",
    "tasks.dependencies.remove",
    "tasks.delete",
    "tasks.cancel",
    "tasks.retry",
    "knowledge.read",
    "knowledge.author",
    "knowledge.correct",
    "knowledge.verify",
    "knowledge.retire",
    "knowledge.restore",
    "search.read",
    "decisions.read",
    "decisions.resolve",
    "providers.configured.read",
    "provider.quota",
    "supervisor.history.read",
    "supervisor.message.enqueue",
    "session.message.enqueue",
    "terminal.watch",
    "terminal.control",
    "dashboard.actions",
    "results.read",
    "results.comment",
    "results.feedback",
    "results.media.read",
    "results.media.upload",
    "playbooks.inspect",
    "playbooks.loaded-definitions.read",
    "playbooks.catalog.read",
    "playbooks.runs.start",
    "playbooks.runs.cancel",
    "playbooks.activations.activate",
    "playbooks.activations.rollback",
];

fn lowercase_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl<Files> Manifest<Files> {
    pub fn validate_compatibility(&self) -> Result<()> {
        ensure!(
            matches!(self.schema_version, 1 | 2) && matches!(self.api_version, ApiVersion::Read1),
            "unsupported dashboard manifest version"
        );
        if self.schema_version == 1 {
            ensure!(
                self.version.is_none()
                    && self.source.is_none()
                    && self.toolchain.is_none()
                    && self.required_api_versions.is_none()
                    && self.required_capabilities.is_none(),
                "release metadata requires dashboard manifest schema 2"
            );
            return Ok(());
        }
        ensure!(
            self.version
                .as_ref()
                .is_some_and(|version| semver::Version::parse(version).is_ok()),
            "dashboard release version missing or invalid"
        );
        ensure!(
            self.source.as_ref().is_some_and(|source| {
                lowercase_hex(&source.revision, 40) && !(source.official && source.dirty)
            }),
            "dashboard source provenance missing or invalid"
        );
        ensure!(
            self.toolchain.as_ref().is_some_and(|toolchain| {
                !toolchain.node.is_empty()
                    && !toolchain.pnpm.is_empty()
                    && lowercase_hex(&toolchain.lockfile_sha256, 64)
            }),
            "dashboard toolchain missing or invalid"
        );
        // Unknown required contracts are refused by ContractVersion's strict
        // deserializer, shared with the actual bootstrap response contract.
        ensure!(
            self.required_api_versions.as_ref().is_some_and(|versions| {
                versions
                    .iter()
                    .any(|version| matches!(version, ContractVersion::Read1))
            }),
            "dashboard required API contracts missing dashboard-read-1"
        );
        let capabilities = self
            .required_capabilities
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("dashboard required capabilities missing"))?;
        for capability in capabilities {
            ensure!(
                CAPABILITIES.contains(&capability.as_str()),
                "unsupported required dashboard capability: {capability}"
            );
        }
        Ok(())
    }
}
