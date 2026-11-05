//! Guarded, finite terminal effects. This module never launches a session and
//! deliberately provides no snapshot/capture or unknown-resolution operation.
use super::*;
use aq_session_protocol::{ControllerLease, InputDisposition, InputReceipt, TerminalIdentity};
use aq_terminal_relay_protocol::{Effect, Fence, FenceAck, ResizeReceipt};
use chacha20poly1305::{
    ChaCha20Poly1305, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use rand::RngCore;
use std::sync::Arc;
use zeroize::Zeroizing;

pub const MAX_INPUT_RECORDS: usize = 1024;
pub const MAX_CONTROL_RECORDS: usize = 1024;
const MAX_REQUEST: u64 = 96 * 1024;
// Ciphertext uses a JSON byte array (at most4 encoded bytes per raw byte).
// Leave envelope space and reserve the longer Unknown outcome before Injecting.
const MAX_LEDGER_PLAIN: usize = 480 * 1024;
const STATE: &str = "terminal-state.json";
const BINDING: &str = "terminal-binding.json";

/// A trusted embedding must qualify its clock against the coordinator. There is
/// no default: unqualified clocks must not construct an input-capable bridge.
#[derive(Clone, Copy)]
pub struct QualifiedClock {
    skew_ms: u64,
}
impl QualifiedClock {
    pub fn maximum_skew_ms(&self) -> u64 {
        self.skew_ms
    }
    pub fn assume_bounded_skew(skew_ms: u64) -> Result<Self> {
        ensure!(skew_ms <= 1000, "clock uncertainty bounds");
        Ok(Self { skew_ms })
    }
    fn project(&self, deadline: u64) -> Result<u64> {
        let remaining = deadline
            .checked_sub(wall_ms()?)
            .context("terminal deadline elapsed")?
            .checked_sub(self.skew_ms)
            .context("terminal deadline uncertainty")?;
        ensure!(
            remaining > 0 && remaining <= 15000,
            "terminal deadline bounds"
        );
        ownership::raw_boot_ms()
            .checked_add(remaining)
            .context("terminal boot deadline overflow")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalBinding {
    pub terminal: TerminalIdentity,
    pub guardian: GuardianIdentity,
    pub pane: PaneIdentity,
    pub directory: DirectoryIdentity,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectoryIdentity {
    pub device: u64,
    pub inode: u64,
}
pub(crate) struct DirectoryGuard {
    file: File,
    path: PathBuf,
    identity: DirectoryIdentity,
}
impl DirectoryGuard {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)?;
        let m = file.metadata()?;
        ensure!(
            m.is_dir() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o777 == 0o700,
            "unsafe host directory"
        );
        let guard = Self {
            file,
            path: path.into(),
            identity: DirectoryIdentity {
                device: m.dev(),
                inode: m.ino(),
            },
        };
        guard.verify()?;
        Ok(guard)
    }
    pub(crate) fn existing(path: &Path) -> Result<Self> {
        let guard = Self::open(path)?;
        let anchor: DirectoryIdentity = private_json(&path.join("directory.json"), 1024)?;
        ensure!(anchor == guard.identity, "host directory anchor changed");
        guard.verify()?;
        Ok(guard)
    }
    pub(crate) fn anchor(&self) -> Result<()> {
        self.verify()?;
        durable_new(
            &self.path.join("directory.json"),
            &serde_json::to_vec(&self.identity)?,
        )?;
        self.verify()
    }
    pub(crate) fn verify(&self) -> Result<()> {
        let m = std::fs::symlink_metadata(&self.path)?;
        let held = self.file.metadata()?;
        ensure!(
            m.is_dir()
                && m.uid() == unsafe { libc::geteuid() }
                && m.mode() & 0o777 == 0o700
                && DirectoryIdentity {
                    device: m.dev(),
                    inode: m.ino()
                } == self.identity
                && DirectoryIdentity {
                    device: held.dev(),
                    inode: held.ino()
                } == self.identity
                && held.nlink() > 0,
            "host directory replaced"
        );
        Ok(())
    }
}
impl TerminalBinding {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.directory.inode > 0, "terminal directory identity");
        self.terminal.validate()?;
        self.guardian.intent.session.validate()?;
        ensure!(
            self.terminal.session == self.guardian.intent.session
                && !self.guardian.intent.launch_id.is_nil()
                && !self.guardian.nonce.is_nil(),
            "terminal binding identities"
        );
        ensure!(
            Uuid::parse_str(&self.guardian.machine_boot_id).is_ok_and(|id| !id.is_nil())
                && self.guardian.pid_namespace.len() <= 128
                && self.guardian.pid_namespace.starts_with("pid:[")
                && self.guardian.pid_namespace.ends_with(']')
                && self.pane.session_name == "aq"
                && self.pane.pane_id.len() <= 32
                && self
                    .pane
                    .pane_id
                    .strip_prefix('%')
                    .is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())),
            "terminal physical identity bounds"
        );
        for process in [&self.guardian.process, &self.pane.server, &self.pane.pane] {
            ensure!(
                process.pid > 1 && process.start_ticks > 0,
                "terminal process evidence bounds"
            );
        }
        ensure!(
            self.guardian.process.pid != self.pane.server.pid
                && self.pane.server.pid != self.pane.pane.pid,
            "terminal process ancestry identities"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "body",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Receipt {
    Fence(FenceAck),
    Input(InputReceipt),
    Resize(ResizeReceipt),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    digest: Digest,
    receipt: Receipt,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    revision: u64,
    output_claim: Option<Uuid>,
    binding: TerminalBinding,
    fence: Option<Fence>,
    controller_deadline_boot_ms: u64,
    resize_sequence: u64,
    input: BTreeMap<Uuid, Record>,
    control: BTreeMap<Uuid, Record>,
}
impl State {
    fn record(&self, id: Uuid) -> Option<&Record> {
        self.input.get(&id).or_else(|| self.control.get(&id))
    }
    fn unknown(&self) -> bool {
        self.input.values().any(|r| matches!(&r.receipt,
        Receipt::Input(v) if matches!(v.disposition, InputDisposition::Injecting | InputDisposition::DeliveryUnknown)))
    }
    fn save(&mut self, dir: &Path) -> Result<()> {
        self.revision = self
            .revision
            .checked_add(1)
            .context("terminal ledger revision overflow")?;
        ensure!(
            self.input.len() <= MAX_INPUT_RECORDS && self.control.len() <= MAX_CONTROL_RECORDS,
            "terminal receipt capacity"
        );
        let mut worst = self.clone();
        let mut injecting = false;
        for record in worst.input.values_mut() {
            if let Receipt::Input(receipt) = &mut record.receipt
                && receipt.disposition == InputDisposition::Injecting
            {
                receipt.disposition = InputDisposition::DeliveryUnknown;
                injecting = true;
            }
        }
        ensure!(
            serde_json::to_vec(&worst)?.len() <= MAX_LEDGER_PLAIN - if injecting { 32 } else { 0 },
            "terminal ledger plaintext byte capacity"
        );
        let bytes = serde_json::to_vec(self)?;
        ensure!(
            bytes.len() as u64 <= MAX_LOCAL_FILE,
            "terminal receipt bytes"
        );
        durable_replace(
            &dir.join(STATE),
            &protect(dir, &self.binding, b"state", &bytes)?,
        )
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    binding: TerminalBinding,
    effect: Effect,
    deadline_boot_ms: u64,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ciphertext {
    version: u32,
    nonce: [u8; 12],
    bytes: Vec<u8>,
}

/// Opaque local capability tied to an existing immutable host, never a socket
/// selector. Submission returns only after mailbox publication is durable.
#[derive(Clone)]
pub struct TerminalHandle {
    directory: PathBuf,
    binding: TerminalBinding,
    directory_guard: Arc<DirectoryGuard>,
    pending_observation: bool,
}
impl SessionHandle {
    pub fn bind_terminal(&self, terminal: &TerminalIdentity) -> Result<TerminalHandle> {
        self.bind_terminal_inner(terminal, false)
    }
    pub(crate) fn bind_terminal_inner(
        &self,
        terminal: &TerminalIdentity,
        pending_observation: bool,
    ) -> Result<TerminalHandle> {
        terminal.validate()?;
        let _lock = RenewalLock::acquire(&self.directory.join("terminal.lock"))?;
        let directory_guard = Arc::new(DirectoryGuard::existing(&self.directory)?);
        let observation = self.observe()?;
        let config: SealedLaunch = read_json(&self.directory.join("launch.json"))?;
        ensure!(
            config.intent()? == self.identity.intent,
            "terminal sealed launch changed"
        );
        if pending_observation {
            let spec = config
                .observed
                .as_ref()
                .context("pending observer specification")?;
            spec.validate(&config)?;
            ensure!(
                &spec.terminal == terminal,
                "pending observer exact terminal"
            );
        }
        let wall_deadline = config.wall_deadline()?;
        live_phase(
            &self.directory,
            &observation,
            wall_deadline,
            observation.permit_deadline_boot_ms,
            pending_observation,
        )?;
        ensure!(
            observation.phase == HostPhase::Running
                || pending_observation && observation.phase == HostPhase::Starting,
            "terminal binding requires running host"
        );
        ensure!(
            terminal.session == self.identity.intent.session,
            "terminal session mismatch"
        );
        let binding = TerminalBinding {
            terminal: terminal.clone(),
            guardian: self.identity.clone(),
            pane: observation.pane.context("terminal pane unavailable")?,
            directory: directory_guard.identity,
        };
        binding.validate()?;
        let path = self.directory.join(BINDING);
        if path.exists() {
            let old: TerminalBinding = private_json(&path, MAX_REQUEST)?;
            ensure!(old == binding, "terminal binding replacement refused");
        } else {
            // Publish binding last: a partial installation grants no authority.
            let key_path = self.directory.join("terminal.key");
            if !key_path.exists() {
                let mut key = Zeroizing::new([0u8; 32]);
                rand::rngs::OsRng.fill_bytes(&mut *key);
                durable_new(&key_path, &*key)?;
            }
            let _ = key(&self.directory)?;
            let mut state = State {
                revision: 0,
                output_claim: None,
                binding: binding.clone(),
                fence: None,
                controller_deadline_boot_ms: 0,
                resize_sequence: 0,
                input: BTreeMap::new(),
                control: BTreeMap::new(),
            };
            let state_path = self.directory.join(STATE);
            if state_path.exists() {
                let old = load(&self.directory, &binding)?;
                ensure!(
                    old.binding == binding
                        && old.fence.is_none()
                        && old.input.is_empty()
                        && old.control.is_empty(),
                    "partial terminal binding mismatch"
                );
            } else {
                state.save(&self.directory)?;
            }
            durable_new(&path, &serde_json::to_vec(&binding)?)?;
        }
        Ok(TerminalHandle {
            directory: self.directory.clone(),
            binding,
            directory_guard,
            pending_observation,
        })
    }
}
impl TerminalHandle {
    /// Current visible plaintext from the exact owned pane. This is for local
    /// harness readiness/auth-dialog classification, not a renderer Snapshot or
    /// permission to resolve unknown input. No scrollback or caller tmux argv.
    pub fn current_viewport(&self) -> Result<String> {
        self.directory_guard.verify()?;
        let _lock = RenewalLock::acquire(&self.directory.join("terminal.lock"))?;
        let state = load(&self.directory, &self.binding)?;
        ensure!(
            state.binding == self.binding,
            "terminal viewport binding changed"
        );
        let host = SessionHandle {
            directory: self.directory.clone(),
            identity: self.binding.guardian.clone(),
            private_source: None,
        };
        let observation = host.observe()?;
        ensure!(
            observation.phase == HostPhase::Running
                && observation.pane.as_ref() == Some(&self.binding.pane),
            "terminal viewport requires original running pane"
        );
        let config: SealedLaunch = read_json(&self.directory.join("launch.json"))?;
        let output = guarded_tmux(
            &self.directory,
            &config,
            &observation,
            config.wall_deadline()?,
            observation.permit_deadline_boot_ms,
            &["capture-pane", "-p", "-t", &self.binding.pane.pane_id],
        )?;
        ensure!(output.len() <= 65_536, "terminal viewport byte bound");
        self.directory_guard.verify()?;
        String::from_utf8(output).context("terminal viewport UTF8")
    }
    pub(crate) fn output_settings(
        &self,
    ) -> Result<(PathBuf, PathBuf, aq_session_protocol::TerminalPolicy)> {
        self.directory_guard.verify()?;
        let config: SealedLaunch = read_json(&self.directory.join("launch.json"))?;
        ensure!(
            config.intent()? == self.binding.guardian.intent,
            "output sealed configuration changed"
        );
        let observation: HostObservation = read_json(&self.directory.join("state.json"))?;
        ensure!(
            observation.guardian == self.binding.guardian
                && observation.pane.as_ref() == Some(&self.binding.pane),
            "output physical identity changed"
        );
        if self.pending_observation {
            let spec = config
                .observed
                .as_ref()
                .context("pending output specification")?;
            spec.validate(&config)?;
            ensure!(
                spec.terminal == self.binding.terminal,
                "pending output terminal mismatch"
            );
        }
        live_phase(
            &self.directory,
            &observation,
            config.wall_deadline()?,
            observation.permit_deadline_boot_ms,
            self.pending_observation,
        )?;
        self.directory_guard.verify()?;
        Ok((
            self.directory.clone(),
            config.tmux,
            config.manifest.terminal().clone(),
        ))
    }
    pub(crate) fn claim_output(&self) -> Result<Uuid> {
        let _lock = RenewalLock::acquire(&self.directory.join("terminal.lock"))?;
        self.output_settings()?;
        let mut state = load(&self.directory, &self.binding)?;
        ensure!(
            state.output_claim.is_none(),
            "output producer already claimed; no implicit restart"
        );
        let claim = Uuid::new_v4();
        state.output_claim = Some(claim);
        state.save(&self.directory)?;
        durable_new(
            &self.directory.join("terminal-output.claim"),
            &serde_json::to_vec(&self.binding)?,
        )?;
        self.directory_guard.verify()?;
        Ok(claim)
    }
    pub(crate) fn viewer_claim(&self) -> Result<Uuid> {
        self.output_settings()?;
        ensure!(!self.pending_observation, "viewer requires actual Running");
        let config: SealedLaunch = read_json(&self.directory.join("launch.json"))?;
        ensure!(
            config.observed.as_ref().is_some_and(|s| s.interactive())
                && config.manifest.terminal().input_enabled,
            "PTY viewer requires explicit interactive profile"
        );
        load(&self.directory, &self.binding)?
            .output_claim
            .context("viewer requires original control observer claim")
    }
    pub fn binding(&self) -> &TerminalBinding {
        &self.binding
    }
    pub fn recover(host: &SessionHandle, terminal: &TerminalIdentity) -> Result<Self> {
        let directory_guard = Arc::new(DirectoryGuard::existing(&host.directory)?);
        let binding: TerminalBinding = private_json(&host.directory.join(BINDING), MAX_REQUEST)?;
        ensure!(
            binding.guardian == host.identity
                && binding.terminal == *terminal
                && binding.directory == directory_guard.identity,
            "terminal recovery binding mismatch"
        );
        let state = load(&host.directory, &binding)?;
        ensure!(state.binding == binding, "terminal state binding mismatch");
        let _ = key(&host.directory)?;
        Ok(Self {
            directory: host.directory.clone(),
            binding,
            directory_guard,
            pending_observation: false,
        })
    }
    /// A duplicate returns historical metadata; it never authorizes another
    /// physical effect. Busy admission is explicit and has no implicit retry.
    pub fn submit(&self, effect: &Effect, clock: QualifiedClock) -> Result<Option<Receipt>> {
        self.directory_guard.verify()?;
        effect.validate()?;
        ensure!(
            effect.terminal() == &self.binding.terminal,
            "terminal effect identity mismatch"
        );
        let id = operation_id(effect)?;
        let digest = digest(effect)?;
        let _lock = RenewalLock::acquire(&self.directory.join("terminal.lock"))?;
        let state = load(&self.directory, &self.binding)?;
        ensure!(
            !self.directory.join("terminal.disabled").exists(),
            "terminal effects disabled"
        );
        if let Some(old) = state.record(id) {
            ensure!(old.digest == digest, "changed terminal operation payload");
            return Ok(Some(old.receipt.clone()));
        }
        let host = SessionHandle {
            directory: self.directory.clone(),
            identity: self.binding.guardian.clone(),
            private_source: None,
        };
        let observation = host.observe()?;
        ensure!(
            observation.phase == HostPhase::Running
                && observation.pane.as_ref() == Some(&self.binding.pane),
            "terminal host not running or pane changed"
        );
        let config: SealedLaunch = read_json(&self.directory.join("launch.json"))?;
        if let Effect::Input(input) = effect {
            ensure!(
                config.manifest.terminal().input_enabled
                    && input.bytes.len() <= config.manifest.terminal().max_input_bytes as usize
                    && input.bytes.len()
                        <= config.manifest.terminal().max_pending_input_bytes as usize,
                "sealed terminal input policy"
            );
            ensure!(
                state.input.len() < MAX_INPUT_RECORDS,
                "terminal input receipt capacity"
            );
        } else {
            ensure!(
                state.control.len() < MAX_CONTROL_RECORDS,
                "terminal control receipt capacity"
            );
        }
        let deadline_boot_ms = match effect {
            Effect::Fence(v) => v
                .controller
                .as_ref()
                .map(|c| clock.project(c.expires_at_ms))
                .transpose()?
                .unwrap_or(observation.permit_deadline_boot_ms),
            Effect::Input(v) => clock.project(v.deadline_ms)?,
            Effect::Resize(v) => clock.project(v.deadline_ms)?,
            _ => bail!("terminal operation unsupported; no snapshot qualification"),
        };
        let path = request_path(&self.directory, effect);
        if path.exists() {
            let old = decrypt(
                &self.directory,
                &self.binding,
                &private_bytes(&path, MAX_REQUEST)?,
            )?;
            ensure!(
                operation_id(&old.effect)? == id && digest == self::digest(&old.effect)?,
                "terminal mailbox busy or changed request"
            );
            return Ok(None);
        }
        let request = Request {
            binding: self.binding.clone(),
            effect: effect.clone(),
            deadline_boot_ms,
        };
        durable_new(&path, &encrypt(&self.directory, &request)?)?;
        self.directory_guard.verify()?;
        Ok(None)
    }
    pub fn receipt(&self, id: Uuid) -> Result<Option<Receipt>> {
        self.directory_guard.verify()?;
        ensure!(!id.is_nil(), "terminal receipt ID");
        let _lock = RenewalLock::acquire(&self.directory.join("terminal.lock"))?;
        let mut state = load(&self.directory, &self.binding)?;
        if !supervisor::guardian_live(&self.binding.guardian)
            || self.directory.join("terminal.disabled").exists()
        {
            let mut changed = false;
            for record in state.input.values_mut() {
                if let Receipt::Input(v) = &mut record.receipt
                    && v.disposition == InputDisposition::Injecting
                {
                    v.disposition = InputDisposition::DeliveryUnknown;
                    changed = true;
                }
            }
            if changed {
                state.save(&self.directory)?;
            }
        }
        Ok(state.record(id).map(|r| r.receipt.clone()))
    }
    /// Recovery of a durably received network input whose plaintext was lost.
    /// Absence under the guardian's admission lock proves the physical boundary
    /// was never entered: cancel its mailbox and durably refuse it. An existing
    /// receipt is returned unchanged; this method never injects input.
    pub fn reject_unstarted_input(&self, id: Uuid, command_digest: &Digest) -> Result<Receipt> {
        self.directory_guard.verify()?;
        ensure!(!id.is_nil(), "terminal input recovery ID");
        let _lock = RenewalLock::acquire(&self.directory.join("terminal.lock"))?;
        let mut state = load(&self.directory, &self.binding)?;
        if let Some(old) = state.record(id) {
            ensure!(
                &old.digest == command_digest,
                "changed recovered input digest"
            );
            return Ok(old.receipt.clone());
        }
        ensure!(
            state.input.len() < MAX_INPUT_RECORDS,
            "terminal input receipt capacity"
        );
        let path = self.directory.join("terminal-effect.request");
        if path.exists() {
            let pending = decrypt(
                &self.directory,
                &self.binding,
                &private_bytes(&path, MAX_REQUEST)?,
            )?;
            if operation_id(&pending.effect)? == id {
                ensure!(
                    matches!(pending.effect, Effect::Input(_))
                        && &digest(&pending.effect)? == command_digest,
                    "recovered input mailbox mismatch"
                );
                std::fs::remove_file(&path)?;
                File::open(&self.directory)?.sync_all()?;
            }
        }
        let receipt = Receipt::Input(InputReceipt {
            operation_id: id,
            command_digest: command_digest.clone(),
            terminal: self.binding.terminal.clone(),
            disposition: if self.directory.join("terminal.disabled").exists() {
                InputDisposition::DeliveryUnknown
            } else {
                InputDisposition::Rejected
            },
        });
        state.input.insert(
            id,
            Record {
                digest: command_digest.clone(),
                receipt: receipt.clone(),
            },
        );
        state.save(&self.directory)?;
        Ok(receipt)
    }
}

fn wall_ms() -> Result<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis()
        .try_into()
        .context("terminal wall clock overflow")
}
fn operation_id(effect: &Effect) -> Result<Uuid> {
    match effect {
        Effect::Fence(v) => Ok(v.fence_id),
        Effect::Input(v) => Ok(v.operation_id),
        Effect::Resize(v) => Ok(v.operation_id),
        _ => bail!("terminal operation unsupported"),
    }
}
fn digest(effect: &Effect) -> Result<Digest> {
    Ok(match effect {
        Effect::Fence(v) => typed_digest(v)?,
        Effect::Input(v) => canonical_digest(v)?,
        Effect::Resize(v) => canonical_digest(v)?,
        _ => bail!("terminal operation unsupported"),
    })
}
fn request_path(dir: &Path, effect: &Effect) -> PathBuf {
    dir.join(if matches!(effect, Effect::Fence(_)) {
        "terminal-fence.request"
    } else {
        "terminal-effect.request"
    })
}
fn load(dir: &Path, binding: &TerminalBinding) -> Result<State> {
    binding.validate()?;
    let plaintext = unprotect(
        dir,
        binding,
        b"state",
        &private_bytes(&dir.join(STATE), MAX_LOCAL_FILE)?,
    )?;
    let state: State = serde_json::from_slice(&plaintext)
        .map_err(|_| anyhow::anyhow!("invalid terminal ledger"))?;
    ensure!(
        state.revision > 0
            && state.output_claim.is_none_or(|id| !id.is_nil())
            && state.binding == *binding
            && state.input.len() <= MAX_INPUT_RECORDS
            && state.control.len() <= MAX_CONTROL_RECORDS,
        "terminal ledger binding/bounds"
    );
    Ok(state)
}
fn private_bytes(path: &Path, max: u64) -> Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let m = file.metadata()?;
    ensure!(
        m.is_file()
            && m.nlink() == 1
            && m.uid() == unsafe { libc::geteuid() }
            && m.mode() & 0o777 == 0o600
            && m.len() <= max,
        "unsafe terminal material"
    );
    let mut bytes = Vec::new();
    file.take(max + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= max,
        "terminal material grew beyond bound"
    );
    Ok(bytes)
}
fn private_json<T: serde::de::DeserializeOwned>(path: &Path, max: u64) -> Result<T> {
    serde_json::from_slice(&private_bytes(path, max)?)
        .map_err(|_| anyhow::anyhow!("invalid terminal metadata"))
}
fn key(dir: &Path) -> Result<Zeroizing<Vec<u8>>> {
    let key = Zeroizing::new(private_bytes(&dir.join("terminal.key"), 32)?);
    ensure!(key.len() == 32, "terminal key length");
    Ok(key)
}
fn aad(binding: &TerminalBinding, purpose: &[u8]) -> Result<Vec<u8>> {
    let mut aad = b"aq.guarded-terminal-request/1\0".to_vec();
    aad.extend_from_slice(purpose);
    aad.push(0);
    aad.extend_from_slice(typed_digest(binding)?.as_str().as_bytes());
    Ok(aad)
}
fn typed_digest<T: Serialize>(value: &T) -> Result<Digest> {
    let mut json = serde_json::to_value(value)?;
    json.sort_all_objects();
    Ok(Digest::of(&serde_json::to_vec(&json)?))
}
fn encrypt(dir: &Path, request: &Request) -> Result<Vec<u8>> {
    let plaintext = Zeroizing::new(serde_json::to_vec(request)?);
    let encoded = protect(dir, &request.binding, b"request", &plaintext)?;
    ensure!(
        encoded.len() as u64 <= MAX_REQUEST,
        "terminal mailbox bytes"
    );
    Ok(encoded)
}
fn protect(
    dir: &Path,
    binding: &TerminalBinding,
    purpose: &[u8],
    plaintext: &[u8],
) -> Result<Vec<u8>> {
    let key = key(dir)?;
    let cipher =
        ChaCha20Poly1305::new_from_slice(&key).map_err(|_| anyhow::anyhow!("terminal key"))?;
    let mut nonce = [0u8; 12];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let bytes = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad: &aad(binding, purpose)?,
            },
        )
        .map_err(|_| anyhow::anyhow!("terminal request protection failed"))?;
    let encoded = serde_json::to_vec(&Ciphertext {
        version: 1,
        nonce,
        bytes,
    })?;
    ensure!(
        encoded.len() as u64 <= MAX_LOCAL_FILE,
        "terminal protected bytes"
    );
    Ok(encoded)
}
fn decrypt(dir: &Path, binding: &TerminalBinding, encoded: &[u8]) -> Result<Request> {
    let plaintext = unprotect(dir, binding, b"request", encoded)?;
    let request: Request = serde_json::from_slice(&plaintext)
        .map_err(|_| anyhow::anyhow!("invalid terminal request"))?;
    ensure!(
        request.binding == *binding,
        "terminal request binding mismatch"
    );
    request.effect.validate()?;
    ensure!(
        request.effect.terminal() == &binding.terminal,
        "terminal request session mismatch"
    );
    Ok(request)
}
fn unprotect(
    dir: &Path,
    binding: &TerminalBinding,
    purpose: &[u8],
    encoded: &[u8],
) -> Result<Zeroizing<Vec<u8>>> {
    let sealed: Ciphertext = serde_json::from_slice(encoded)
        .map_err(|_| anyhow::anyhow!("invalid protected terminal request"))?;
    ensure!(sealed.version == 1, "terminal request version");
    let key = key(dir)?;
    let cipher =
        ChaCha20Poly1305::new_from_slice(&key).map_err(|_| anyhow::anyhow!("terminal key"))?;
    let plaintext = Zeroizing::new(
        cipher
            .decrypt(
                Nonce::from_slice(&sealed.nonce),
                Payload {
                    msg: &sealed.bytes,
                    aad: &aad(binding, purpose)?,
                },
            )
            .map_err(|_| anyhow::anyhow!("terminal request authentication failed"))?,
    );
    Ok(plaintext)
}

/// Guardian-only; one fence and one data effect at most per ownership-loop turn.
pub(crate) fn tick(
    dir: &Path,
    config: &SealedLaunch,
    host: &HostObservation,
    wall_deadline: u64,
    directory_guard: &DirectoryGuard,
    revision_highwater: &mut u64,
) -> Result<()> {
    if !dir.join("terminal-fence.request").exists() && !dir.join("terminal-effect.request").exists()
    {
        return Ok(());
    }
    if !dir.join(BINDING).exists() {
        return Ok(());
    }
    directory_guard.verify()?;
    let Ok(_lock) = RenewalLock::acquire(&dir.join("terminal.lock")) else {
        return Ok(());
    };
    let binding: TerminalBinding = private_json(&dir.join(BINDING), MAX_REQUEST)?;
    ensure!(
        binding.guardian == host.guardian
            && host.pane.as_ref() == Some(&binding.pane)
            && binding.directory == directory_guard.identity,
        "terminal guardian binding mismatch"
    );
    let mut state = load(dir, &binding)?;
    ensure!(
        state.revision >= *revision_highwater,
        "terminal ledger rollback refused"
    );
    *revision_highwater = state.revision;
    for name in ["terminal-fence.request", "terminal-effect.request"] {
        let path = dir.join(name);
        if !path.exists() {
            continue;
        }
        let request = decrypt(dir, &binding, &private_bytes(&path, MAX_REQUEST)?)?;
        let id = operation_id(&request.effect)?;
        let digest = digest(&request.effect)?;
        if let Some(old) = state.record(id) {
            ensure!(
                old.digest == digest,
                "terminal request changed after admission"
            );
        } else {
            apply(
                dir,
                config,
                host,
                wall_deadline,
                &mut state,
                &request,
                (id, digest),
            )?;
        }
        std::fs::remove_file(&path)?;
        File::open(dir)?.sync_all()?;
        *revision_highwater = state.revision;
        directory_guard.verify()?;
    }
    Ok(())
}
fn live(dir: &Path, host: &HostObservation, wall_deadline: u64, deadline: u64) -> Result<()> {
    live_phase(dir, host, wall_deadline, deadline, false)
}
fn live_phase(
    dir: &Path,
    host: &HostObservation,
    wall_deadline: u64,
    deadline: u64,
    pending_observation: bool,
) -> Result<()> {
    let now = ownership::raw_boot_ms();
    ensure!(
        (host.phase == HostPhase::Running
            || pending_observation && host.phase == HostPhase::Starting)
            && now < host.permit_deadline_boot_ms
            && now < wall_deadline
            && now < deadline
            && !dir.join("stop.request").exists(),
        "terminal authority expired or cancelled"
    );
    let pane = host.pane.as_ref().context("terminal pane missing")?;
    ensure!(
        ownership::process_evidence(pane.server.pid)? == pane.server
            && ownership::process_evidence(pane.pane.pid)? == pane.pane
            && supervisor::guardian_live(&host.guardian),
        "terminal process identity changed"
    );
    for process in [&pane.server, &pane.pane] {
        ensure!(
            ownership::pidfd_is_live(&ownership::verified_pidfd(process)?)?,
            "terminal process exited"
        );
    }
    Ok(())
}
fn controller(state: &State, lease: &ControllerLease, now: u64) -> Result<()> {
    ensure!(
        state.fence.as_ref().and_then(|v| v.controller.as_ref()) == Some(lease)
            && now >= lease.issued_at_ms
            && now < lease.expires_at_ms
            && ownership::raw_boot_ms() < state.controller_deadline_boot_ms,
        "terminal controller stale or expired"
    );
    Ok(())
}
fn apply(
    dir: &Path,
    config: &SealedLaunch,
    host: &HostObservation,
    wall_deadline: u64,
    state: &mut State,
    request: &Request,
    identity: (Uuid, Digest),
) -> Result<()> {
    let (id, digest) = identity;
    let result = match &request.effect {
        Effect::Fence(fence) => {
            ensure!(
                state.control.len() < MAX_CONTROL_RECORDS,
                "terminal control receipt capacity"
            );
            ensure!(
                state
                    .fence
                    .as_ref()
                    .is_none_or(|old| fence.generation > old.generation),
                "stale terminal fence"
            );
            live(dir, host, wall_deadline, request.deadline_boot_ms)?;
            if let Some(lease) = &fence.controller {
                ensure!(
                    config.manifest.terminal().input_enabled
                        && wall_ms()? >= lease.issued_at_ms
                        && wall_ms()? < lease.expires_at_ms,
                    "terminal controller expired or input disabled"
                );
            }
            state.fence = Some(fence.clone());
            state.controller_deadline_boot_ms = request.deadline_boot_ms;
            state.resize_sequence = 0;
            Receipt::Fence(FenceAck {
                terminal: fence.terminal.clone(),
                fence_id: fence.fence_id,
                generation: fence.generation,
            })
        }
        Effect::Input(input) => {
            ensure!(
                state.input.len() < MAX_INPUT_RECORDS,
                "terminal input receipt capacity"
            );
            let valid = (|| -> Result<()> {
                ensure!(
                    !state.unknown(),
                    "terminal input paused after uncertain effect"
                );
                ensure!(
                    config.manifest.terminal().input_enabled
                        && input.bytes.len() <= config.manifest.terminal().max_input_bytes as usize
                        && input.bytes.len()
                            <= config.manifest.terminal().max_pending_input_bytes as usize,
                    "sealed terminal input policy"
                );
                controller(state, &input.lease, wall_ms()?)?;
                ensure!(
                    wall_ms()? >= input.issued_at_ms && wall_ms()? < input.deadline_ms,
                    "terminal input deadline"
                );
                live(dir, host, wall_deadline, request.deadline_boot_ms)
            })();
            let mut receipt = InputReceipt {
                operation_id: id,
                command_digest: digest.clone(),
                terminal: input.lease.terminal.clone(),
                disposition: if valid.is_ok() {
                    InputDisposition::Injecting
                } else {
                    InputDisposition::Rejected
                },
            };
            state.input.insert(
                id,
                Record {
                    digest: digest.clone(),
                    receipt: Receipt::Input(receipt.clone()),
                },
            );
            state.save(dir)?; // This is the irreversible no-replay boundary.
            if valid.is_ok() {
                test_fault(dir, "before-effect")?;
                let result = inject(
                    dir,
                    config,
                    host,
                    wall_deadline,
                    request.deadline_boot_ms,
                    input,
                );
                test_fault(dir, "after-effect")?;
                receipt.disposition = if result.is_ok() {
                    InputDisposition::Applied
                } else {
                    InputDisposition::DeliveryUnknown
                };
            }
            state.input.insert(
                id,
                Record {
                    digest,
                    receipt: Receipt::Input(receipt),
                },
            );
            return state.save(dir);
        }
        Effect::Resize(resize) => {
            ensure!(
                state.control.len() < MAX_CONTROL_RECORDS,
                "terminal control receipt capacity"
            );
            controller(state, &resize.lease, wall_ms()?)?;
            ensure!(
                resize.sequence > state.resize_sequence
                    && wall_ms()? >= resize.issued_at_ms
                    && wall_ms()? < resize.deadline_ms,
                "terminal resize stale or expired"
            );
            live(dir, host, wall_deadline, request.deadline_boot_ms)?;
            // Resize also has an at-most-once intent; an uncertain result is not
            // replayed as another command under the same operation ID.
            state.resize_sequence = resize.sequence;
            let receipt = ResizeReceipt {
                terminal: resize.lease.terminal.clone(),
                operation_id: id,
                command_digest: digest.clone(),
                sequence: resize.sequence,
                applied: false,
            };
            state.control.insert(
                id,
                Record {
                    digest: digest.clone(),
                    receipt: Receipt::Resize(receipt.clone()),
                },
            );
            state.save(dir)?;
            let applied = guarded_tmux(
                dir,
                config,
                host,
                wall_deadline,
                request.deadline_boot_ms,
                &[
                    "resize-window",
                    "-t",
                    &state.binding.pane.pane_id,
                    "-x",
                    &resize.columns.to_string(),
                    "-y",
                    &resize.rows.to_string(),
                ],
            )
            .is_ok();
            Receipt::Resize(ResizeReceipt { applied, ..receipt })
        }
        _ => bail!("terminal operation unsupported"),
    };
    state.control.insert(
        id,
        Record {
            digest,
            receipt: result,
        },
    );
    state.save(dir)
}
fn test_fault(_dir: &Path, _stage: &str) -> Result<()> {
    #[cfg(feature = "test-support")]
    {
        let arm = _dir.join(format!("terminal-fault-{_stage}.arm"));
        if arm.exists() {
            std::fs::remove_file(arm)?;
            durable_new(&_dir.join("terminal-fault.reached"), _stage.as_bytes())?;
            unsafe {
                libc::raise(libc::SIGSTOP);
            }
        }
    }
    Ok(())
}
fn inject(
    dir: &Path,
    config: &SealedLaunch,
    host: &HostObservation,
    wall_deadline: u64,
    deadline: u64,
    input: &aq_session_protocol::InputCommand,
) -> Result<()> {
    live(dir, host, wall_deadline, deadline)?;
    let buffer = format!("aq-{}", input.operation_id.simple());
    let bounded_deadline = deadline
        .min(wall_deadline)
        .min(host.permit_deadline_boot_ms);
    let timeout = Duration::from_millis(
        bounded_deadline
            .saturating_sub(ownership::raw_boot_ms())
            .min(250),
    );
    bounded_command_input(
        Command::new(&config.tmux)
            .args(["-N", "-S"])
            .arg(dir.join("tmux.sock"))
            .args(["load-buffer", "-b", &buffer, "-"]),
        Some(&input.bytes),
        timeout,
    )?;
    live(dir, host, wall_deadline, deadline)?;
    ensure!(
        wall_ms()? < input.deadline_ms && wall_ms()? < input.lease.expires_at_ms,
        "terminal input expired before paste"
    );
    // -r preserves literal LF bytes; no bracketed-paste wrapper or shell exists.
    let result = guarded_tmux(
        dir,
        config,
        host,
        wall_deadline,
        deadline,
        &[
            "paste-buffer",
            "-r",
            "-d",
            "-b",
            &buffer,
            "-t",
            &host.pane.as_ref().context("terminal pane missing")?.pane_id,
        ],
    );
    // Target only the immutable pane ID, never a user-provided session name.
    result.map(|_| ())
}
pub(crate) fn guarded_tmux(
    dir: &Path,
    config: &SealedLaunch,
    host: &HostObservation,
    wall_deadline: u64,
    deadline: u64,
    args: &[&str],
) -> Result<Vec<u8>> {
    live(dir, host, wall_deadline, deadline)?;
    let deadline = deadline
        .min(wall_deadline)
        .min(host.permit_deadline_boot_ms);
    let budget = Duration::from_millis(deadline.saturating_sub(ownership::raw_boot_ms()).min(250));
    let result = bounded_command_input(
        Command::new(&config.tmux)
            .args(["-N", "-S"])
            .arg(dir.join("tmux.sock"))
            .args(args),
        None,
        budget,
    )?;
    live(dir, host, wall_deadline, deadline)?;
    Ok(result)
}

pub(crate) fn verify_output_claim(
    dir: &Path,
    binding: &TerminalBinding,
    claim: Uuid,
    host: &HostObservation,
    guard: &DirectoryGuard,
    highwater: &mut u64,
    pending_observation: bool,
) -> Result<()> {
    guard.verify()?;
    binding.validate()?;
    ensure!(
        !claim.is_nil()
            && binding.guardian == host.guardian
            && (host.phase == HostPhase::Running
                || pending_observation && host.phase == HostPhase::Starting)
            && host.pane.as_ref() == Some(&binding.pane)
            && binding.directory == guard.identity,
        "reader physical binding"
    );
    let recorded: TerminalBinding = private_json(&dir.join(BINDING), MAX_REQUEST)?;
    let marker: TerminalBinding = private_json(&dir.join("terminal-output.claim"), MAX_REQUEST)?;
    ensure!(
        recorded == *binding && marker == *binding,
        "reader immutable claim binding"
    );
    // Atomic encrypted state replacement gives a coherent version. The claim is
    // immutable; concurrent input receipts need no lock across this read.
    let state = load(dir, binding)?;
    ensure!(
        state.output_claim == Some(claim) && state.revision >= *highwater,
        "reader claim or ledger highwater mismatch"
    );
    *highwater = state.revision;
    guard.verify()
}
