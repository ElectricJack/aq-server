//! Worker login write-back requests and version decisions.
use super::{Lineage, head_digest, head_version, renewable};
use aq_admission::{Digest, LaunchBinding, MaterialLease, transport::WorkerIdentity};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MaterialRefresh {
    pub schema_version: u32,
    pub operation_id: Uuid,
    pub identity: WorkerIdentity,
    pub binding: LaunchBinding,
    pub lease_id: Uuid,
    /// The version these bytes were refreshed from (N in "N+1 based on N").
    pub base_version: i64,
    pub content_digest: Digest,
}
/// Write-back outcome. `accepted: false` means a newer version won; `version`
/// is then the server's current version (fetched by the next attempt).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RefreshOutcome {
    pub accepted: bool,
    pub version: i64,
    pub replayed: bool,
}

pub enum RefreshDecision {
    Complete(RefreshOutcome),
    Refused,
    Append,
}
impl MaterialRefresh {
    pub fn matches_content(&self, bytes: &[u8]) -> bool {
        (1..=crate::store::MAX_BYTES).contains(&bytes.len())
            && Digest::of(bytes) == self.content_digest
    }
    pub fn evaluate(&self, lineage: &Lineage, lease: &MaterialLease) -> RefreshDecision {
        if let Some(prior) = lineage.entries.iter().find(|e| {
            e.lease_id == Some(lease.lease_id) && e.content_digest == self.content_digest.as_str()
        }) {
            return RefreshDecision::Complete(RefreshOutcome {
                accepted: true,
                version: prior.sequence,
                replayed: true,
            });
        }
        // An operator replace (or revocation, above) ends this lease's login.
        if !renewable(lineage, lease.material_digest.as_str()) {
            return RefreshDecision::Refused;
        }
        let current = head_version(lineage);
        if self.base_version != current {
            // Someone newer won: keep the current version.
            return RefreshDecision::Complete(RefreshOutcome {
                accepted: false,
                version: current,
                replayed: false,
            });
        }
        if head_digest(lineage) == self.content_digest.as_str() {
            // Unchanged bytes: nothing to append.
            return RefreshDecision::Complete(RefreshOutcome {
                accepted: true,
                version: current,
                replayed: true,
            });
        }
        RefreshDecision::Append
    }
}

/// Configured credential the lease names, with its index, if it is still in
/// the loaded registry at the same revision.
fn configured_index(
    loaded: &aq_intelligence_config::LoadedRegistry,
    lease: &MaterialLease,
) -> Option<usize> {
    loaded.credentials.iter().position(|c| {
        c.profile == lease.credential_profile && c.revision == lease.credential_revision
    })
}
pub async fn writable_lineage(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    registry: &aq_intelligence_config::LoadedRegistry,
    lease: &MaterialLease,
    node: uuid::Uuid,
) -> Result<Option<(usize, Lineage)>, super::Error> {
    let Some(index) = configured_index(registry, lease) else {
        return Ok(None);
    };
    let (credential, configured) = (
        &registry.credentials[index],
        &registry.config.credentials[index],
    );
    let Some(base) = registry.fingerprints.get(index).cloned().flatten() else {
        return Ok(None);
    };
    let allowed = configured.kind == aq_selection::CredentialKind::SubscriptionLogin
        && credential.enabled
        && configured.allows(node);
    if !allowed {
        return Ok(None);
    }
    let lineage = super::lineage_locked(
        tx,
        &super::identity(&credential.profile),
        credential.revision,
        &base,
    )
    .await?;
    Ok(Some((index, lineage)))
}
