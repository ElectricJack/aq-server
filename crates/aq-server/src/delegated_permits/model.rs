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
        if permit_id.is_nil() || boot.is_nil() {
            return Err(ContractError::InvalidShape);
        }
        let mut mac = Hmac::<Sha256>::new_from_slice(self.0.as_slice())
            .map_err(|_| ContractError::InvalidShape)?;
        mac.update(b"aq.delegated-permit.incarnation/v1\0");
        mac.update(permit_id.as_bytes());
        mac.update(boot.as_bytes());
        let bytes = Zeroizing::new(mac.finalize().into_bytes().to_vec());
        let token = Zeroizing::new(format!("aqp_{}", URL_SAFE_NO_PAD.encode(bytes.as_slice())));
        serde_json::from_value(serde_json::Value::String(token.to_string()))
            .map_err(|_| ContractError::InvalidShape)
    }

    pub fn credential(&self, permit_id: Uuid) -> Result<PermitCredential> {
        if permit_id.is_nil() {
            return Err(ContractError::InvalidShape);
        }
        let mut mac = Hmac::<Sha256>::new_from_slice(self.0.as_slice())
            .map_err(|_| ContractError::InvalidShape)?;
        mac.update(b"aq.delegated-permit.credential/v1\0");
        mac.update(permit_id.as_bytes());
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
