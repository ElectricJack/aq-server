use super::LoadedRegistry;
use aq_selection::AuthorizedCredential;

/// One credential's digest contribution. Untagged tuple variants serialize as
/// plain JSON arrays, so `Legacy` is byte-identical to the pre-C5a1
/// `(credential, allowed_node_ids)` tuple: an unchanged default configuration
/// keeps its stored digest across the upgrade and needs no revision bump.
#[derive(serde::Serialize)]
#[serde(untagged)]
enum Placement<'a> {
    Legacy(&'a aq_selection::AuthorizedCredential, &'a [uuid::Uuid]),
    Kinded(
        &'a aq_selection::AuthorizedCredential,
        &'a [uuid::Uuid],
        aq_selection::CredentialKind,
    ),
    /// Enrolled nodes allowed by default (Q3); only a credential that turns it off adds this.
    Restricted(
        &'a aq_selection::AuthorizedCredential,
        &'a [uuid::Uuid],
        aq_selection::CredentialKind,
        bool,
    ),
}
impl LoadedRegistry {
    pub fn digest(&self) -> aq_selection::Result<aq_selection::Digest> {
        // The backend allowlist participates through `registry.credentials`, whose
        // canonical, skip-if-unrestricted field `load` derives; `c.credential` is
        // validated unrestricted, so it also serializes exactly as before.
        let placements: Vec<_> = self
            .config
            .credentials
            .iter()
            .map(|c| match c.kind {
                kind if !c.allow_enrolled_nodes => {
                    Placement::Restricted(&c.credential, &c.allowed_node_ids, kind, false)
                }
                aq_selection::CredentialKind::ApiKey => {
                    Placement::Legacy(&c.credential, &c.allowed_node_ids)
                }
                kind => Placement::Kinded(&c.credential, &c.allowed_node_ids, kind),
            })
            .collect();
        aq_selection::digest(&(
            self.config.revision,
            placements,
            &self.config.qualified_harnesses,
            &self.credentials,
            &self.fingerprints,
        ))
    }
}

fn definition_changed(old: &AuthorizedCredential, new: &AuthorizedCredential) -> bool {
    old.profile != new.profile
        || old.provider != new.provider
        || old.account_reference != new.account_reference
        || old.endpoint_reference != new.endpoint_reference
}
fn state_regressed(old: &AuthorizedCredential, new: &AuthorizedCredential, rotated: bool) -> bool {
    new.revision < old.revision
        || (new.revision == old.revision
            && (rotated || old.enabled != new.enabled || old.provisionable != new.provisionable))
}
pub struct CredentialDefinition {
    pub identity: String,
    pub definition: aq_selection::Digest,
    pub state_digest: aq_selection::Digest,
    pub revision: u64,
}
impl LoadedRegistry {
    pub fn for_scope<'a>(
        &'a self,
        scope: &'a aq_selection::ObservationScope,
    ) -> impl Iterator<Item = &'a AuthorizedCredential> {
        self.credentials.iter().filter(move |c| {
            c.profile == scope.credential_profile
                && c.revision == scope.credential_revision
                && c.provider == scope.provider
                && c.account_reference == scope.account_reference
                && c.endpoint_reference == scope.endpoint_reference
        })
    }
    pub fn verify_rotation(
        &self,
        old: &[AuthorizedCredential],
        fingerprints: &[Option<String>],
    ) -> Result<(), &'static str> {
        for (index, new) in self.credentials.iter().enumerate() {
            if let Some((old_index, old)) = old.iter().enumerate().find(|(_, c)| {
                c.profile.id == new.profile.id && c.profile.version == new.profile.version
            }) {
                if definition_changed(old, new) {
                    return Err("credential identity/version cannot change definition");
                }
                if state_regressed(
                    old,
                    new,
                    fingerprints.get(old_index) != self.fingerprints.get(index),
                ) {
                    return Err(
                        "credential rotation/state change requires increasing credential revision",
                    );
                }
            }
        }
        Ok(())
    }
    pub fn definitions(&self) -> impl Iterator<Item = aq_selection::Result<CredentialDefinition>> {
        self.credentials
            .iter()
            .enumerate()
            .map(|(index, credential)| {
                let fingerprint = self.fingerprints.get(index).ok_or_else(|| {
                    aq_selection::SelectionError::Invalid(
                        "credential fingerprint slot missing".into(),
                    )
                })?;
                CredentialDefinition::new(credential, fingerprint)
            })
    }
}

impl CredentialDefinition {
    fn new(
        credential: &AuthorizedCredential,
        fingerprint: &Option<String>,
    ) -> aq_selection::Result<Self> {
        let identity = format!(
            "{}@{}",
            credential.profile.id.as_str(),
            credential.profile.version.as_str()
        );
        let definition = aq_selection::digest(&(
            &credential.profile,
            &credential.provider,
            &credential.account_reference,
            &credential.endpoint_reference,
        ))?;
        let state_digest =
            aq_selection::digest(&(credential.enabled, credential.provisionable, fingerprint))?;
        Ok(CredentialDefinition {
            identity,
            definition,
            state_digest,
            revision: credential.revision,
        })
    }
}
#[cfg(test)]
mod tests {
    use crate::IntelligenceConfig;
    use aq_selection::{BackendKind, CredentialKind};

    /// A pre-C5a1 startup configuration: no `kind`, no `allowed_backends`.
    fn legacy_config() -> IntelligenceConfig {
        serde_json::from_value(serde_json::json!({
            "revision": 7,
            "credentials": [{
                "credential": {
                    "profile": {"id":"test", "version":"1", "digest":"a".repeat(64)},
                    "provider":"test", "account_reference":"acct", "endpoint_reference":"endpoint",
                    "revision":3, "enabled":true, "provisionable":false
                },
                "allowed_node_ids": ["00000000-0000-4000-8000-000000000001"],
                "secret_file": null
            }],
            "qualified_harnesses": []
        }))
        .unwrap()
    }
    fn digest(cfg: &IntelligenceConfig) -> aq_selection::Digest {
        cfg.load().unwrap().digest().unwrap()
    }

    /// Pinned from base 86ca279 (pre-C5a1 `configuration`) on this exact
    /// input; see docs/implementation/C5a1.md. A change here forces every
    /// existing installation through a 409 until its revision is bumped.
    const LEGACY_DIGEST: &str = "35c9911673be2ce9dece9de54ea211f45be4b59f83f6b238e08a215cd0873ebd";

    #[test]
    fn credential_legacy_default_config_digest_is_unchanged() {
        let cfg = legacy_config();
        assert_eq!(digest(&cfg).as_str(), LEGACY_DIGEST);
        // Spelling the defaults explicitly, in any order, is the same policy.
        let mut explicit = cfg.clone();
        explicit.credentials[0].kind = CredentialKind::ApiKey;
        explicit.credentials[0].allowed_backends = vec![
            BackendKind::CloudflareContainers,
            BackendKind::DockerLocal,
            BackendKind::Boat,
        ];
        assert_eq!(digest(&explicit).as_str(), LEGACY_DIGEST);
    }

    #[test]
    fn enrolled_flag_changes_digest_only_when_turned_off() {
        let mut cfg = legacy_config();
        assert!(cfg.credentials[0].allow_enrolled_nodes);
        cfg.credentials[0].allow_enrolled_nodes = true;
        assert_eq!(digest(&cfg).as_str(), LEGACY_DIGEST);
        cfg.credentials[0].allow_enrolled_nodes = false;
        assert_ne!(digest(&cfg).as_str(), LEGACY_DIGEST);
    }

    #[test]
    fn credential_kind_and_backend_policy_change_registry_digest() {
        let cfg = legacy_config();
        let baseline = digest(&cfg);
        let mut changed = cfg.clone();
        changed.credentials[0].kind = CredentialKind::SubscriptionLogin;
        assert_ne!(digest(&changed), baseline);
        let mut narrowed = cfg.clone();
        narrowed.credentials[0].allowed_backends = vec![BackendKind::Boat];
        assert_ne!(digest(&narrowed), baseline);
        let mut denied = cfg;
        denied.credentials[0].allowed_backends = vec![];
        assert_ne!(digest(&denied), baseline);
        assert_ne!(digest(&denied), digest(&narrowed));
    }

    #[test]
    fn credential_narrowed_backend_order_does_not_change_digest() {
        let mut first = legacy_config();
        first.credentials[0].allowed_backends =
            vec![BackendKind::CloudflareContainers, BackendKind::DockerLocal];
        let mut second = legacy_config();
        second.credentials[0].allowed_backends =
            vec![BackendKind::DockerLocal, BackendKind::CloudflareContainers];
        assert_eq!(digest(&first), digest(&second));
        assert_eq!(
            first.load().unwrap().credentials[0].allowed_backends,
            vec![BackendKind::DockerLocal, BackendKind::CloudflareContainers]
        );
    }
}
