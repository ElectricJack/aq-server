//! Fixture-only owner assessment of a live finite local Responses peer.
//! Reads real local facts; never rewrites liveness, inventory, selection or grants.
#![allow(dead_code)] // Composed fixture includes this helper separately.
use super::{
    codex_runtime_recipe::{self as recipe, Prepared, ProgramPin},
    native_runner_transport::PrivateState,
};
use anyhow::{Context, Result, ensure};
use aq_admission::{
    Digest, preparation_refresh_v1 as phase,
    scoped_presence::{InventoryReceipt, InventoryV2},
    transport::{TransportDocument, WorkerIdentity},
};
use aq_selection::{
    AvailabilityObservation, CapabilitySnapshot, ExecutionTarget, ObservationScope,
};
use aq_server::{AppState, profiled_execution::registration::NativeRegistration};
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use sqlx::Row;
use std::{
    collections::BTreeSet,
    fs::OpenOptions,
    io::Read,
    net::TcpListener,
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};
use tokio::task::JoinHandle;
use tower::ServiceExt;
use uuid::Uuid;

pub struct LocalImage {
    pub guest: ProgramPin,
    pub outer: PathBuf,
}
pub struct LocalEvidence {
    /// Exactly the five actual images from the retained runtime/environment definitions.
    pub images: Vec<LocalImage>,
    pub synthetic_key_file: PathBuf,
    pub synthetic_key_digest: Digest,
    pub synthetic_key_bytes: u64,
}
pub struct PeerWitness<'a, T> {
    pub listener: &'a TcpListener,
    pub server: &'a JoinHandle<T>,
    pub origin: &'a str,
}
pub enum Stage<'a> {
    BeforePoll,
    BeforePrepare(&'a phase::PrepareRequest),
}
pub struct Assessor {
    sequence: u64,
    last: Option<(Uuid, Value)>,
    subject: Option<Value>,
    images: Vec<SealedImage>,
}

#[derive(Debug, PartialEq, Eq)]
struct ImageStamp {
    dev: u64,
    inode: u64,
    bytes: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    links: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}
impl From<std::fs::Metadata> for ImageStamp {
    fn from(m: std::fs::Metadata) -> Self {
        Self {
            dev: m.dev(),
            inode: m.ino(),
            bytes: m.len(),
            mode: m.mode(),
            uid: m.uid(),
            gid: m.gid(),
            links: m.nlink(),
            modified: (m.mtime(), m.mtime_nsec()),
            changed: (m.ctime(), m.ctime_nsec()),
        }
    }
}
// Continuity of an already hashed immutable fixture image, not a new hash or
// protection against a hostile same-UID writer. The unchanged worker factory
// still verifies its own actual NamedReadonlyEnvironment/mount/image premises.
struct SealedImage {
    route: PathBuf,
    pin: Value,
    file: std::fs::File,
    stamp: ImageStamp,
}
impl SealedImage {
    fn seal(image: &LocalImage) -> Result<Self> {
        let (file, stamp) = rehash(
            &image.outer,
            image.guest.bytes,
            &image.guest.artifact.digest,
            false,
        )?;
        Ok(Self {
            route: image.outer.clone(),
            pin: serde_json::to_value(&image.guest)?,
            file,
            stamp,
        })
    }
    fn current(&self, image: &LocalImage) -> Result<()> {
        ensure!(
            self.route == image.outer && self.pin == serde_json::to_value(&image.guest)?,
            "sealed image route/pin changed"
        );
        let route = file(&self.route, image.guest.bytes, false)?;
        ensure!(
            ImageStamp::from(self.file.metadata()?) == self.stamp
                && ImageStamp::from(route.metadata()?) == self.stamp,
            "sealed image descriptor/route changed"
        );
        Ok(())
    }
}

#[derive(Debug, Default, serde::Serialize)]
struct TemporalClocks {
    started_at_ms: Option<u64>,
    checked_at_ms: Option<u64>,
    server_at_ms: Option<u64>,
    real_at_ms: Option<u64>,
    heartbeat_at_ms: Option<u64>,
    inventory_observed_at_ms: Option<u64>,
    inventory_expires_at_ms: Option<u64>,
    binding_observed_at_ms: Option<u64>,
    binding_expires_at_ms: Option<u64>,
    receipt_recorded_at_ms: Option<u64>,
    receipt_expires_at_ms: Option<u64>,
    preparation_expires_at_ms: Option<u64>,
    observation_received_at_ms: Option<u64>,
    observation_expires_at_ms: Option<u64>,
}
#[derive(Debug, Clone, Copy)]
enum TemporalKind {
    Clock,
    Heartbeat,
    AssessmentAge,
    Inventory,
    InventoryReceipt,
    Preparation,
    Observation,
}
impl TemporalKind {
    fn code(self) -> &'static str {
        match self {
            Self::Clock => "freshness_clock",
            Self::Heartbeat => "freshness_heartbeat",
            Self::AssessmentAge => "freshness_assessment_age",
            Self::Inventory => "freshness_inventory",
            Self::InventoryReceipt => "freshness_inventory_receipt",
            Self::Preparation => "freshness_preparation",
            Self::Observation => "freshness_observation",
        }
    }
}
#[derive(Debug)]
struct TemporalRefusal {
    kind: TemporalKind,
    clocks: TemporalClocks,
}
impl std::fmt::Display for TemporalRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.kind.code())
    }
}
impl std::error::Error for TemporalRefusal {}
fn temporal(valid: bool, kind: TemporalKind, clocks: TemporalClocks) -> Result<()> {
    if valid {
        Ok(())
    } else {
        Err(TemporalRefusal { kind, clocks }.into())
    }
}
/// Numeric fields from a typed clock refusal only; never arbitrary error text.
pub fn refusal_clocks(error: &anyhow::Error) -> Value {
    error
        .downcast_ref::<TemporalRefusal>()
        .map_or(Value::Null, |failure| json!(failure.clocks))
}

/// Fixed fixture diagnostics only. Never return paths, material, request bytes
/// or arbitrary error-chain text to the callback response/log.
pub fn refusal_code(error: &anyhow::Error) -> &'static str {
    if let Some(failure) = error.downcast_ref::<TemporalRefusal>() {
        return failure.kind.code();
    }
    for cause in error.chain() {
        let code = match cause.to_string().as_str() {
            "actual image revalidation unavailable"
            | "actual file changed or pin mismatch"
            | "all five actual images required"
            | "exact unique retained guest image"
            | "public definition bytes" => "image_pin",
            "sealed image route/pin changed"
            | "sealed image descriptor/route changed"
            | "sealed image continuity unavailable" => "image_continuity",
            "synthetic material revalidation unavailable"
            | "owned private synthetic material"
            | "single finite-peer material"
            | "actual scoped synthetic material pin"
            | "actual installed finite-peer material bytes" => "material",
            "actual journal observation unavailable"
            | "actual journal format/digest"
            | "actual journal node identity"
            | "actual retained native config bytes"
            | "actual retained Prepared request before execution"
            | "retained actual source/artifact inputs"
            | "first actual Poll only" => "journal_inputs",
            "actual exact native registration"
            | "current configured native qualification"
            | "registration semantic digest" => "native_registration",
            "current real global provisionability inventory, not installed presence"
            | "exact actual inventory receipt/head/snapshot" => "inventory",
            "finite local server exited"
            | "exact numeric finite peer origin"
            | "actual finite peer listening witness" => "finite_peer",
            "unchanged original finite-peer qualification subject" => "original_subject",
            "assessment requires actual wall clock"
            | "actual current heartbeat required"
            | "actual awaited heartbeat absent"
            | "local assessment exceeded unchanged freshness age"
            | "new actual owner observation timing/scope" => "freshness",
            "protected owner assessment route refused"
            | "actual scoped provisionable material config"
            | "actual node revoked"
            | "actual fixture pool/owner operation join"
            | "prior observation operation reply changed"
            | "only the two actual fixture phases may assess" => "owner_scope",
            _ => continue,
        };
        return code;
    }
    "assessment_unavailable"
}

fn canonical(value: &Value) -> Result<Vec<u8>> {
    let mut value = value.clone();
    value.sort_all_objects();
    Ok(serde_json::to_vec(&value)?)
}
fn current_ms(state: &AppState) -> Result<u64> {
    let now = u64::try_from((state.clock)().timestamp_millis())?;
    let real = u64::try_from(chrono::Utc::now().timestamp_millis())?;
    temporal(
        now.abs_diff(real) <= 1000,
        TemporalKind::Clock,
        TemporalClocks {
            server_at_ms: Some(now),
            real_at_ms: Some(real),
            ..Default::default()
        },
    )?;
    Ok(now)
}
fn file(path: &Path, bytes: u64, private: bool) -> Result<std::fs::File> {
    ensure!(
        path.is_absolute() && bytes > 0 && bytes <= 512 * 1024 * 1024,
        "bounded actual file"
    );
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC | nix::libc::O_NONBLOCK)
        .open(path)?;
    let m = file.metadata()?;
    ensure!(
        m.is_file() && m.len() == bytes,
        "actual regular file length"
    );
    if private {
        ensure!(
            bytes <= 65536
                && m.mode() & 0o7777 == 0o600
                && m.nlink() == 1
                && m.uid() == unsafe { nix::libc::geteuid() },
            "owned private synthetic material"
        );
    }
    Ok(file)
}
fn rehash(
    path: &Path,
    bytes: u64,
    digest: &Digest,
    private: bool,
) -> Result<(std::fs::File, ImageStamp)> {
    let mut file = file(path, bytes, private)?;
    let before = ImageStamp::from(file.metadata()?);
    let mut hash = Sha256::new();
    let mut chunk = zeroize::Zeroizing::new([0u8; 65536]);
    let mut count = 0u64;
    loop {
        let n = file.read(&mut *chunk)?;
        if n == 0 {
            break;
        }
        count += n as u64;
        ensure!(count <= bytes, "actual file grew");
        hash.update(&chunk[..n]);
    }
    let after = ImageStamp::from(file.metadata()?);
    ensure!(
        count == bytes && format!("{:x}", hash.finalize()) == digest.as_str() && before == after,
        "actual file changed or pin mismatch"
    );
    Ok((file, after))
}
fn peer<T>(witness: &PeerWitness<'_, T>) -> Result<()> {
    ensure!(!witness.server.is_finished(), "finite local server exited");
    let addr = witness.listener.local_addr()?;
    ensure!(
        addr.ip() == std::net::Ipv4Addr::LOCALHOST
            && addr.port() > 0
            && witness.origin == format!("http://{addr}/"),
        "exact numeric finite peer origin"
    );
    let mut listening = 0i32;
    let mut size = std::mem::size_of_val(&listening) as nix::libc::socklen_t;
    ensure!(
        unsafe {
            nix::libc::getsockopt(
                witness.listener.as_raw_fd(),
                nix::libc::SOL_SOCKET,
                nix::libc::SO_ACCEPTCONN,
                (&mut listening as *mut i32).cast(),
                &mut size,
            )
        } == 0
            && listening == 1
            && size as usize == std::mem::size_of_val(&listening),
        "actual finite peer listening witness"
    );
    Ok(())
}
fn image_routes(prepared: &Prepared, evidence: &LocalEvidence) -> Result<()> {
    let body = |pin: &aq_admission::ArtifactRef| -> Result<Value> {
        let definition = prepared
            .profiled
            .definition_bodies
            .iter()
            .find(|d| d.artifact() == pin)
            .context("retained public image definition")?;
        ensure!(
            Digest::of(definition.body().as_bytes()) == pin.digest,
            "public definition bytes"
        );
        Ok(serde_json::from_str(definition.body())?)
    };
    let runtime = body(&prepared.runtime)?;
    let environment = body(&prepared.environment)?;
    let expected = [
        &runtime["worker"],
        &environment["images"]["codex"],
        &environment["images"]["host"],
        &environment["images"]["tmux"],
        &environment["images"]["client"],
    ];
    ensure!(
        evidence.images.len() == 5,
        "all five actual images required"
    );
    let mut seen = BTreeSet::new();
    for image in &evidence.images {
        let guest = serde_json::to_value(&image.guest)?;
        ensure!(
            expected.contains(&&guest) && seen.insert(image.guest.path.clone()),
            "exact unique retained guest image"
        );
    }
    Ok(())
}
async fn human(state: &AppState, path: &str, body: Option<Value>) -> Result<Value> {
    let mut request = Request::builder()
        .method(if body.is_some() { "POST" } else { "GET" })
        .uri(path)
        .header("x-aq-dev-email", "owner@localhost");
    if body.is_some() {
        request = request
            .header("content-type", "application/json")
            .header("x-aq-request", "1")
            .header("origin", &state.config.server.public_origin);
    }
    let response = aq_server::build_admin_router(state.clone())
        .oneshot(
            request.body(Body::from(
                body.map(|v| serde_json::to_vec(&v))
                    .transpose()?
                    .unwrap_or_default(),
            ))?,
        )
        .await?;
    ensure!(
        response.status() == StatusCode::OK,
        "protected owner assessment route refused"
    );
    Ok(serde_json::from_slice(
        &to_bytes(response.into_body(), 1048576).await?,
    )?)
}
fn journal(private: &PrivateState, node: Uuid) -> Result<(Value, WorkerIdentity, Digest)> {
    let bytes = private.read(Path::new("journal/journal.json"), 4194304)?;
    let snapshot: Value = serde_json::from_slice(&bytes)?;
    ensure!(
        snapshot["state"]["format"] == 12
            && snapshot["digest"] == json!(Digest::of(&canonical(&snapshot["state"])?)),
        "actual journal format/digest"
    );
    let identity: WorkerIdentity = serde_json::from_value(snapshot["state"]["identity"].clone())?;
    identity.validate()?;
    ensure!(identity.node_id == node, "actual journal node identity");
    Ok((snapshot["state"].clone(), identity, Digest::of(&bytes)))
}
fn prepared_inputs(
    private: &PrivateState,
    state: &Value,
    identity: &WorkerIdentity,
    request: &phase::PrepareRequest,
    prepared: &Prepared,
    evidence: &LocalEvidence,
    now: u64,
) -> Result<()> {
    let attempt = &state["attempts"][request.original.binding.attempt_id.to_string()];
    let accepted: phase::AcceptedDelivery =
        serde_json::from_value(attempt["refresh_preparation"]["delivery"].clone())?;
    let accept: phase::AcceptRequest =
        serde_json::from_value(attempt["refresh_acceptance"]["request"].clone())?;
    let proposal: phase::Proposal =
        serde_json::from_value(attempt["refresh_acceptance"]["proposal"].clone())?;
    request.validate_for(&accepted.receipt, &accept, &proposal)?;
    temporal(
        now < accepted.receipt.lease.expires_at_ms,
        TemporalKind::Preparation,
        TemporalClocks {
            checked_at_ms: Some(now),
            preparation_expires_at_ms: Some(accepted.receipt.lease.expires_at_ms),
            ..Default::default()
        },
    )?;
    ensure!(
        request.original.identity == *identity
            && attempt["refresh_prepared"]["request"] == json!(request)
            && attempt["refresh_prepared"]["request_digest"]
                == json!(Digest::of(&canonical(&json!(request))?))
            && state["revision"].as_u64().context("journal revision")?
                > request.original.journal_revision
            && attempt["refresh_execution"].is_null()
            && attempt["native_host"].is_null(),
        "actual retained Prepared request before execution"
    );
    ensure!(
        attempt["source"]["verified"].is_object()
            && attempt["downloads"]
                .as_object()
                .context("actual downloads")?
                .values()
                .all(|d| d["receipt"].is_object()),
        "retained actual source/artifact inputs"
    );
    ensure!(
        request.original.materials.len() == 1 && accepted.receipt.lease.materials.len() == 1,
        "single finite-peer material"
    );
    let material = &request.original.materials[0];
    let lease = &accepted.receipt.lease.materials[0];
    material.validate_against(lease, material.received_at_ms)?;
    ensure!(
        material.material_digest == evidence.synthetic_key_digest
            && lease.material_bytes == evidence.synthetic_key_bytes
            && lease.credential_profile == prepared.credential,
        "actual scoped synthetic material pin"
    );
    let bytes = zeroize::Zeroizing::new(
        private.read(
            &PathBuf::from("materials")
                .join(material.lease_id.to_string())
                .join("material.bin"),
            65536,
        )?,
    );
    ensure!(
        bytes.len() as u64 == lease.material_bytes
            && Digest::of(&bytes) == material.material_digest,
        "actual installed finite-peer material bytes"
    );
    Ok(())
}
fn inventory_clocks(
    started: u64,
    checked: u64,
    heartbeat: u64,
    snapshot: &CapabilitySnapshot,
    receipt: &InventoryReceipt,
) -> TemporalClocks {
    TemporalClocks {
        started_at_ms: Some(started),
        checked_at_ms: Some(checked),
        heartbeat_at_ms: Some(heartbeat),
        inventory_observed_at_ms: Some(snapshot.observed_at_ms),
        inventory_expires_at_ms: Some(snapshot.expires_at_ms),
        binding_observed_at_ms: snapshot
            .credential_bindings
            .first()
            .map(|b| b.observed_at_ms),
        binding_expires_at_ms: snapshot
            .credential_bindings
            .first()
            .map(|b| b.expires_at_ms),
        receipt_recorded_at_ms: Some(receipt.recorded_at_ms),
        receipt_expires_at_ms: Some(receipt.expires_at_ms),
        ..Default::default()
    }
}
async fn validate_registration(
    state: &AppState,
    pool: &sqlx::PgPool,
    node: Uuid,
    prepared: &Prepared,
    identity: &WorkerIdentity,
) -> Result<()> {
    let execution: Value = sqlx::query_scalar("SELECT state FROM execution_nodes WHERE node_id=$1")
        .bind(node)
        .fetch_one(pool)
        .await?;
    let registration: NativeRegistration = serde_json::from_value(execution["native"].clone())?;
    TransportDocument::Register(registration.request.clone()).validate()?;
    ensure!(
        execution["identity"] == json!(identity)
            && &registration.request.identity == identity
            && registration.request.runtime == prepared.runtime
            && registration.qualification == prepared.qualification
            && registration.semantic_digest
                == aq_server::profiled_execution::registration::semantic_digest(
                    &registration.request
                )
                .map_err(|_| anyhow::anyhow!("registration semantic digest"))?,
        "actual exact native registration"
    );
    let config = state
        .config
        .profiled_execution
        .as_ref()
        .context("actual runtime config")?;
    ensure!(
        config
            .qualified(&registration.request)
            .map_err(anyhow::Error::msg)?
            .qualification
            == prepared.qualification,
        "current configured native qualification"
    );
    Ok(())
}

impl Assessor {
    /// Call BEFORE starting the worker, heartbeat, inventory or admission clock.
    /// Only actual immutable local image files are retained; no mutable owner
    /// fact, material, journal, clock or availability observation is cached.
    pub fn seal_images(prepared: &Prepared, evidence: &LocalEvidence) -> Result<Self> {
        image_routes(prepared, evidence)?;
        let images = evidence
            .images
            .iter()
            .map(SealedImage::seal)
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            sequence: 0,
            last: None,
            subject: None,
            images,
        })
    }
    fn image_continuity(&self, prepared: &Prepared, evidence: &LocalEvidence) -> Result<()> {
        image_routes(prepared, evidence)?;
        ensure!(self.images.len() == 5, "all five actual images required");
        for (sealed, current) in self.images.iter().zip(&evidence.images) {
            sealed.current(current)?;
        }
        Ok(())
    }
    /// The caller serializes these two fixture callbacks. No database locks are
    /// held while reading local images or executing the protected owner routes.
    #[allow(clippy::too_many_arguments)] // Explicit borrowed fixture owners, no authority reconstruction.
    pub async fn assess<T>(
        &mut self,
        state: &AppState,
        pool: &sqlx::PgPool,
        node: Uuid,
        prepared: &Prepared,
        private: &PrivateState,
        evidence: &LocalEvidence,
        witness: PeerWitness<'_, T>,
        stage: Stage<'_>,
    ) -> Result<Value> {
        ensure!(
            matches!(
                (&stage, self.sequence),
                (Stage::BeforePoll, 0) | (Stage::BeforePrepare(_), 1)
            ),
            "only the two actual fixture phases may assess"
        );
        self.image_continuity(prepared, evidence)
            .context("sealed image continuity unavailable")?;
        let started = current_ms(state)?;
        peer(&witness)?;
        drop(
            rehash(
                &evidence.synthetic_key_file,
                evidence.synthetic_key_bytes,
                &evidence.synthetic_key_digest,
                true,
            )
            .context("synthetic material revalidation unavailable")?,
        );
        for (path, expected) in prepared.files()? {
            ensure!(
                private.read(&PathBuf::from("config").join(path), 2097152)? == expected.as_bytes(),
                "actual retained native config bytes"
            );
        }
        let (journal, identity, journal_digest) =
            journal(private, node).context("actual journal observation unavailable")?;
        match stage {
            Stage::BeforePoll => ensure!(
                journal["attempts"]
                    .as_object()
                    .context("journal attempts")?
                    .is_empty(),
                "first actual Poll only"
            ),
            Stage::BeforePrepare(request) => prepared_inputs(
                private,
                &journal,
                &identity,
                request,
                prepared,
                evidence,
                current_ms(state)?,
            )?,
        }
        let (last_seen, revoked): (Option<chrono::DateTime<chrono::Utc>>, bool) =
            sqlx::query_as("SELECT last_seen_at,revoked_at IS NOT NULL FROM nodes WHERE id=$1")
                .bind(node)
                .fetch_one(pool)
                .await?;
        let last = u64::try_from(
            last_seen
                .context("actual awaited heartbeat absent")?
                .timestamp_millis(),
        )?;
        let now = current_ms(state)?;
        ensure!(!revoked, "actual node revoked");
        temporal(
            now >= last && now - last <= 30000,
            TemporalKind::Heartbeat,
            TemporalClocks {
                started_at_ms: Some(started),
                checked_at_ms: Some(now),
                heartbeat_at_ms: Some(last),
                ..Default::default()
            },
        )?;
        validate_registration(state, pool, node, prepared, &identity).await?;
        let registry = state
            .config
            .intelligence
            .as_ref()
            .context("actual intelligence config")?;
        let configured = registry
            .config
            .credentials
            .iter()
            .find(|c| c.credential.profile == prepared.credential)
            .context("exact finite-peer credential")?;
        // The subject of this assessment: the exact runtime, harness, model
        // and credential the recipe prepared. The renderer is the server's
        // one constant rule (spec 0014: no context measurement).
        let rule = aq_server::profiled_execution::context_budget::Renderer::rule()?;
        let subject = json!({
            "runtime":prepared.runtime,"runtime_qualification":prepared.qualification,
            "adapter":prepared.adapter,"environment":prepared.environment,
            "harness":prepared.harness.harness,"cli_version":prepared.harness.cli_version,
            "harness_qualification":prepared.harness.qualification,
            "credential_profile":configured.credential.profile,
            "credential_revision":configured.credential.revision,
            "provider":configured.credential.provider,
            "account_reference":configured.credential.account_reference,
            "endpoint_reference":configured.credential.endpoint_reference,
            "model":recipe::MODEL,"reasoning":recipe::REASONING,
        });
        ensure!(
            rule.definition == prepared.renderer
                && self.subject.as_ref().is_none_or(|prior| prior == &subject),
            "unchanged original finite-peer qualification subject"
        );
        ensure!(
            configured.allowed_node_ids.contains(&node)
                && configured.secret_file.as_ref() == Some(&evidence.synthetic_key_file)
                && configured.credential.enabled
                && configured.credential.provider == "openai"
                && configured.credential.account_reference == "finite-responses-account"
                && configured.credential.endpoint_reference == "finite-private-loopback-responses"
                && registry
                    .credentials
                    .iter()
                    .any(|c| c.profile == prepared.credential && c.provisionable),
            "actual scoped provisionable material config"
        );
        let snapshot: CapabilitySnapshot = serde_json::from_value(
            sqlx::query_scalar::<_, Value>(
                "SELECT snapshot FROM intelligence_node_snapshots WHERE node_id=$1 AND boot_id=$2",
            )
            .bind(node)
            .bind(identity.boot_id)
            .fetch_one(pool)
            .await?,
        )?;
        ensure!(
            snapshot.target
                == ExecutionTarget::Worker {
                    node_id: node,
                    boot_id: identity.boot_id
                }
                && json!(snapshot.harnesses) == json!([prepared.harness])
                && snapshot.credential_bindings.len() == 1
                && snapshot.credential_bindings[0].credential_profile == prepared.credential
                && snapshot.credential_bindings[0].credential_revision
                    == configured.credential.revision
                && !snapshot.credential_bindings[0].available
                && snapshot.credential_bindings[0].preparation.is_some(),
            "current real global provisionability inventory, not installed presence"
        );
        temporal(
            now >= snapshot.observed_at_ms
                && now < snapshot.expires_at_ms
                && now >= snapshot.credential_bindings[0].observed_at_ms
                && now < snapshot.credential_bindings[0].expires_at_ms,
            TemporalKind::Inventory,
            TemporalClocks {
                started_at_ms: Some(started),
                checked_at_ms: Some(now),
                inventory_observed_at_ms: Some(snapshot.observed_at_ms),
                inventory_expires_at_ms: Some(snapshot.expires_at_ms),
                binding_observed_at_ms: Some(snapshot.credential_bindings[0].observed_at_ms),
                binding_expires_at_ms: Some(snapshot.credential_bindings[0].expires_at_ms),
                ..Default::default()
            },
        )?;
        let row = sqlx::query("SELECT o.request_bytes,o.receipt_bytes,h.snapshot_digest,h.inventory_revision FROM profiled_inventory_semantic_heads h JOIN profiled_presence_operations o ON o.node_id=h.node_id AND o.operation_id=h.operation_id WHERE h.node_id=$1 AND h.boot_id=$2 AND h.mode='scoped_v1' AND o.kind='inventory_v2'").bind(node).bind(identity.boot_id).fetch_one(pool).await?;
        let inventory: InventoryV2 =
            serde_json::from_slice(&row.get::<Vec<u8>, _>("request_bytes"))?;
        let receipt: InventoryReceipt =
            serde_json::from_slice(&row.get::<Vec<u8>, _>("receipt_bytes"))?;
        receipt.validate_for(&inventory)?;
        ensure!(
            inventory.identity == identity
                && inventory.runtime == prepared.runtime
                && inventory.runtime_qualification == prepared.qualification
                && receipt.inventory.revision == snapshot.revision
                && row.get::<i64, _>("inventory_revision") as u64 == snapshot.revision
                && row.get::<String, _>("snapshot_digest")
                    == Digest::of(&canonical(&json!(snapshot))?).as_str()
                && receipt.inventory.snapshot_digest.as_str()
                    == row.get::<String, _>("snapshot_digest"),
            "exact actual inventory receipt/head/snapshot"
        );
        temporal(
            now >= receipt.recorded_at_ms && now < receipt.expires_at_ms,
            TemporalKind::InventoryReceipt,
            inventory_clocks(started, now, last, &snapshot, &receipt),
        )?;
        let scope = ObservationScope {
            credential_revision: configured.credential.revision,
            provider: configured.credential.provider.clone(),
            credential_profile: prepared.credential.clone(),
            account_reference: configured.credential.account_reference.clone(),
            endpoint_reference: configured.credential.endpoint_reference.clone(),
            model: Some(recipe::MODEL.into()),
            node_id: Some(node),
        };
        let before = human(state, "/api/intelligence/registry", None).await?;
        let revision = before["revision"]
            .as_u64()
            .context("actual registry revision")?;
        let operation = Uuid::new_v4();
        let sequence = self
            .sequence
            .checked_add(1)
            .context("assessment sequence overflow")?;
        peer(&witness)?;
        self.image_continuity(prepared, evidence)
            .context("sealed image continuity unavailable")?;
        let checked_at = current_ms(state)?;
        temporal(
            checked_at
                .checked_sub(started)
                .is_some_and(|elapsed| elapsed < 5000),
            TemporalKind::AssessmentAge,
            inventory_clocks(started, checked_at, last, &snapshot, &receipt),
        )?;
        temporal(
            checked_at < snapshot.expires_at_ms
                && checked_at < snapshot.credential_bindings[0].expires_at_ms,
            TemporalKind::Inventory,
            inventory_clocks(started, checked_at, last, &snapshot, &receipt),
        )?;
        temporal(
            checked_at < receipt.expires_at_ms,
            TemporalKind::InventoryReceipt,
            inventory_clocks(started, checked_at, last, &snapshot, &receipt),
        )?;
        let observed = human(state, "/api/intelligence/observations", Some(json!({"operation_id":operation,"expected_revision":revision,"sequence":sequence,"scope":scope,"state":"available","ttl_ms":30000,"cooldown_ms":0}))).await?;
        let typed: AvailabilityObservation = serde_json::from_value(observed.clone())?;
        typed.validate()?;
        ensure!(
            typed.scope == scope
                && typed.sequence == sequence
                && typed.state == aq_selection::Availability::Available
                && matches!(
                    typed.source,
                    aq_selection::ObservationSource::Operator { .. }
                ),
            "new actual owner observation timing/scope"
        );
        let mut clocks = inventory_clocks(started, checked_at, last, &snapshot, &receipt);
        clocks.observation_received_at_ms = Some(typed.received_at_ms);
        clocks.observation_expires_at_ms = Some(typed.expires_at_ms);
        temporal(
            typed.received_at_ms >= checked_at
                && typed.expires_at_ms.checked_sub(typed.received_at_ms) == Some(30000)
                && typed
                    .received_at_ms
                    .checked_sub(started)
                    .is_some_and(|elapsed| elapsed < 5000)
                && typed.received_at_ms < snapshot.expires_at_ms
                && typed.received_at_ms < snapshot.credential_bindings[0].expires_at_ms
                && typed.received_at_ms < receipt.expires_at_ms,
            TemporalKind::Observation,
            clocks,
        )?;
        // Tie the explicitly borrowed fixture pool to the real protected owner
        // operation. The caller supplies state and pool from the same Env.
        let retained: Value = sqlx::query_scalar(
            "SELECT response FROM intelligence_operations WHERE operation_id=$1",
        )
        .bind(operation)
        .fetch_one(pool)
        .await?;
        ensure!(
            retained == observed,
            "actual fixture pool/owner operation join"
        );
        if let Some((operation, previous)) = &self.last {
            let retained: Value = sqlx::query_scalar(
                "SELECT response FROM intelligence_operations WHERE operation_id=$1",
            )
            .bind(operation)
            .fetch_one(pool)
            .await?;
            ensure!(
                &retained == previous,
                "prior observation operation reply changed"
            );
        }
        self.sequence = sequence;
        self.last = Some((operation, observed.clone()));
        self.subject = Some(subject);
        Ok(
            json!({"scope":"finite_local_responses_peer_only","observation":observed,"checked_at_ms":checked_at,"journal_digest":journal_digest,"actual_image_count":5,"image_evidence":"pre_start_hash_and_current_retained_fd_route_continuity","mutable_checked_started_at_ms":started,"material_revalidated":true,"native_registration_revalidated":true,"inventory_revalidated":true,"finite_peer_listening":true,"provider_availability_claim":false,"selection_rewritten":false,"ttl_ms":30000,"observation_max_age_ms":5000}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn small_image(root: &Path, name: &str) -> LocalImage {
        let outer = root.join(name);
        let bytes = b"explicit small synthetic image, never executed";
        std::fs::write(&outer, bytes).unwrap();
        std::fs::set_permissions(&outer, std::fs::Permissions::from_mode(0o600)).unwrap();
        LocalImage {
            outer,
            guest: ProgramPin {
                path: PathBuf::from("/opt/aq-qualification/synthetic-image"),
                artifact: aq_admission::ArtifactRef {
                    id: aq_admission::Text::new("aq.synthetic-image-test").unwrap(),
                    version: aq_admission::Text::new("1").unwrap(),
                    digest: Digest::of(bytes),
                },
                bytes: bytes.len() as u64,
            },
        }
    }
    #[test]
    fn image_seals_require_real_hash_and_original_pin() {
        let root = tempfile::tempdir().unwrap();
        let image = small_image(root.path(), "image");
        let sealed = SealedImage::seal(&image).unwrap();
        sealed.current(&image).unwrap();
        let mut changed = LocalImage {
            outer: image.outer.clone(),
            guest: image.guest.clone(),
        };
        changed.guest.artifact.digest = Digest::of(b"different synthetic bytes");
        assert!(SealedImage::seal(&changed).is_err());
        assert!(sealed.current(&changed).is_err());
    }
    #[test]
    fn image_seals_refuse_metadata_change_and_same_bytes_route_replacement() {
        let root = tempfile::tempdir().unwrap();
        let image = small_image(root.path(), "image");
        let sealed = SealedImage::seal(&image).unwrap();
        std::fs::set_permissions(&image.outer, std::fs::Permissions::from_mode(0o400)).unwrap();
        assert!(sealed.current(&image).is_err());
        std::fs::set_permissions(&image.outer, std::fs::Permissions::from_mode(0o600)).unwrap();
        let sealed = SealedImage::seal(&image).unwrap();
        let replacement = small_image(root.path(), "replacement");
        std::fs::rename(&replacement.outer, &image.outer).unwrap();
        assert!(
            sealed.current(&image).is_err(),
            "retained old FD cannot bless replacement route even with identical bytes"
        );
    }
    #[test]
    fn temporal_refusal_exports_only_static_code_and_numeric_clocks() {
        let error = temporal(
            false,
            TemporalKind::AssessmentAge,
            TemporalClocks {
                started_at_ms: Some(10),
                checked_at_ms: Some(5011),
                inventory_expires_at_ms: Some(30000),
                receipt_expires_at_ms: Some(30001),
                ..Default::default()
            },
        )
        .unwrap_err()
        .context("unexported arbitrary path or body");
        assert_eq!(refusal_code(&error), "freshness_assessment_age");
        let clocks = refusal_clocks(&error);
        assert_eq!(clocks["started_at_ms"], 10);
        assert_eq!(clocks["checked_at_ms"], 5011);
        assert!(
            clocks
                .as_object()
                .unwrap()
                .values()
                .all(|value| value.is_null() || value.is_u64())
        );
        let other = anyhow::anyhow!("unexported synthetic secret/path/error");
        assert_eq!(refusal_code(&other), "assessment_unavailable");
        assert!(refusal_clocks(&other).is_null());
    }
}
