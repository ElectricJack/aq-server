//! Fixed pre-offer Git export I/O. SQL reservation/finalization are owned by
//! profiled_execution::source_exports; this module cannot mint launch authority.
use super::{
    AppError, AppState, GitManifest, Result, RetentionRecord, Runtime, json, runtime,
    successful_reply,
};
pub use crate::profiled_execution::source_exports::Request;
use crate::profiled_execution::source_exports::{LinkedExport, ReservedExport};
use aq_admission::Digest;
use aq_delivery::source_transport::files::Installation;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use uuid::Uuid;
const MAX_MANIFEST: usize = 1_048_576;
const MAX_RETENTION: usize = 16_384;
pub const MAX_VERIFIED_BYTES: usize = 4_194_304;
fn file_slot() -> Result<tokio::sync::OwnedSemaphorePermit> {
    (aq_machine_control::limits::EXPORT_FILES.enter()).ok_or_else(pending)
}
fn refused() -> AppError {
    AppError::Conflict("pre-offer source export unavailable or inconsistent".into())
}
fn pending() -> AppError {
    AppError::UpdatesUnavailable("source export remains owned; reconcile original operation".into())
}
fn canonical(value: &impl Serialize) -> Result<Vec<u8>> {
    let mut value = serde_json::to_value(value).map_err(|_| AppError::Internal)?;
    value.sort_all_objects();
    serde_json::to_vec(&value).map_err(|_| AppError::Internal)
}
fn stamp(state: &AppState) -> Result<u64> {
    u64::try_from((state.clock)().timestamp_millis())
        .ok()
        .filter(|t| *t > 0)
        .ok_or_else(refused)
}
fn correlation(request: &Request) -> Value {
    json!({"owner":"aq.profiled-base-v1","project_id":request.project_id,"project_revision":request.project_revision,"source_observation":request.source_observation})
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifiedExport {
    schema_version: u32,
    request_digest: Digest,
    export_operation_id: Uuid,
    manifest_json: String,
    manifest_digest: Digest,
    bundle_bytes: u64,
    bundle_digest: Digest,
    producer_installation_id: Uuid,
    export_response: Value,
    retention_inspection: Value,
    observed_at_ms: u64,
    expires_at_ms: u64,
}
impl VerifiedExport {
    pub fn request_digest(&self) -> &Digest {
        &self.request_digest
    }
    pub fn operation_id(&self) -> Uuid {
        self.export_operation_id
    }
    pub fn manifest_bytes(&self) -> &[u8] {
        self.manifest_json.as_bytes()
    }
    pub fn manifest_digest(&self) -> &Digest {
        &self.manifest_digest
    }
    pub fn bundle_bytes(&self) -> u64 {
        self.bundle_bytes
    }
    pub fn bundle_digest(&self) -> &Digest {
        &self.bundle_digest
    }
    pub fn observed_at_ms(&self) -> u64 {
        self.observed_at_ms
    }
    pub fn expires_at_ms(&self) -> u64 {
        self.expires_at_ms
    }
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        let bytes = canonical(self)?;
        if bytes.len() > MAX_VERIFIED_BYTES {
            return Err(refused());
        }
        Ok(bytes)
    }
    /// Decode only an immutable owner record already authenticated by SQL. This
    /// checks structure/linkage, not current retention or a new physical export.
    pub fn decode_stored(token: &ReservedExport, bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_VERIFIED_BYTES {
            return Err(refused());
        }
        let fact: Self = serde_json::from_slice(bytes).map_err(|_| refused())?;
        fact.validate_for(token)?;
        if fact.canonical_bytes()? != bytes {
            return Err(refused());
        }
        Ok(fact)
    }
    pub fn validate_for(&self, token: &ReservedExport) -> Result<()> {
        let request = token.request();
        request.validate().map_err(|_| refused())?;
        if self.schema_version != 1 {
            return Err(refused());
        }
        verify_operation(token, &self.request_digest, self.export_operation_id)?;
        self.verify_manifest_bytes()?;
        if !valid_observation(self.observed_at_ms, self.expires_at_ms) || !self.fits(token) {
            return Err(refused());
        }
        let manifest: GitManifest =
            serde_json::from_str(&self.manifest_json).map_err(|_| refused())?;
        verify_manifest(request, &manifest, &self.export_response, self.bundle_bytes)?;
        if !self.matches_bundle(&manifest) {
            return Err(refused());
        }
        verify_retention(request, &self.retention_inspection)?;
        self.canonical_bytes()?;
        Ok(())
    }
    fn verify_manifest_bytes(&self) -> Result<()> {
        let matches = self.manifest_json.len() <= MAX_MANIFEST
            && self.manifest_digest == Digest::of(self.manifest_bytes())
            && !self.producer_installation_id.is_nil();
        if matches { Ok(()) } else { Err(refused()) }
    }
    fn fits(&self, reservation: &ReservedExport) -> bool {
        self.bundle_bytes != 0
            && self
                .bundle_bytes
                .checked_add(self.manifest_json.len() as u64)
                .is_some_and(|n| n <= reservation.reserved_bytes())
    }
    fn matches_bundle(&self, manifest: &GitManifest) -> bool {
        manifest.bundle_size == self.bundle_bytes
            && manifest.bundle_sha256.as_deref() == Some(self.bundle_digest.as_str())
            && manifest.producer_installation_id == self.producer_installation_id.to_string()
    }
}
fn verify_operation(token: &ReservedExport, digest: &Digest, operation: Uuid) -> Result<()> {
    let matches = digest == token.request_digest()
        && token.request().digest().map_err(|_| refused())? == *digest
        && operation == token.request().operation_id;
    if matches { Ok(()) } else { Err(refused()) }
}
fn valid_observation(start: u64, expires: u64) -> bool {
    start != 0
        && expires <= i64::MAX as u64
        && expires
            .checked_sub(start)
            .is_some_and(|span| (1..=30_000).contains(&span))
}
fn matches_reservation(m: &GitManifest, request: &Request) -> bool {
    m.repository_id == request.repository_id.to_string()
        && m.repository_identity == request.logical_identity
        && m.binding_generation == request.binding_generation
        && m.transfer_id == request.operation_id.to_string()
}
fn verify_manifest(
    request: &Request,
    m: &GitManifest,
    response: &Value,
    max_bundle: u64,
) -> Result<()> {
    if !matches_reservation(m, request) || !m.is_full_base(&request.base, max_bundle) {
        return Err(refused());
    }
    if m.caller_correlation != correlation(request) || !m.has_producer() {
        return Err(refused());
    }
    if !m
        .matches_export_response(response, request.operation_id)
        .map_err(|_| AppError::Internal)?
        || canonical(response)?.len() > MAX_MANIFEST
    {
        return Err(refused());
    }
    let digest = m.bundle_sha256.as_ref().ok_or_else(refused)?;
    serde_json::from_value::<Digest>(json!(digest)).map_err(|_| refused())?;
    Ok(())
}
fn verify_retention(request: &Request, value: &Value) -> Result<()> {
    let record = RetentionRecord(value);
    let matches = record.matches_handle(
        request.operation_id,
        request.repository_id,
        request.binding_generation,
    ) && record.active_transfer()
        && record.unreleased()
        && record.matches_ref(request.operation_id, &request.base.hex);
    if canonical(value)?.len() > MAX_RETENTION || !matches {
        return Err(refused());
    }
    Ok(())
}
async fn exchange(rt: &Runtime, request: &Value, cap: usize, timeout: Duration) -> Result<Value> {
    let call = async {
        let mut socket = tokio::net::UnixStream::connect(&rt.config.git_socket)
            .await
            .map_err(|_| pending())?;
        let mut bytes = canonical(request)?;
        bytes.push(b'\n');
        socket.write_all(&bytes).await.map_err(|_| pending())?;
        let mut frame = Vec::new();
        BufReader::new(socket)
            .take((cap + 1) as u64)
            .read_until(b'\n', &mut frame)
            .await
            .map_err(|_| pending())?;
        if frame.len() > cap || frame.last() != Some(&b'\n') {
            return Err(pending());
        }
        let response: Value = serde_json::from_slice(&frame).map_err(|_| pending())?;
        if !successful_reply(&response, &request["request_id"])
            || request
                .get("operation_id")
                .is_some_and(|id| response.get("operation_id") != Some(id))
        {
            return Err(pending());
        }
        response.get("result").cloned().ok_or_else(pending)
    };
    tokio::time::timeout(timeout, call)
        .await
        .map_err(|_| pending())?
}
async fn inspect(rt: &Runtime, request: &Request) -> Result<Value> {
    let body = json!({"protocol_version":1,"request_id":format!("profiled-retention-{}",Uuid::new_v4()),"operation":"retention.inspect","repository_id":request.repository_id,"expected_binding_generation":request.binding_generation,"params":{"handle_id":request.operation_id,"limit":1},"deadline_ms":5000});
    let value = exchange(rt, &body, MAX_RETENTION, Duration::from_secs(5)).await?;
    verify_retention(request, &value)?;
    Ok(value)
}
/// The caller must persist a token reservation before I/O and preserve it after
/// every error/timeout. This returns facts, not a launch/attempt/transfer wrapper.
pub async fn verify_export(state: &AppState, token: &ReservedExport) -> Result<VerifiedExport> {
    let request = token.request();
    request.validate().map_err(|_| refused())?;
    if request.digest().map_err(|_| refused())? != *token.request_digest() {
        return Err(refused());
    }
    let rt = runtime(state)?;
    let _bulk = rt.bulk.try_acquire().map_err(|_| pending())?;
    // A timed-out regular-file read keeps this slot until its OS call actually
    // returns. Cancellation cannot accumulate unbounded blocking hash workers.
    let slot = file_slot()?;
    if token.reserved_bytes()
        < rt.config
            .max_bundle_bytes
            .checked_add(MAX_MANIFEST as u64)
            .ok_or_else(refused)?
    {
        return Err(refused());
    }
    let body = json!({"protocol_version":1,"request_id":format!("profiled-base-export-{}",request.operation_id),"operation":"artifact.export","repository_id":request.repository_id,"operation_id":request.operation_id,"expected_binding_generation":request.binding_generation,"params":{"source":request.base,"base":request.base,"retention_handle":request.source_retention_handle,"confirmed_prerequisites":[],"reference_only":false,"caller_correlation":correlation(request)},"deadline_ms":30000});
    let response = exchange(&rt, &body, MAX_MANIFEST, Duration::from_secs(35)).await?;
    let observed_at_ms = stamp(state)?;
    let root = rt.config.git_root.clone();
    let request_for_read = request.clone();
    let response_for_read = response.clone();
    let max = rt.config.max_bundle_bytes;
    let read = tokio::task::spawn_blocking(move || {
        let _slot = slot;
        read_files(&root, &request_for_read, &response_for_read, max)
    });
    let verified = tokio::time::timeout(Duration::from_secs(25), read)
        .await
        .map_err(|_| pending())?
        .map_err(|_| pending())??;
    let retention = inspect(&rt, request).await?;
    let now = stamp(state)?;
    let expires = observed_at_ms.checked_add(30_000).ok_or_else(refused)?;
    if now < observed_at_ms || now >= expires {
        return Err(pending());
    }
    let fact = VerifiedExport {
        schema_version: 1,
        request_digest: token.request_digest().clone(),
        export_operation_id: request.operation_id,
        manifest_json: verified.0,
        manifest_digest: verified.1,
        bundle_bytes: verified.2,
        bundle_digest: verified.3,
        producer_installation_id: verified.4,
        export_response: response,
        retention_inspection: retention,
        observed_at_ms,
        expires_at_ms: expires,
    };
    fact.validate_for(token)?;
    Ok(fact)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetentionObservation {
    request_digest: Digest,
    export_operation_id: Uuid,
    inspection: Value,
    observed_at_ms: u64,
    expires_at_ms: u64,
}
impl RetentionObservation {
    pub fn observed_at_ms(&self) -> u64 {
        self.observed_at_ms
    }
    pub fn expires_at_ms(&self) -> u64 {
        self.expires_at_ms
    }
    pub fn validate_for(&self, token: &ReservedExport) -> Result<()> {
        token.request().validate().map_err(|_| refused())?;
        verify_operation(token, &self.request_digest, self.export_operation_id)?;
        if !valid_observation(self.observed_at_ms, self.expires_at_ms) {
            return Err(refused());
        }
        verify_retention(token.request(), &self.inspection)
    }
}
/// Fresh retention observation is separate from the immutable export receipt.
/// It does not re-export bytes or refresh the old receipt's timestamp.
pub async fn inspect_retained(
    state: &AppState,
    token: &ReservedExport,
    fact: &VerifiedExport,
) -> Result<RetentionObservation> {
    let rt = runtime(state)?;
    let _bulk = rt.bulk.try_acquire().map_err(|_| pending())?;
    inspect_retained_inner(state, token, fact).await
}
/// Read-only continuing-execution proof. Cold exports cannot consume this gate.
/// The caller still owns current assignment/auth and post-lock freshness fences.
pub(crate) async fn inspect_retained_for_execution(
    state: &AppState,
    token: &ReservedExport,
    fact: &VerifiedExport,
) -> Result<RetentionObservation> {
    let _slot = (aq_machine_control::limits::EXPORT_INSPECTIONS.enter()).ok_or_else(pending)?;
    tokio::time::timeout(
        Duration::from_secs(2),
        inspect_retained_inner(state, token, fact),
    )
    .await
    .map_err(|_| pending())?
}
async fn inspect_retained_inner(
    state: &AppState,
    token: &ReservedExport,
    fact: &VerifiedExport,
) -> Result<RetentionObservation> {
    fact.validate_for(token)?;
    let rt = runtime(state)?;
    let start = stamp(state)?;
    let inspection = inspect(&rt, token.request()).await?;
    let end = stamp(state)?;
    let expires = start.checked_add(30_000).ok_or_else(refused)?;
    if end < start || end >= expires {
        return Err(refused());
    }
    let observation = RetentionObservation {
        request_digest: token.request_digest().clone(),
        export_operation_id: token.request().operation_id,
        inspection,
        observed_at_ms: start,
        expires_at_ms: expires,
    };
    observation.validate_for(token)?;
    Ok(observation)
}
fn read_files(
    root: &std::path::Path,
    request: &Request,
    response: &Value,
    max_bundle: u64,
) -> Result<(String, Digest, u64, Digest, Uuid)> {
    let root = Installation::open(root).map_err(|_| refused())?;
    let installation = root.id().map_err(|_| refused())?;
    let directory = root.artifact(request.operation_id).map_err(|_| refused())?;
    let bytes = directory.manifest(MAX_MANIFEST).map_err(|_| refused())?;
    let manifest: GitManifest = serde_json::from_slice(&bytes).map_err(|_| refused())?;
    verify_manifest(request, &manifest, response, max_bundle)?;
    if manifest.producer_installation_id != installation.to_string() {
        return Err(refused());
    }
    let (total, hash) = directory.verify_bundle(&manifest).map_err(|_| refused())?;
    let digest = serde_json::from_value(json!(hash)).map_err(|_| refused())?;
    let manifest_digest = Digest::of(&bytes);
    let manifest_json = String::from_utf8(bytes).map_err(|_| refused())?;
    Ok((manifest_json, manifest_digest, total, digest, installation))
}

#[derive(Debug, Clone, Copy)]
pub enum Member {
    Manifest,
    Bundle,
}
#[derive(Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ChunkRange {
    pub(crate) offset: u64,
    limit: u32,
}
impl ChunkRange {
    pub(crate) fn valid_limit(&self) -> bool {
        (1..=1_048_576).contains(&self.limit)
    }
}
/// Bounded I/O; route fences authority/retention and workers verify the whole artifact digest.
pub(crate) async fn read_chunk(
    state: &AppState,
    linked: &LinkedExport,
    member: Member,
    range: ChunkRange,
) -> Result<Vec<u8>> {
    let fact = linked.fact();
    fact.validate_for(linked.token())?;
    if !range.valid_limit() {
        return Err(refused());
    }
    let rt = runtime(state)?;
    let _bulk = rt.bulk.try_acquire().map_err(|_| pending())?;
    let slot = file_slot()?;
    let root = rt.config.git_root.clone();
    let fact = fact.clone();
    let read = tokio::task::spawn_blocking(move || {
        let _slot = slot;
        read_chunk_file(&root, &fact, member, range)
    });
    tokio::time::timeout(Duration::from_secs(5), read)
        .await
        .map_err(|_| pending())?
        .map_err(|_| pending())?
}
fn read_chunk_file(
    root: &std::path::Path,
    fact: &VerifiedExport,
    member: Member,
    range: ChunkRange,
) -> Result<Vec<u8>> {
    if !range.valid_limit() {
        return Err(refused());
    }
    let ChunkRange { offset, limit } = range;
    let root = Installation::open(root).map_err(|_| refused())?;
    let directory = root
        .artifact(fact.export_operation_id)
        .map_err(|_| refused())?;
    // Manifest is small and pins the member length/digest. A replaced bundle
    // with unchanged size is still caught by the worker's whole-artifact hash.
    let manifest = directory.manifest(MAX_MANIFEST).map_err(|_| refused())?;
    if manifest != fact.manifest_bytes() {
        return Err(refused());
    }
    let expected = match member {
        Member::Manifest => manifest.len() as u64,
        Member::Bundle => fact.bundle_bytes,
    };
    let remaining = expected.checked_sub(offset).ok_or_else(refused)?;
    let count = remaining.min(u64::from(limit)) as usize;
    if matches!(member, Member::Manifest) {
        let start = usize::try_from(offset).map_err(|_| refused())?;
        return Ok(manifest[start..start + count].to_vec());
    }
    directory
        .read_bundle(expected, offset, count)
        .map_err(|_| refused())
}

#[cfg(test)]
mod tests;

#[cfg(all(test, feature = "test-support"))]
mod actual_git_test;

pub(crate) mod workspace;
