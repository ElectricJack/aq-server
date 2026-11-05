//! Fixed pre-offer Git export I/O. SQL reservation/finalization are owned by
//! profiled_execution::source_exports; this module cannot mint launch authority.
use super::{AppError, AppState, GitManifest, Result, Runtime, json, runtime};
pub use crate::profiled_execution::source_exports::Request;
use crate::profiled_execution::source_exports::ReservedExport;
use aq_admission::Digest;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    io::Read,
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    sync::Semaphore,
};
use uuid::Uuid;
const MAX_MANIFEST: usize = 1_048_576;
const MAX_RETENTION: usize = 16_384;
pub const MAX_VERIFIED_BYTES: usize = 4_194_304;
static FILE_READS: OnceLock<Arc<Semaphore>> = OnceLock::new();
fn file_slot() -> Result<tokio::sync::OwnedSemaphorePermit> {
    FILE_READS
        .get_or_init(|| Arc::new(Semaphore::new(2)))
        .clone()
        .try_acquire_owned()
        .map_err(|_| pending())
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
        if self.schema_version != 1
            || self.request_digest != *token.request_digest()
            || request.digest().map_err(|_| refused())? != self.request_digest
            || self.export_operation_id != request.operation_id
            || self.manifest_json.len() > MAX_MANIFEST
            || self.manifest_digest != Digest::of(self.manifest_bytes())
            || self.producer_installation_id.is_nil()
            || self.observed_at_ms == 0
            || self.expires_at_ms <= self.observed_at_ms
            || self.expires_at_ms - self.observed_at_ms > 30_000
            || self.expires_at_ms > i64::MAX as u64
            || self.bundle_bytes == 0
            || self
                .bundle_bytes
                .checked_add(self.manifest_json.len() as u64)
                .is_none_or(|n| n > token.reserved_bytes())
        {
            return Err(refused());
        }
        let manifest: GitManifest =
            serde_json::from_str(&self.manifest_json).map_err(|_| refused())?;
        verify_manifest(request, &manifest, &self.export_response, self.bundle_bytes)?;
        if manifest.bundle_size != self.bundle_bytes
            || manifest.bundle_sha256.as_deref() != Some(self.bundle_digest.as_str())
            || manifest.producer_installation_id != self.producer_installation_id.to_string()
        {
            return Err(refused());
        }
        verify_retention(request, &self.retention_inspection)?;
        self.canonical_bytes()?;
        Ok(())
    }
}
fn verify_manifest(
    request: &Request,
    m: &GitManifest,
    response: &Value,
    max_bundle: u64,
) -> Result<()> {
    if m.format_version != 1
        || m.repository_id != request.repository_id.to_string()
        || m.repository_identity != request.logical_identity
        || m.binding_generation != request.binding_generation
        || m.transfer_id != request.operation_id.to_string()
        || m.object_format != "sha1"
        || m.base != request.base
        || m.source != request.base
        || !m.prerequisites.is_empty()
        || m.reference_only
        || m.bundle_size == 0
        || m.bundle_size > max_bundle
        || m.caller_correlation != correlation(request)
        || Uuid::parse_str(&m.producer_installation_id)
            .ok()
            .is_none_or(|id| id.is_nil())
        || response.as_object().is_none_or(|v| v.len() != 3)
        || response["artifact_handle"] != request.operation_id.to_string()
        || response["retention_handle"] != request.operation_id.to_string()
        || response["manifest"] != serde_json::to_value(m).map_err(|_| AppError::Internal)?
        || canonical(response)?.len() > MAX_MANIFEST
    {
        return Err(refused());
    }
    let digest = m.bundle_sha256.as_ref().ok_or_else(refused)?;
    serde_json::from_value::<Digest>(json!(digest)).map_err(|_| refused())?;
    Ok(())
}
fn verify_retention(request: &Request, value: &Value) -> Result<()> {
    if canonical(value)?.len() > MAX_RETENTION
        || value["version"] != 1
        || value["id"] != request.operation_id.to_string()
        || value["repository_id"] != request.repository_id.to_string()
        || value["binding_generation"] != request.binding_generation
        || value["generation"]
            .as_u64()
            .is_none_or(|n| n == 0 || n > i64::MAX as u64)
        || value["state"] != "active"
        || value["resource"] != "transfer"
        || value.get("release_operation") != Some(&Value::Null)
        || value.get("release_reason") != Some(&Value::Null)
        || value["refs"]
            != json!([{"name":format!("refs/aq-git/transfers/{}",request.operation_id),"oid":request.base.hex}])
    {
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
        if response["protocol_version"] != 1
            || response["request_id"] != request["request_id"]
            || response["ok"] != true
            || response.get("error").is_some()
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
        if self.request_digest != *token.request_digest()
            || token.request().digest().map_err(|_| refused())? != self.request_digest
            || self.export_operation_id != token.request().operation_id
            || self.observed_at_ms == 0
            || self.expires_at_ms <= self.observed_at_ms
            || self.expires_at_ms - self.observed_at_ms > 30_000
            || self.expires_at_ms > i64::MAX as u64
        {
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
    static READS: OnceLock<Semaphore> = OnceLock::new();
    let _slot = READS
        .get_or_init(|| Semaphore::new(2))
        .try_acquire()
        .map_err(|_| pending())?;
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
fn open_member(
    parent: &std::fs::File,
    name: &std::ffi::OsStr,
    directory: bool,
) -> Result<std::fs::File> {
    use nix::{
        fcntl::{OFlag, openat},
        sys::stat::Mode,
    };
    use std::os::fd::{AsRawFd, FromRawFd};
    let mut flags = OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC;
    if directory {
        flags |= OFlag::O_DIRECTORY;
    }
    let fd = openat(Some(parent.as_raw_fd()), name, flags, Mode::empty()).map_err(|_| refused())?;
    // openat returned a newly owned descriptor. No other owner is constructed.
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}
fn private_metadata(file: &std::fs::File, directory: bool) -> Result<std::fs::Metadata> {
    use std::os::unix::fs::MetadataExt;
    let meta = file.metadata().map_err(|_| refused())?;
    if meta.uid() != nix::unistd::geteuid().as_raw()
        || meta.mode() & 0o022 != 0
        || (directory && !meta.is_dir())
        || (!directory && !meta.is_file())
    {
        return Err(refused());
    }
    Ok(meta)
}
fn open_root(path: &std::path::Path) -> Result<std::fs::File> {
    use std::path::Component;
    if !path.is_absolute() {
        return Err(refused());
    }
    let mut dir = std::fs::File::open("/").map_err(|_| refused())?;
    for part in path.components() {
        match part {
            Component::RootDir => {}
            Component::Normal(name) => dir = open_member(&dir, name, true)?,
            _ => return Err(refused()),
        }
    }
    private_metadata(&dir, true)?;
    Ok(dir)
}
fn unchanged(before: &std::fs::Metadata, after: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    (
        before.dev(),
        before.ino(),
        before.len(),
        before.mtime(),
        before.mtime_nsec(),
        before.ctime(),
        before.ctime_nsec(),
    ) == (
        after.dev(),
        after.ino(),
        after.len(),
        after.mtime(),
        after.mtime_nsec(),
        after.ctime(),
        after.ctime_nsec(),
    )
}
fn bounded_file(parent: &std::fs::File, name: &str, cap: usize) -> Result<Vec<u8>> {
    let mut file = open_member(parent, std::ffi::OsStr::new(name), false)?;
    let before = private_metadata(&file, false)?;
    if before.len() == 0 || before.len() > cap as u64 {
        return Err(refused());
    }
    let mut bytes = Vec::with_capacity(before.len() as usize);
    file.by_ref()
        .take((cap + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| refused())?;
    if bytes.len() > cap
        || bytes.len() as u64 != before.len()
        || !unchanged(&before, &file.metadata().map_err(|_| refused())?)
    {
        return Err(refused());
    }
    Ok(bytes)
}
fn read_files(
    root: &std::path::Path,
    request: &Request,
    response: &Value,
    max_bundle: u64,
) -> Result<(String, Digest, u64, Digest, Uuid)> {
    use sha2::{Digest as _, Sha256};
    let root = open_root(root)?;
    let format = bounded_file(&root, "format.json", 65_536)?;
    let format: Value = serde_json::from_slice(&format).map_err(|_| refused())?;
    if format["format_version"] != 1 {
        return Err(refused());
    }
    let installation = format["installation_id"]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
        .filter(|id| !id.is_nil())
        .ok_or_else(refused)?;
    let artifacts = open_member(&root, std::ffi::OsStr::new("artifacts"), true)?;
    private_metadata(&artifacts, true)?;
    let directory = open_member(
        &artifacts,
        std::ffi::OsStr::new(&request.operation_id.to_string()),
        true,
    )?;
    private_metadata(&directory, true)?;
    let bytes = bounded_file(&directory, "manifest.json", MAX_MANIFEST)?;
    let manifest: GitManifest = serde_json::from_slice(&bytes).map_err(|_| refused())?;
    verify_manifest(request, &manifest, response, max_bundle)?;
    if manifest.producer_installation_id != installation.to_string() {
        return Err(refused());
    }
    let mut bundle = open_member(&directory, std::ffi::OsStr::new("objects.bundle"), false)?;
    let before = private_metadata(&bundle, false)?;
    if before.len() != manifest.bundle_size {
        return Err(refused());
    }
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 65_536];
    let mut total = 0u64;
    loop {
        let remaining = manifest
            .bundle_size
            .checked_add(1)
            .and_then(|n| n.checked_sub(total))
            .ok_or_else(refused)?;
        let cap = buffer
            .len()
            .min(usize::try_from(remaining).unwrap_or(usize::MAX));
        if cap == 0 {
            return Err(refused());
        }
        let count = bundle.read(&mut buffer[..cap]).map_err(|_| refused())?;
        if count == 0 {
            break;
        }
        total += count as u64;
        if total > manifest.bundle_size {
            return Err(refused());
        }
        hasher.update(&buffer[..count]);
    }
    let hash = hex::encode(hasher.finalize());
    if total != manifest.bundle_size
        || manifest.bundle_sha256.as_deref() != Some(&hash)
        || !unchanged(&before, &bundle.metadata().map_err(|_| refused())?)
    {
        return Err(refused());
    }
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
/// Fixed bounded I/O only. The native route owns credential, attempt and current
/// retention checks before and after this call. Workers verify the assembled
/// artifact digest before hydration; reading a chunk does not admit execution.
pub async fn read_chunk(
    state: &AppState,
    token: &ReservedExport,
    fact: &VerifiedExport,
    member: Member,
    offset: u64,
    limit: u32,
) -> Result<Vec<u8>> {
    fact.validate_for(token)?;
    if limit == 0 || limit > 262_144 {
        return Err(refused());
    }
    let rt = runtime(state)?;
    let _bulk = rt.bulk.try_acquire().map_err(|_| pending())?;
    let slot = file_slot()?;
    let root = rt.config.git_root.clone();
    let fact = fact.clone();
    let read = tokio::task::spawn_blocking(move || {
        let _slot = slot;
        read_chunk_file(&root, &fact, member, offset, limit)
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
    offset: u64,
    limit: u32,
) -> Result<Vec<u8>> {
    use std::io::{Seek, SeekFrom};
    if limit == 0 || limit > 262_144 {
        return Err(refused());
    }
    let root = open_root(root)?;
    let artifacts = open_member(&root, std::ffi::OsStr::new("artifacts"), true)?;
    private_metadata(&artifacts, true)?;
    let directory = open_member(
        &artifacts,
        std::ffi::OsStr::new(&fact.export_operation_id.to_string()),
        true,
    )?;
    private_metadata(&directory, true)?;
    // Manifest is small and pins the member length/digest. A replaced bundle
    // with unchanged size is still caught by the worker's whole-artifact hash.
    let manifest = bounded_file(&directory, "manifest.json", MAX_MANIFEST)?;
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
    let mut file = open_member(&directory, std::ffi::OsStr::new("objects.bundle"), false)?;
    let before = private_metadata(&file, false)?;
    if before.len() != expected {
        return Err(refused());
    }
    file.seek(SeekFrom::Start(offset)).map_err(|_| refused())?;
    let mut bytes = vec![0; count];
    file.read_exact(&mut bytes).map_err(|_| refused())?;
    if !unchanged(&before, &file.metadata().map_err(|_| refused())?) {
        return Err(refused());
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiled_execution::source_exports::synthetic_reserved_export;
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
        path::PathBuf,
    };
    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
        artifact: PathBuf,
        token: ReservedExport,
        response: Value,
        retention: Value,
    }
    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("git");
        let request = Request {
            schema_version: 1,
            operation_id: Uuid::new_v4(),
            project_id: Uuid::new_v4(),
            project_revision: 1,
            repository_id: Uuid::new_v4(),
            logical_identity: "source-fixture".into(),
            binding_generation: 1,
            base: aq_worker_protocol::ObjectId {
                algorithm: aq_worker_protocol::ObjectAlgorithm::Sha1,
                hex: "a".repeat(40),
            },
            source_retention_handle: Uuid::new_v4(),
            source_observation: aq_admission::ArtifactRef {
                id: aq_admission::Text::new("source-observation").unwrap(),
                version: aq_admission::Text::new("1").unwrap(),
                digest: Digest::of(b"observation"),
            },
        };
        let artifact = root
            .join("artifacts")
            .join(request.operation_id.to_string());
        fs::create_dir_all(&artifact).unwrap();
        let producer = Uuid::new_v4();
        fs::write(
            root.join("format.json"),
            json!({"format_version":1,"installation_id":producer}).to_string(),
        )
        .unwrap();
        let bundle = b"synthetic bundle bytes, not a real Git export";
        fs::write(artifact.join("objects.bundle"), bundle).unwrap();
        let manifest = json!({"format_version":1,"repository_id":request.repository_id,"repository_identity":request.logical_identity,"binding_generation":1,"transfer_id":request.operation_id,"object_format":"sha1","base":request.base,"source":request.base,"prerequisites":[],"bundle_size":bundle.len(),"bundle_sha256":Digest::of(bundle),"producer_installation_id":producer,"reference_only":false,"caller_correlation":correlation(&request)});
        fs::write(
            artifact.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        let response = json!({"artifact_handle":request.operation_id,"retention_handle":request.operation_id,"manifest":manifest});
        let retention = json!({"version":1,"id":request.operation_id,"repository_id":request.repository_id,"binding_generation":1,"generation":1,"resource":"transfer","refs":[{"name":format!("refs/aq-git/transfers/{}",request.operation_id),"oid":request.base.hex}],"state":"active","dependencies":[],"release_operation":null,"release_reason":null});
        Fixture {
            _dir: dir,
            root,
            artifact,
            token: synthetic_reserved_export(request, 2_097_152).unwrap(),
            response,
            retention,
        }
    }
    fn fact(f: &Fixture) -> VerifiedExport {
        let v = read_files(&f.root, f.token.request(), &f.response, 4096).unwrap();
        VerifiedExport {
            schema_version: 1,
            request_digest: f.token.request_digest().clone(),
            export_operation_id: f.token.request().operation_id,
            manifest_json: v.0,
            manifest_digest: v.1,
            bundle_bytes: v.2,
            bundle_digest: v.3,
            producer_installation_id: v.4,
            export_response: f.response.clone(),
            retention_inspection: f.retention.clone(),
            observed_at_ms: 1000,
            expires_at_ms: 31000,
        }
    }
    #[test]
    fn historical_fact_is_canonical_bound_and_cannot_refresh_itself() {
        let f = fixture();
        let v = fact(&f);
        let bytes = v.canonical_bytes().unwrap();
        let decoded = VerifiedExport::decode_stored(&f.token, &bytes).unwrap();
        assert_eq!(decoded.observed_at_ms(), 1000);
        assert_eq!(decoded.expires_at_ms(), 31000);
        let mut alternate = bytes.clone();
        alternate.push(b' ');
        assert!(VerifiedExport::decode_stored(&f.token, &alternate).is_err());
        let other = fixture();
        assert!(VerifiedExport::decode_stored(&other.token, &bytes).is_err());
        for field in ["request_digest", "manifest_digest", "bundle_digest"] {
            let mut changed = serde_json::to_value(&v).unwrap();
            changed[field] = json!(Digest::of(b"different"));
            assert!(
                VerifiedExport::decode_stored(&f.token, &canonical(&changed).unwrap()).is_err()
            );
        }
        for expiry in [1000, 31001, u64::MAX] {
            let mut changed = v.clone();
            changed.expires_at_ms = expiry;
            assert!(changed.validate_for(&f.token).is_err());
        }
    }
    #[test]
    fn verification_refuses_corrupt_size_digest_manifest_and_installation() {
        for case in 0..6 {
            let mut f = fixture();
            match case {
                0 => fs::write(f.artifact.join("objects.bundle"), b"wrong").unwrap(),
                1 => { let path = f.artifact.join("objects.bundle"); let mut bytes = fs::read(&path).unwrap(); bytes[0] ^= 1; fs::write(path, bytes).unwrap(); }
                2 => f.response["manifest"]["binding_generation"] = json!(2),
                3 => fs::write(f.root.join("format.json"), json!({"format_version":1,"installation_id":Uuid::new_v4()}).to_string()).unwrap(),
                4 => fs::write(f.root.join("format.json"), json!({"format_version":2,"installation_id":f.response["manifest"]["producer_installation_id"]}).to_string()).unwrap(),
                _ => fs::write(f.artifact.join("manifest.json"), vec![b'x';MAX_MANIFEST+1]).unwrap(),
            }
            assert!(
                read_files(&f.root, f.token.request(), &f.response, 4096).is_err(),
                "case {case}"
            );
        }
        let f = fixture();
        assert!(read_files(&f.root, f.token.request(), &f.response, 1).is_err());
    }
    #[test]
    fn nofollow_applies_to_every_artifact_component_and_special_files_do_not_block() {
        for member in [
            "",
            "artifacts",
            "artifact",
            "manifest.json",
            "objects.bundle",
            "format.json",
        ] {
            let f = fixture();
            let path = match member {
                "" => f.root.clone(),
                "artifacts" => f.root.join(member),
                "artifact" => f.artifact.clone(),
                "format.json" => f.root.join(member),
                _ => f.artifact.join(member),
            };
            let original = path.with_extension("original");
            fs::rename(&path, &original).unwrap();
            symlink(&original, &path).unwrap();
            assert!(
                read_files(&f.root, f.token.request(), &f.response, 4096).is_err(),
                "{member}"
            );
        }
        let f = fixture();
        let bundle = f.artifact.join("objects.bundle");
        fs::remove_file(&bundle).unwrap();
        nix::unistd::mkfifo(&bundle, nix::sys::stat::Mode::S_IRUSR).unwrap();
        assert!(read_files(&f.root, f.token.request(), &f.response, 4096).is_err());
        let f = fixture();
        fs::set_permissions(
            f.artifact.join("objects.bundle"),
            fs::Permissions::from_mode(0o666),
        )
        .unwrap();
        assert!(read_files(&f.root, f.token.request(), &f.response, 4096).is_err());
    }
    #[test]
    fn chunk_reads_are_bounded_exact_and_refuse_changed_members() {
        let f = fixture();
        let v = fact(&f);
        let expected = fs::read(f.artifact.join("objects.bundle")).unwrap();
        let mut actual = Vec::new();
        for offset in (0..expected.len()).step_by(7) {
            actual.extend(read_chunk_file(&f.root, &v, Member::Bundle, offset as u64, 7).unwrap());
        }
        assert_eq!(actual, expected);
        assert!(
            read_chunk_file(&f.root, &v, Member::Bundle, expected.len() as u64, 7)
                .unwrap()
                .is_empty()
        );
        assert!(
            read_chunk_file(&f.root, &v, Member::Bundle, expected.len() as u64 + 1, 7).is_err()
        );
        assert!(read_chunk_file(&f.root, &v, Member::Bundle, u64::MAX, 7).is_err());
        for limit in [0, 262145, u32::MAX] {
            assert!(read_chunk_file(&f.root, &v, Member::Bundle, 0, limit).is_err());
        }
        assert_eq!(
            read_chunk_file(&f.root, &v, Member::Manifest, 0, 262144).unwrap(),
            v.manifest_bytes()
        );
        fs::write(f.artifact.join("manifest.json"), b"{}").unwrap();
        assert!(read_chunk_file(&f.root, &v, Member::Bundle, 0, 7).is_err());
        let f = fixture();
        let v = fact(&f);
        let path = f.artifact.join("objects.bundle");
        let saved = path.with_extension("saved");
        fs::rename(&path, &saved).unwrap();
        symlink(&saved, &path).unwrap();
        assert!(read_chunk_file(&f.root, &v, Member::Bundle, 0, 7).is_err());
        fs::remove_file(&path).unwrap();
        fs::write(&path, b"short").unwrap();
        assert!(read_chunk_file(&f.root, &v, Member::Bundle, 0, 7).is_err());
    }
    #[test]
    fn retention_requires_exact_active_transfer_and_finite_observation() {
        let f = fixture();
        let observation = RetentionObservation {
            request_digest: f.token.request_digest().clone(),
            export_operation_id: f.token.request().operation_id,
            inspection: f.retention.clone(),
            observed_at_ms: 1000,
            expires_at_ms: 31000,
        };
        observation.validate_for(&f.token).unwrap();
        for (field, value) in [
            ("state", json!("releasing")),
            ("release_operation", json!(Uuid::new_v4())),
            ("resource", json!("workspace")),
            ("refs", json!([])),
            ("generation", json!(0)),
            ("repository_id", json!(Uuid::new_v4())),
            ("binding_generation", json!(2)),
        ] {
            let mut changed = observation.clone();
            changed.inspection[field] = value;
            assert!(changed.validate_for(&f.token).is_err(), "{field}");
        }
        let mut changed = observation.clone();
        changed.expires_at_ms = 31001;
        assert!(changed.validate_for(&f.token).is_err());
        assert!(observation.validate_for(&fixture().token).is_err());
    }
    fn runtime_for(f: &Fixture) -> Runtime {
        Runtime::new(super::super::SourceRetentionConfig {
            keep_per_task: 2,
            max_age: 30 * 86400,
            staging_root: f._dir.path().join("staging"),
            git_root: f.root.clone(),
            git_socket: f._dir.path().join("git.sock"),
            max_staging_bytes: 4_194_304,
            max_bundle_bytes: 4096,
            bulk_concurrency: 2,
            incomplete_ttl_secs: 60,
        })
        .unwrap()
    }
    #[tokio::test]
    async fn socket_replies_are_bounded_and_correlated_without_inventing_success() {
        for case in 0..5 {
            let f = fixture();
            let rt = runtime_for(&f);
            let listener = tokio::net::UnixListener::bind(&rt.config.git_socket).unwrap();
            let task = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                let r: Value = serde_json::from_str(&line).unwrap();
                let mut reply = json!({"protocol_version":1,"request_id":r["request_id"],"operation_id":r["operation_id"],"ok":true,"result":{"done":true}});
                match case {
                    1 => reply["request_id"] = json!("different"),
                    2 => reply["operation_id"] = json!(Uuid::new_v4()),
                    3 => reply["result"] = json!("x".repeat(1024)),
                    4 => reply["ok"] = json!(false),
                    _ => {}
                }
                let mut bytes = serde_json::to_vec(&reply).unwrap();
                bytes.push(b'\n');
                reader.get_mut().write_all(&bytes).await.unwrap();
            });
            let result = exchange(
                &rt,
                &json!({"request_id":"request","operation_id":Uuid::new_v4()}),
                512,
                Duration::from_secs(1),
            )
            .await;
            assert_eq!(result.is_ok(), case == 0, "case {case}");
            task.await.unwrap();
        }
    }
}

#[cfg(all(test, feature = "test-support"))]
mod actual_git_test {
    use super::*;
    use crate::{config::Config, identity::verifier_from_config, test_support::TestDb};
    async fn git(path: &std::path::Path, args: &[&str]) -> String {
        let out = tokio::process::Command::new(std::env::var("AQ_TEST_GIT_EXECUTABLE").unwrap())
            .current_dir(path)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.test")
            .env("GIT_COMMITTER_NAME", "Fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.test")
            .output()
            .await
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().into()
    }
    async fn call(rt: &Runtime, repo: Uuid, operation: &str, params: Value) -> Value {
        let op = Uuid::new_v4();
        exchange(rt,&json!({"protocol_version":1,"request_id":op.to_string(),"operation":operation,"repository_id":repo,"operation_id":op,"expected_binding_generation":if operation=="repo.register" {0}else{1},"params":params}),MAX_MANIFEST,Duration::from_secs(30)).await.unwrap_or_else(|error| panic!("fixture {operation}: {error:?}"))
    }
    #[tokio::test]
    #[ignore = "requires selected immutable aq-git and Git executables; disposable PostgreSQL"]
    async fn real_export_exact_retry_and_fresh_inspection_preserve_original_fact() {
        let dir = tempfile::tempdir().unwrap();
        let seed = dir.path().join("seed");
        std::fs::create_dir(&seed).unwrap();
        git(&seed, &["init", "-b", "main"]).await;
        std::fs::write(seed.join("source"), b"retained pre-offer source\n").unwrap();
        git(&seed, &["add", "."]).await;
        git(&seed, &["commit", "-m", "source"]).await;
        let base = aq_worker_protocol::ObjectId {
            algorithm: aq_worker_protocol::ObjectAlgorithm::Sha1,
            hex: git(&seed, &["rev-parse", "HEAD"]).await,
        };
        let settings = super::super::SourceRetentionConfig {
            keep_per_task: 2,
            max_age: 30 * 86400,
            staging_root: dir.path().join("staging"),
            git_root: dir.path().join("git"),
            git_socket: dir.path().join("git.sock"),
            max_staging_bytes: 4_194_304,
            max_bundle_bytes: 1_048_576,
            bulk_concurrency: 2,
            incomplete_ttl_secs: 60,
        };
        let runtime = Arc::new(Runtime::new(settings).unwrap());
        let mut child =
            tokio::process::Command::new(std::env::var("AQ_TEST_GIT_CANDIDATE").unwrap())
                .arg("--state-dir")
                .arg(&runtime.config.git_root)
                .arg("--git-executable")
                .arg(std::env::var("AQ_TEST_GIT_EXECUTABLE").unwrap())
                .arg("--listen")
                .arg(&runtime.config.git_socket)
                .arg("serve")
                .kill_on_drop(true)
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap();
        for _ in 0..500 {
            if runtime.config.git_socket.exists() {
                break;
            }
            assert!(child.try_wait().unwrap().is_none());
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let repo = Uuid::new_v4();
        call(&runtime,repo,"repo.register",json!({"identity":"real-preoffer","mode":"central","remote_url":seed,"allow_file_transport":true,"allowed_refs":["refs/heads/main"],"allowed_targets":["refs/heads/main"]})).await;
        call(&runtime, repo, "repo.sync", json!({})).await;
        let workspace = call(&runtime, repo, "workspace.create", json!({"base":base})).await;
        let checkpoint=call(&runtime,repo,"workspace.checkpoint",json!({"workspace_id":workspace["workspace_id"],"expected_generation":1,"expected_head":base,"quiescent":true,"mode":"head"})).await;
        let request = Request {
            schema_version: 1,
            operation_id: Uuid::new_v4(),
            project_id: Uuid::new_v4(),
            project_revision: 1,
            repository_id: repo,
            logical_identity: "real-preoffer".into(),
            binding_generation: 1,
            base,
            source_retention_handle: serde_json::from_value(checkpoint["retention_handle"].clone())
                .unwrap(),
            source_observation: aq_admission::ArtifactRef {
                id: aq_admission::Text::new("synthetic-owner-observation").unwrap(),
                version: aq_admission::Text::new("1").unwrap(),
                digest: Digest::of(b"synthetic authorization fixture"),
            },
        };
        let token = crate::profiled_execution::source_exports::synthetic_reserved_export(
            request, 2_097_152,
        )
        .unwrap();
        let db = TestDb::create().await;
        let config:Config=toml::from_str("format_version=1\nenvironment='development'\n[server]\nadmin_bind='127.0.0.1:0'\nworker_bind='127.0.0.1:0'\npublic_origin='http://127.0.0.1:8080'\n[auth]\nmode='dev'\ndev_email='owner@localhost'\n[bootstrap]\nowner_email='owner@localhost'\nenabled=true\n").unwrap();
        let state = AppState {
            agent_access: None,
            source_retention: Some(runtime.clone()),
            identity: verifier_from_config(&config.auth, reqwest::Client::new()),
            config: Arc::new(config),
            store: db.store().await,
            clock: crate::system_clock(),
        };
        let first = verify_export(&state, &token).await.unwrap();
        let original = first.canonical_bytes().unwrap();
        let second = verify_export(&state, &token).await.unwrap();
        assert_eq!(first.manifest_bytes(), second.manifest_bytes());
        assert_eq!(first.bundle_digest(), second.bundle_digest());
        let current = inspect_retained(&state, &token, &first).await.unwrap();
        current.validate_for(&token).unwrap();
        assert!(current.observed_at_ms() >= first.observed_at_ms());
        assert_eq!(first.canonical_bytes().unwrap(), original);
        VerifiedExport::decode_stored(&token, &original).unwrap();
        let bundle = runtime
            .config
            .git_root
            .join("artifacts")
            .join(token.request().operation_id.to_string())
            .join("objects.bundle");
        let output = git(&seed, &["bundle", "list-heads", bundle.to_str().unwrap()]).await;
        assert!(output.contains(&token.request().base.hex));
        assert!(output.contains(&format!(
            "refs/aq-git/transfers/{}",
            token.request().operation_id
        )));
        std::fs::write(bundle, b"corrupt after verified export").unwrap();
        assert!(verify_export(&state, &token).await.is_err());
        assert_eq!(first.canonical_bytes().unwrap(), original);
        child.kill().await.unwrap();
        child.wait().await.unwrap();
        // The synthetic token deliberately does not claim durable SQL ownership,
        // task admission, attempt creation, worker hydration, or provider launch.
    }
}
