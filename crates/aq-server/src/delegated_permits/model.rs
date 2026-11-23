use aq_admission_sandbox::sandbox::{
    ContractError, DelegatedPermit, PermitBinding, PermitContext, PermitCredential, PermitScope,
    RunnerOperation,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use uuid::Uuid;
use zeroize::Zeroizing;

type Result<T> = std::result::Result<T, ContractError>;

/// Server-private, separately backed-up key. Changing it makes retained issuance
/// undeliverable; never generate a replacement implicitly during restart.
pub struct CredentialKey(Zeroizing<[u8; 32]>);
impl std::fmt::Debug for CredentialKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CredentialKey([REDACTED])")
    }
}
impl CredentialKey {
    pub fn new(bytes: [u8; 32]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    pub(crate) fn incarnation_credential(
        &self,
        permit_id: Uuid,
        boot: Uuid,
    ) -> Result<PermitCredential> {
        self.derive_credential(b"aq.delegated-permit.incarnation/v1\0", &[permit_id, boot])
    }

    pub fn credential(&self, permit_id: Uuid) -> Result<PermitCredential> {
        self.derive_credential(b"aq.delegated-permit.credential/v1\0", &[permit_id])
    }

    fn derive_credential(&self, domain: &[u8], ids: &[Uuid]) -> Result<PermitCredential> {
        if ids.iter().any(Uuid::is_nil) {
            return Err(ContractError::InvalidShape);
        }
        let mut mac = Hmac::<Sha256>::new_from_slice(self.0.as_slice())
            .map_err(|_| ContractError::InvalidShape)?;
        mac.update(domain);
        for id in ids {
            mac.update(id.as_bytes());
        }
        let bytes = Zeroizing::new(mac.finalize().into_bytes().to_vec());
        let token = Zeroizing::new(format!("aqp_{}", URL_SAFE_NO_PAD.encode(bytes.as_slice())));
        serde_json::from_value(serde_json::Value::String(token.to_string()))
            .map_err(|_| ContractError::InvalidShape)
    }
}

/// Persistable metadata. Deserialization is deliberately not authorization.
/// Every use validates shape, stored verifier, revocation and current context.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermitRecord {
    pub permit_id: Uuid,
    pub binding: PermitBinding,
    pub scope: PermitScope,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
    pub hard_deadline_ms: u64,
    verifier: [u8; 32],
    revoked: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    credential_boot: Option<Uuid>,
}
impl PermitRecord {
    pub fn from_issued(permit: &DelegatedPermit) -> Result<Self> {
        permit.validate()?;
        Ok(Self {
            permit_id: permit.permit_id,
            binding: permit.binding.clone(),
            scope: permit.scope,
            issued_at_ms: permit.issued_at_ms,
            expires_at_ms: permit.expires_at_ms,
            hard_deadline_ms: permit.hard_deadline_ms,
            verifier: Sha256::digest(permit.credential.expose().as_bytes()).into(),
            revoked: false,
            credential_boot: None,
        })
    }

    fn document(&self, credential: PermitCredential) -> DelegatedPermit {
        DelegatedPermit {
            schema_version: 1,
            permit_id: self.permit_id,
            credential,
            binding: self.binding.clone(),
            scope: self.scope,
            issued_at_ms: self.issued_at_ms,
            expires_at_ms: self.expires_at_ms,
            hard_deadline_ms: self.hard_deadline_ms,
        }
    }

    pub(crate) fn verifier(&self) -> [u8; 32] {
        self.verifier
    }

    pub fn verify_credential(&self, token: &str) -> Result<()> {
        if self.revoked || !(32..=256).contains(&token.len()) {
            return Err(ContractError::ScopeDenied);
        }
        let candidate: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        if bool::from(self.verifier.ct_eq(&candidate)) {
            Ok(())
        } else {
            Err(ContractError::ScopeDenied)
        }
    }

    pub fn delivery(&self, key: &CredentialKey) -> Result<DelegatedPermit> {
        let credential = match self.credential_boot {
            Some(boot) if boot == self.binding.node.boot_id => {
                key.incarnation_credential(self.permit_id, boot)?
            }
            Some(_) => return Err(ContractError::BindingMismatch),
            None => key.credential(self.permit_id)?,
        };
        self.verify_credential(credential.expose())?;
        let document = self.document(credential);
        document.validate()?;
        Ok(document)
    }

    pub fn authorize(
        &self,
        current: &PermitContext,
        operation: RunnerOperation,
        now_ms: u64,
    ) -> Result<()> {
        if self.revoked {
            return Err(ContractError::ScopeDenied);
        }
        // The contract validates metadata independently of authentication. This
        // fixed non-secret placeholder never leaves this method or authorizes a
        // bearer; callers separately verify the presented credential.
        let placeholder = serde_json::from_value(serde_json::Value::String("x".repeat(32)))
            .map_err(|_| ContractError::InvalidShape)?;
        self.document(placeholder)
            .validate_for(current, operation, now_ms)
    }

    pub(crate) fn rebind(&mut self, key: &CredentialKey, boot: Uuid) -> Result<()> {
        let credential = key.incarnation_credential(self.permit_id, boot)?;
        self.binding.node.boot_id = boot;
        self.credential_boot = Some(boot);
        self.verifier = Sha256::digest(credential.expose().as_bytes()).into();
        Ok(())
    }

    pub fn revoke(&mut self) {
        self.revoked = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_derivation_preserves_retained_v1_bytes() {
        let key = CredentialKey::new([7; 32]);
        let permit = Uuid::from_u128(1);
        let boot = Uuid::from_u128(6);
        // Digests pin both domain separators and UUID byte order without
        // retaining bearer credentials in the source.
        for (credential, expected) in [
            (
                key.credential(permit).unwrap(),
                [
                    0xce, 0xeb, 0x8a, 0x00, 0xd4, 0xc4, 0x14, 0x61, 0xc6, 0x4e, 0x36, 0xf1, 0xa6,
                    0x99, 0xc2, 0x52, 0xf6, 0x1e, 0xcd, 0xa3, 0x02, 0xca, 0x53, 0x85, 0xf0, 0x52,
                    0x7f, 0x75, 0x9e, 0x6b, 0xac, 0xac,
                ],
            ),
            (
                key.incarnation_credential(permit, boot).unwrap(),
                [
                    0x74, 0xbf, 0xe7, 0xd4, 0x11, 0x5b, 0xd6, 0x36, 0x79, 0xcd, 0xf1, 0x08, 0x3f,
                    0x27, 0x3d, 0x44, 0xc5, 0xb8, 0x46, 0x03, 0x54, 0x8b, 0x00, 0x06, 0xc2, 0xcd,
                    0x93, 0x94, 0xa5, 0x44, 0xcc, 0x9b,
                ],
            ),
        ] {
            let encoded = credential.expose().strip_prefix("aqp_").unwrap();
            assert_eq!(URL_SAFE_NO_PAD.decode(encoded).unwrap(), expected);
        }
    }

    #[test]
    fn credential_derivation_rejects_nil_bindings() {
        let key = CredentialKey::new([7; 32]);
        let id = Uuid::from_u128(1);
        assert!(key.credential(Uuid::nil()).is_err());
        assert!(key.incarnation_credential(Uuid::nil(), id).is_err());
        assert!(key.incarnation_credential(id, Uuid::nil()).is_err());
    }
}
