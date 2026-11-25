//! Current public-Git observation for a confirmed result import. This is not a
//! pre-offer ReservedExport and cannot be constructed from serialized claims.
use super::*;
use aq_admission::Digest as ClaimDigest;

pub struct ResultRetentionObservation {
    confirmation_digest: ClaimDigest,
    inspection: Value,
    observed_at_ms: u64,
    expires_at_ms: u64,
}
fn confirmation_digest(fact: &RecordedSourceConfirmation) -> Result<ClaimDigest> {
    let value = json!({"manifest":fact.manifest,"receipt":fact.receipt,"acknowledgement":fact.acknowledgement});
    Ok(ClaimDigest::of(
        &serde_json::to_vec(&value).map_err(internal)?,
    ))
}
fn unavailable() -> AppError {
    AppError::UpdatesUnavailable("current result retention is not confirmed".into())
}
fn stamp(state: &AppState) -> Result<u64> {
    u64::try_from((state.clock)().timestamp_millis())
        .ok()
        .filter(|v| *v > 0)
        .ok_or_else(unavailable)
}
fn verify(fact: &RecordedSourceConfirmation, v: &Value) -> Result<()> {
    let m = &fact.manifest;
    let record = RetentionRecord(v);
    let matches = record.matches_handle(
        m.artifact_id,
        m.source.repository_id,
        m.source.binding_generation,
    ) && record.active_transfer()
        && record.unreleased()
        && record.matches_ref(m.artifact_id, &fact.receipt.source.hex);
    if serde_json::to_vec(v).map_err(internal)?.len() > 16384 || !matches {
        return Err(unavailable());
    }
    Ok(())
}
impl ResultRetentionObservation {
    fn valid_at(&self, now: u64) -> bool {
        self.observed_at_ms != 0
            && self.expires_at_ms.checked_sub(self.observed_at_ms) == Some(30_000)
            && (self.observed_at_ms..self.expires_at_ms).contains(&now)
    }
    /// Call after reloading the exact owner fact in the admission transaction.
    /// Observation expiry is rechecked after all DB lock waits; no distributed
    /// lock against a later Git release is claimed.
    pub fn validate_current(
        &self,
        state: &AppState,
        fact: &RecordedSourceConfirmation,
    ) -> Result<()> {
        let now = stamp(state)?;
        if self.confirmation_digest != confirmation_digest(fact)? || !self.valid_at(now) {
            return Err(unavailable());
        }
        verify(fact, &self.inspection)
    }
}
/// Call outside database authority locks. Exactly one bounded local public read;
/// no remote fetch, retry, caller-selected socket, or renewed execution grant.
pub async fn inspect_current(
    state: &AppState,
    fact: &RecordedSourceConfirmation,
) -> Result<ResultRetentionObservation> {
    let rt = runtime(state)?;
    let _permit = rt.bulk.try_acquire().map_err(|_| unavailable())?;
    let start = stamp(state)?;
    let request_id = Uuid::new_v4().to_string();
    let m = &fact.manifest;
    let request = json!({"protocol_version":1,"request_id":request_id,"operation":"retention.inspect","repository_id":m.source.repository_id,"expected_binding_generation":m.source.binding_generation,"params":{"handle_id":m.artifact_id,"limit":1},"deadline_ms":5000});
    let call = async {
        let mut socket = tokio::net::UnixStream::connect(&rt.config.git_socket)
            .await
            .map_err(|_| unavailable())?;
        let mut bytes = serde_json::to_vec(&request).map_err(internal)?;
        bytes.push(b'\n');
        socket.write_all(&bytes).await.map_err(|_| unavailable())?;
        let mut frame = Vec::new();
        BufReader::new(socket)
            .take(32769)
            .read_until(b'\n', &mut frame)
            .await
            .map_err(|_| unavailable())?;
        if frame.len() > 32768 || frame.last() != Some(&b'\n') {
            return Err(unavailable());
        }
        let reply: Value = serde_json::from_slice(&frame).map_err(|_| unavailable())?;
        if !successful_reply(&reply, &request["request_id"]) {
            return Err(unavailable());
        }
        Ok(reply["result"].clone())
    };
    let inspection = tokio::time::timeout(Duration::from_secs(5), call)
        .await
        .map_err(|_| unavailable())??;
    let proof = ResultRetentionObservation {
        confirmation_digest: confirmation_digest(fact)?,
        inspection,
        observed_at_ms: start,
        expires_at_ms: start.checked_add(30000).ok_or_else(unavailable)?,
    };
    proof.validate_current(state, fact)?;
    Ok(proof)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn synthetic() -> (RecordedSourceConfirmation, Value) {
        let repository_id = Uuid::new_v4();
        let artifact_id = Uuid::new_v4();
        let transfer_id = Uuid::new_v4();
        let object = aq_worker_protocol::ObjectId {
            algorithm: aq_worker_protocol::ObjectAlgorithm::Sha1,
            hex: "a".repeat(40),
        };
        let manifest = TransferManifest {
            transfer_id,
            role: aq_worker_protocol::SourceRole::Checkpoint,
            checkpoint: object.clone(),
            attempt: aq_worker_protocol::AttemptRef {
                attempt_id: Uuid::new_v4(),
                session_id: Uuid::new_v4(),
                generation: 1,
            },
            resolved_spec_digest: "b".repeat(64),
            source: SourceBinding {
                repository_id,
                binding_generation: 1,
                logical_identity: "synthetic".into(),
                base: object.clone(),
            },
            artifact_id,
            manifest_digest: "c".repeat(64),
            bundle_digest: None,
            manifest_bytes: 1,
            bundle_bytes: 0,
        };
        let receipt = CentralRetentionReceipt {
            receipt_id: Uuid::new_v4(),
            transfer_id,
            repository_id,
            binding_generation: 1,
            source: object,
            retention_handle: artifact_id,
            manifest_digest: "d".repeat(64),
        };
        let inspection = json!({"version":1,"id":artifact_id,"repository_id":repository_id,"binding_generation":1,"generation":1,"state":"active","resource":"transfer","release_operation":null,"release_reason":null,"refs":[{"name":format!("refs/aq-git/transfers/{artifact_id}"),"oid":"a".repeat(40)}]});
        (
            RecordedSourceConfirmation {
                manifest,
                receipt,
                acknowledgement: json!({"synthetic":true}),
            },
            inspection,
        )
    }
    #[test]
    fn current_result_inspection_requires_exact_active_handle_and_single_source_ref() {
        let (fact, valid) = synthetic();
        verify(&fact, &valid).unwrap();
        for (field, value) in [
            ("state", json!("released")),
            ("state", json!("releasing")),
            ("id", json!(Uuid::new_v4())),
            ("repository_id", json!(Uuid::new_v4())),
            ("binding_generation", json!(2)),
            ("generation", json!(0)),
            ("resource", json!("workspace")),
            ("release_operation", json!(Uuid::new_v4())),
            ("release_reason", json!("released")),
            ("refs", json!([])),
        ] {
            let mut bad = valid.clone();
            bad[field] = value;
            assert!(verify(&fact, &bad).is_err(), "{field}");
        }
        let mut bad = valid.clone();
        bad["refs"][0]["oid"] = json!("e".repeat(40));
        assert!(verify(&fact, &bad).is_err());
        let mut bad = valid.clone();
        bad["refs"]
            .as_array_mut()
            .unwrap()
            .push(valid["refs"][0].clone());
        assert!(verify(&fact, &bad).is_err());
    }
    #[test]
    fn observed_confirmation_digest_changes_for_exact_manifest_receipt_and_ack_bytes() {
        let (mut fact, _) = synthetic();
        let original = confirmation_digest(&fact).unwrap();
        fact.manifest.attempt.generation += 1;
        assert_ne!(confirmation_digest(&fact).unwrap(), original);
        fact.manifest.attempt.generation -= 1;
        fact.receipt.source.hex = "e".repeat(40);
        assert_ne!(confirmation_digest(&fact).unwrap(), original);
        fact.receipt.source.hex = "a".repeat(40);
        fact.acknowledgement["changed"] = json!(true);
        assert_ne!(confirmation_digest(&fact).unwrap(), original);
    }
}
