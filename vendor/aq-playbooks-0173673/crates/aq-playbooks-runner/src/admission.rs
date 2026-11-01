//! Immutable namespace-scoped manual admission receipts. The run and key receipt
//! share one storage CAS. Recovery lookup never grants new execution authority.
use crate::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyedAdmissionRequest {
    pub key: String,
    pub binding: String,
    pub expected_activation_revision: u64,
    pub entry: String,
    pub input: Value,
    /// Authenticated and derived by the host, never workflow input authority.
    pub authority: Authority,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KeyedAdmissionReceipt {
    pub key: String,
    pub request_digest: String,
    pub run_id: String,
    pub activation_revision: u64,
    pub catalog_revision: u64,
    pub accepted_at_ms: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyedAdmissionDisposition {
    pub receipt: KeyedAdmissionReceipt,
    pub duplicate: bool,
}
impl KeyedAdmissionRequest {
    fn validate(&self) -> Result<()> {
        fn text(s: &str, max: usize) -> bool {
            !s.is_empty() && s.len() <= max && !s.contains('\0')
        }
        if !text(&self.key, 256)
            || !text(&self.binding, 512)
            || !text(&self.entry, 512)
            || self.expected_activation_revision == 0
            || !text(&self.authority.actor, 512)
            || !text(&self.authority.scope, 512)
            || self.authority.capabilities.len() > 64
            || self.authority.capabilities.iter().any(|s| !text(s, 128))
        {
            return Err(Error::Refused(
                "invalid keyed admission identity or authority bounds".into(),
            ));
        }
        struct Count(usize);
        impl std::io::Write for Count {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if bytes.len() > self.0 {
                    return Err(std::io::Error::other("keyed admission byte bound"));
                }
                self.0 -= bytes.len();
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        serde_json::to_writer(Count(1_048_576), self)
            .map_err(|_| Error::Refused("keyed admission byte bound".into()))?;
        Ok(())
    }
}
impl<S: Storage> Engine<S> {
    /// Historical lookup only. Hosts must authenticate visibility separately.
    /// A receipt survives root GC; its run/output need not still be retained.
    pub fn admission_receipt(&self, key: &str) -> Result<Option<KeyedAdmissionReceipt>> {
        if key.is_empty() || key.len() > 256 || key.contains('\0') {
            return Err(Error::Refused("invalid keyed admission identity".into()));
        }
        Ok(self
            .store
            .load(&self.namespace)?
            .state
            .keyed_admissions
            .get(key)
            .cloned())
    }
    /// Exact replay performs no write, new resolution, quota consumption, or
    /// authority grant. First admission creates run+receipt atomically. Keys are
    /// retained indefinitely; combined receipt/state quotas refuse new work.
    pub fn admit_once(
        &self,
        request: KeyedAdmissionRequest,
        now_ms: i64,
    ) -> Result<KeyedAdmissionDisposition> {
        request.validate()?;
        let request_digest = digest(&request)?;
        self.transaction_choice(|s| {
            if let Some(receipt) = s.keyed_admissions.get(&request.key) {
                if receipt.request_digest != request_digest {
                    return Err(Error::Conflict(
                        "keyed admission identity reused with changed content".into(),
                    ));
                }
                return Ok((
                    KeyedAdmissionDisposition {
                        receipt: receipt.clone(),
                        duplicate: true,
                    },
                    false,
                ));
            }
            let activation = s
                .bindings
                .get(&request.binding)
                .ok_or_else(|| Error::NotFound(request.binding.clone()))?
                .clone();
            if activation.revision != request.expected_activation_revision {
                return Err(Error::Conflict(
                    "keyed admission activation revision".into(),
                ));
            }
            if !activation.controls.calls_enabled || !activation.controls.execution_enabled {
                return Err(Error::Refused("keyed admission activation disabled".into()));
            }
            if activation.authority.actor != request.authority.actor
                || activation.authority.scope != request.authority.scope
                || !request
                    .authority
                    .capabilities
                    .is_subset(&activation.authority.capabilities)
            {
                return Err(Error::Refused(
                    "keyed admission activation authority ceiling".into(),
                ));
            }
            let dependencies = resolve(s, &activation.artifact, &request.authority)?;
            for artifact in std::iter::once(&activation.artifact).chain(dependencies.values()) {
                let selected = s
                    .artifacts
                    .get(artifact)
                    .ok_or_else(|| Error::NotFound(artifact.clone()))?;
                if !execution_allowed(
                    s,
                    artifact,
                    &request.authority,
                    &selected.content.playbook.required_capabilities,
                )? {
                    return Err(Error::Refused(
                        "keyed admission execution authority revoked".into(),
                    ));
                }
            }
            let run_id = next_id(s, "run");
            admit_run(
                s,
                run_id.clone(),
                run_id.clone(),
                None,
                None,
                &activation.artifact,
                &request.entry,
                request.input.clone(),
                request.authority.clone(),
                dependencies,
                0,
                now_ms,
            )?;
            let receipt = KeyedAdmissionReceipt {
                key: request.key.clone(),
                request_digest: request_digest.clone(),
                run_id,
                activation_revision: activation.revision,
                catalog_revision: s.catalog_revision,
                accepted_at_ms: now_ms,
            };
            s.keyed_admissions
                .insert(request.key.clone(), receipt.clone());
            Ok((
                KeyedAdmissionDisposition {
                    receipt,
                    duplicate: false,
                },
                true,
            ))
        })
    }
}

/// Storage adapters reject removal/rebinding even through their public CAS API.
/// No receipt pruning/namespace reset protocol is provided in this version.
pub fn verify_receipt_retention(
    previous: &State,
    next: &State,
) -> std::result::Result<(), StorageError> {
    for (key, receipt) in &previous.keyed_admissions {
        if next.keyed_admissions.get(key) != Some(receipt) {
            return Err(StorageError::Backend(
                "immutable keyed admission receipt removed or changed".into(),
            ));
        }
    }
    Ok(())
}
