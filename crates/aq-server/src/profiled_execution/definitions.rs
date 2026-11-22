//! Daemon-owned public definition bytes. A configured hash is not a retained
//! definition body, and retaining a body does not qualify an adapter or model.
use crate::{AppState, error::AppError};
use aq_admission::{ArtifactKind, ArtifactRef, Digest, RetainedArtifact};
use aq_session_protocol::Validate;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use uuid::Uuid;

pub const MAX_DEFINITIONS: usize = 64;
pub const MAX_BODY_BYTES: usize = 2_097_152;
pub const MAX_CATALOG_BYTES: usize = 8_388_608;
#[derive(Clone, Serialize, Deserialize)]
#[serde(try_from = "DefinitionInput")]
pub struct Definition {
    artifact: ArtifactRef,
    kind: ArtifactKind,
    /// Exact public UTF8 bytes, never interpreted as a command or file path.
    /// The configuration owner must exclude credentials and private material.
    body: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DefinitionInput {
    artifact: ArtifactRef,
    kind: ArtifactKind,
    body: String,
}
impl TryFrom<DefinitionInput> for Definition {
    type Error = String;
    fn try_from(input: DefinitionInput) -> Result<Self, String> {
        Self::new(input.artifact, input.kind, input.body)
    }
}
impl Definition {
    pub fn new(artifact: ArtifactRef, kind: ArtifactKind, body: String) -> Result<Self, String> {
        artifact.id.validate().map_err(|_| "definition identity")?;
        artifact
            .version
            .validate()
            .map_err(|_| "definition version")?;
        if kind != ArtifactKind::Definition
            || body.is_empty()
            || body.len() > MAX_BODY_BYTES
            || artifact.id.as_str().len() > 512
            || artifact.version.as_str().len() > 512
            || artifact.digest != Digest::of(body.as_bytes())
        {
            return Err("definition kind, identity or exact bytes mismatch".into());
        }
        Ok(Self {
            artifact,
            kind,
            body,
        })
    }
    pub fn artifact(&self) -> &ArtifactRef {
        &self.artifact
    }
    pub fn body(&self) -> &str {
        &self.body
    }
}
#[derive(Serialize)]
pub(super) struct DefinitionReference<'a> {
    artifact: &'a ArtifactRef,
    kind: ArtifactKind,
    body_bytes: usize,
}
pub(super) fn references(definitions: &[Definition]) -> Vec<DefinitionReference<'_>> {
    definitions
        .iter()
        .map(|d| DefinitionReference {
            artifact: &d.artifact,
            kind: d.kind,
            body_bytes: d.body.len(),
        })
        .collect()
}
impl std::fmt::Debug for Definition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Definition")
            .field("artifact", &self.artifact)
            .field("kind", &self.kind)
            .field("body_bytes", &self.body.len())
            .finish()
    }
}
pub fn validate_catalog(definitions: &[Definition]) -> Result<(), String> {
    if definitions.len() > MAX_DEFINITIONS {
        return Err("definition catalog count bound".into());
    }
    let mut total = 0usize;
    let mut identities = BTreeSet::new();
    for d in definitions {
        d.artifact
            .id
            .validate()
            .map_err(|_| "definition identity")?;
        d.artifact
            .version
            .validate()
            .map_err(|_| "definition version")?;
        if d.kind != ArtifactKind::Definition
            || d.body.is_empty()
            || d.body.len() > MAX_BODY_BYTES
            || d.artifact.id.as_str().len() > 512
            || d.artifact.version.as_str().len() > 512
            || !identities.insert((d.artifact.id.clone(), d.artifact.version.clone()))
        {
            return Err("definition kind, identity or exact bytes mismatch".into());
        }
        total = total
            .checked_add(d.body.len())
            .ok_or("definition catalog overflow")?;
        if total > MAX_CATALOG_BYTES {
            return Err("definition catalog byte bound".into());
        }
    }
    Ok(())
}
/// Retain the exact required configured bodies through the actual inline owner.
/// This does not construct a manifest or grant. The producer must fence the same
/// global configuration after all external work and before offer/grant commit.
pub async fn retain_required(
    state: &AppState,
    project: Uuid,
    required: &[ArtifactRef],
    retain_until_ms: u64,
) -> Result<Vec<RetainedArtifact>, AppError> {
    let config = state
        .config
        .profiled_execution
        .as_ref()
        .ok_or_else(unavailable)?;
    validate_catalog(&config.definition_bodies).map_err(|_| unavailable())?;
    if project.is_nil() || required.len() > MAX_DEFINITIONS {
        return Err(unavailable());
    }
    let mut seen = BTreeSet::new();
    let mut bodies = Vec::new();
    for artifact in required {
        if !seen.insert((artifact.id.clone(), artifact.version.clone())) {
            return Err(unavailable());
        }
        let body = config
            .definition_bodies
            .iter()
            .find(|d| d.artifact == *artifact)
            .ok_or_else(unavailable)?;
        bodies.push(body);
    }
    let schema: String =
        sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
            .fetch_one(&state.store.pool)
            .await
            .map_err(|_| AppError::Internal)?;
    if schema
        .parse::<u32>()
        .ok()
        .is_none_or(|version| version < 27)
    {
        return Err(unavailable());
    }
    let mut receipts = Vec::new();
    for definition in bodies {
        // Retention expiry belongs to the immutable operation identity: retries
        // preserve it; extending retention needs an explicit new owner operation.
        let identity = serde_json::to_vec(&(project, &definition.artifact, retain_until_ms))
            .map_err(|_| AppError::Internal)?;
        let id = Uuid::new_v5(&Uuid::from_bytes(*b"aq-def-owner-v1!"), &identity);
        receipts.push(
            super::artifacts::retain(
                state,
                super::artifacts::Retain {
                    artifact_id: id,
                    project_id: project,
                    owner: super::artifacts::OwnerKind::Composition,
                    artifact: definition.artifact.clone(),
                    kind: ArtifactKind::Definition,
                    bytes: definition.body.as_bytes(),
                    retain_until_ms,
                },
            )
            .await?,
        );
    }
    Ok(receipts)
}
/// Spec 0017 §6.3: the boot config's definitions and template allowlist with
/// the admitted environment versions, read through one catalog.
#[cfg(feature = "project_environments")]
pub async fn catalog<'e>(
    state: &AppState,
    db: impl sqlx::PgExecutor<'e>,
) -> Result<aq_environments::Catalog<'_>, AppError> {
    let bodies = (state.config.profiled_execution.as_deref()).map(|c| &c.definition_bodies[..]);
    let defs = (bodies.unwrap_or_default().iter()).map(|d| (&d.artifact, d.body.as_str()));
    let boot = aq_environments::BootEnvironments::new(defs, state.config.templates.as_deref());
    // A historical test store, older than migration 0164, has no versions.
    #[cfg(feature = "test-support")]
    if state.store.fixture_profile.is_some_and(|v| v < 164) {
        return Ok(aq_environments::Catalog::boot(&boot));
    }
    (aq_environments::Catalog::load(db, &boot).await).map_err(crate::error::db)
}
/// Spec 0017 §6.3: what a node's inventory lists, its configured harnesses
/// and their admitted counterparts, written when the snapshot is stored.
#[cfg(feature = "project_environments")]
pub async fn listed(
    state: &AppState,
    conn: &mut sqlx::PgConnection,
    node: Uuid,
    configured: Vec<aq_selection::InstalledHarness>,
) -> Result<Vec<aq_selection::InstalledHarness>, AppError> {
    let catalog = catalog(state, &mut *conn).await?;
    (aq_environments::listed(conn, &catalog, node, configured).await).map_err(crate::error::db)
}
/// The qualified harnesses the renewal and phase fences accept: the
/// configured ones and, with `project_environments`, their counterparts for
/// every admitted environment.
#[cfg_attr(not(feature = "project_environments"), allow(unused_variables))]
#[allow(clippy::needless_return)]
pub async fn qualified(
    state: &AppState,
    conn: &mut sqlx::PgConnection,
    configured: &[aq_selection::InstalledHarness],
) -> Result<Vec<aq_selection::InstalledHarness>, AppError> {
    #[cfg(feature = "project_environments")]
    return Ok(catalog(state, conn).await?.qualified(configured));
    #[cfg(not(feature = "project_environments"))]
    Ok(configured.to_vec())
}
fn unavailable() -> AppError {
    AppError::Conflict("required public definition bytes unavailable".into())
}
