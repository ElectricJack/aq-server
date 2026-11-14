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
    /// A receipt stays until its window ends (later while its run is live);
    /// its run need not still be in the State.
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
    /// Exact replay performs no write, new resolution, or authority grant.
    /// First admission creates run+receipt atomically. A key is remembered
    /// until its window (`admission_replay_ms`) expires.
    pub fn admit_once(
        &self,
        request: KeyedAdmissionRequest,
        now_ms: i64,
    ) -> Result<KeyedAdmissionDisposition> {
        self.admit_once_at(request, None, now_ms)
    }
    /// [`Self::admit_once`] with the producer's request time (not part of
    /// the request digest, never stored). A key the engine still remembers
    /// answers as before, whatever its request time. Otherwise a request
    /// time older than `max(now, clock) - policy.admission_replay_ms` is
    /// refused with [`Error::Stale`] and nothing is written. `None`
    /// is not checked: an expired key is then accepted as new.
    pub fn admit_once_at(
        &self,
        request: KeyedAdmissionRequest,
        requested_at_ms: Option<i64>,
        now_ms: i64,
    ) -> Result<KeyedAdmissionDisposition> {
        request.validate()?;
        let request_digest = digest(&request)?;
        // An expired key whose run has left is accepted as new. Its old
        // receipt leaves in a commit of its own first: a receipt is never
        // replaced in place.
        for _ in 0..2 {
            if let Some(disposition) =
                self.admit_once_commit(&request, &request_digest, requested_at_ms, now_ms)?
            {
                return Ok(disposition);
            }
        }
        Err(Error::Conflict(
            "keyed admission receipt changed during admission".into(),
        ))
    }
    /// One commit of [`Self::admit_once`]: `None` when it only removed the
    /// expired receipt of the key.
    fn admit_once_commit(
        &self,
        request: &KeyedAdmissionRequest,
        request_digest: &str,
        requested_at_ms: Option<i64>,
        now_ms: i64,
    ) -> Result<Option<KeyedAdmissionDisposition>> {
        self.transaction_choice_at(Some(now_ms), |s| {
            let window_ms = s.retention.policy.admission_replay_ms;
            if let Some(receipt) = s.keyed_admissions.get(&request.key) {
                let start = retention::window_start(s, now_ms);
                if retention::admission_droppable(s, &request.key, start) {
                    // Not remembered any more: a stale request is refused
                    // before this commit forgets the expired receipt.
                    refuse_stale(s, requested_at_ms, || window_ms, now_ms)?;
                    retention::forget_admission(s, &request.key);
                    return Ok((None, true));
                }
                if receipt.request_digest != request_digest {
                    return Err(Error::Conflict(
                        "keyed admission identity reused with changed content".into(),
                    ));
                }
                return Ok((
                    Some(KeyedAdmissionDisposition {
                        receipt: receipt.clone(),
                        duplicate: true,
                    }),
                    false,
                ));
            }
            refuse_stale(s, requested_at_ms, || window_ms, now_ms)?;
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
                request_digest: request_digest.to_string(),
                run_id,
                activation_revision: activation.revision,
                catalog_revision: s.catalog_revision,
                accepted_at_ms: now_ms,
            };
            s.keyed_admissions
                .insert(request.key.clone(), receipt.clone());
            // Held by its live run: it joins the eviction order when the
            // run leaves (`retention::rejoin`).
            s.retention
                .replay_held
                .insert(receipt.run_id.clone(), request.key.clone());
            let expires_at_ms = retention::window_start(s, now_ms).saturating_add(
                i64::try_from(s.retention.policy.admission_replay_ms).unwrap_or(i64::MAX),
            );
            s.retention
                .admission_expires_at_ms
                .insert(request.key.clone(), expires_at_ms);
            retention::window_insert(
                s,
                expires_at_ms,
                format!("{}{}", retention::WINDOW_ADMISSION, request.key),
            );
            Ok((
                Some(KeyedAdmissionDisposition {
                    receipt,
                    duplicate: false,
                }),
                true,
            ))
        })
    }
}

/// Storage adapters reject rebinding a receipt, and removing one before its
/// window expires, even through their public CAS API. This is part of the
/// shared removal check, [`crate::retention::verify_removals`].
pub fn verify_receipt_retention(
    previous: &State,
    next: &State,
) -> std::result::Result<(), StorageError> {
    crate::retention::verify_removals(previous, next).map(|_| ())
}
