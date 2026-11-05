//! One closed derived Codex auth input, pinned by the existing host intent.
//! Private bytes are never serialized in launch metadata or command arguments.
use super::*;
use std::os::fd::{AsRawFd, FromRawFd};
use zeroize::Zeroizing;
const MAX_MATERIAL: u64 = 65536;
pub(crate) mod reconciliation;
/// Independent original installed-material pins, never newly installed bytes.
pub struct OriginalPrivateMaterial {
    pub digest: Digest,
    pub bytes: u64,
    pub record_digest: Digest,
}
/// Public metadata binding a fresh local capability to one actual launch.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopedCodexAuthBinding {
    pub binding: aq_session_protocol::LaunchBinding,
    pub launch_id: Uuid,
    pub origin: String,
    pub deadline_boot_ms: u64,
}
impl ScopedCodexAuthBinding {
    fn validate(&self) -> Result<()> {
        use aq_session_protocol::Validate;
        self.binding.validate()?;
        let port = self
            .origin
            .strip_prefix("http://127.0.0.1:")
            .context("scoped auth numeric loopback origin")?
            .parse::<u16>()?;
        ensure!(
            port > 0
                && self.origin == format!("http://127.0.0.1:{port}")
                && !self.launch_id.is_nil()
                && self.deadline_boot_ms > 0,
            "scoped auth exact origin/launch/deadline"
        );
        Ok(())
    }
    fn verify_launch(&self, config: &SealedLaunch) -> Result<()> {
        self.validate()?;
        ensure!(
            self.launch_id == config.launch_id
                && self.binding.session == config.manifest.task()?.bindings.session
                && self.binding.attempt_id == config.manifest.task()?.bindings.attempt_id
                && self.binding.assignment_generation
                    == config.manifest.task()?.bindings.assignment_generation
                && self.binding.launch_manifest_digest == canonical_digest(&config.manifest)?
                && self.binding.graph_access_profile_digest
                    == config.manifest.task()?.bindings.graph_access_profile.digest
                && self.deadline_boot_ms == config.permit.expires_boot_ms,
            "scoped auth exact launch binding/deadline"
        );
        let setting = format!("openai_base_url=\"{}/v1\"", self.origin);
        let actual: Vec<_> = config
            .command
            .arguments
            .windows(2)
            .filter(|v| v[0] == "--config" && v[1].starts_with("openai_base_url="))
            .collect();
        ensure!(
            actual.len() == 1 && actual[0][1] == setting,
            "scoped auth exact local transport route"
        );
        Ok(())
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScopedAuthPin {
    scope: ScopedCodexAuthBinding,
    capability_digest: Digest,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChatgptAuthPin {
    binding: aq_session_protocol::LaunchBinding,
    launch_id: Uuid,
    deadline_boot_ms: u64,
}
impl ChatgptAuthPin {
    fn verify_launch(&self, config: &SealedLaunch) -> Result<()> {
        self.binding.validate()?;
        ensure!(
            self.launch_id == config.launch_id
                && self.binding.session == config.manifest.task()?.bindings.session
                && self.binding.attempt_id == config.manifest.task()?.bindings.attempt_id
                && self.binding.assignment_generation
                    == config.manifest.task()?.bindings.assignment_generation
                && self.binding.launch_manifest_digest == canonical_digest(&config.manifest)?
                && self.binding.graph_access_profile_digest
                    == config.manifest.task()?.bindings.graph_access_profile.digest
                && self.deadline_boot_ms == config.permit.expires_boot_ms,
            "ChatGPT auth exact launch binding/deadline"
        );
        ensure!(config.command.arguments.windows(2).any(|v| v[0] == "--config" && v[1] == "cli_auth_credentials_store=\"file\"")
            && !config.command.arguments.iter().any(|v| v.starts_with("openai_base_url=") || v.starts_with("model_providers.")),
            "ChatGPT login requires private file store and native provider route");
        Ok(())
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourcePin {
    owner: ProcessEvidence,
    directory_fd: i32,
    device: u64,
    inode: u64,
    bytes: u64,
    digest: Digest,
    record_digest: Digest,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PrivateInputRecord {
    source: SourcePin,
    home_device: u64,
    home_inode: u64,
    auth_device: u64,
    auth_inode: u64,
    auth_bytes: u64,
    auth_digest: Digest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scoped: Option<ScopedAuthPin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    chatgpt: Option<ChatgptAuthPin>,
}
impl std::fmt::Debug for PrivateInputRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PrivateInputRecord(<private pins>)")
    }
}
impl PrivateInputRecord {
    pub(crate) fn scoped(&self) -> bool {
        self.scoped.is_some()
    }
}
/// Constructed from an actual retained material directory. Keeping this input
/// alive keeps its descriptor pin valid; it grants no material/exec permission.
pub struct PrivateLaunchInput {
    source: File,
    pin: SourcePin,
    auth: Zeroizing<Vec<u8>>,
    scoped: Option<ScopedAuthPin>,
    chatgpt: Option<ChatgptAuthPin>,
}
fn directory(file: &File, name: &std::ffi::CStr) -> Result<File> {
    let fd = unsafe {
        libc::openat(
            file.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    ensure!(fd >= 0, "private input directory unavailable");
    Ok(unsafe { File::from_raw_fd(fd) })
}
fn private_directory(file: &File) -> Result<std::fs::Metadata> {
    let m = file.metadata()?;
    ensure!(
        m.is_dir() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o7777 == 0o700,
        "private input directory identity/mode"
    );
    Ok(m)
}
fn read_member(
    dir: &File,
    name: &std::ffi::CStr,
    limit: u64,
) -> Result<(Zeroizing<Vec<u8>>, std::fs::Metadata)> {
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    ensure!(fd >= 0, "private input file unavailable");
    let mut file = unsafe { File::from_raw_fd(fd) };
    let m = file.metadata()?;
    ensure!(
        m.is_file()
            && m.uid() == unsafe { libc::geteuid() }
            && m.mode() & 0o7777 == 0o600
            && m.nlink() == 1
            && m.len() > 0
            && m.len() <= limit,
        "private input file identity/mode/bound"
    );
    let mut bytes = Zeroizing::new(Vec::with_capacity(m.len() as usize));
    (&mut file).take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 == m.len() && file.metadata()?.len() == m.len(),
        "private input changed during read"
    );
    Ok((bytes, m))
}
fn read_source(pin: &SourcePin) -> Result<Zeroizing<Vec<u8>>> {
    let live = ownership::verified_pidfd(&pin.owner)?;
    ensure!(
        ownership::pidfd_is_live(&live)?
            && pin.directory_fd >= 3
            && pin.bytes > 0
            && pin.bytes <= MAX_MATERIAL,
        "private source owner/bytes"
    );
    let path = format!("/proc/{}/fd/{}", pin.owner.pid, pin.directory_fd);
    // This is the exact verified process's fd namespace, not a caller-chosen
    // filesystem symlink. The opened directory must match the retained pin.
    let dir = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(path)?;
    let m = private_directory(&dir)?;
    ensure!(
        m.dev() == pin.device && m.ino() == pin.inode,
        "private source descriptor changed"
    );
    let (record, _) = read_member(&dir, c"record.json", 16384)?;
    ensure!(
        Digest::of(&record) == pin.record_digest,
        "private original installation record changed"
    );
    let (bytes, _) = read_member(&dir, c"material.bin", MAX_MATERIAL)?;
    ensure!(
        bytes.len() as u64 == pin.bytes
            && Digest::of(&bytes) == pin.digest
            && ownership::pidfd_is_live(&live)?,
        "private original material changed"
    );
    Ok(bytes)
}
fn derive_auth(key: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    ensure!(
        !key.is_empty() && key.len() <= 4096 && key.iter().all(|b| b.is_ascii_graphic()),
        "Codex API key byte format"
    );
    let mut out = Zeroizing::new(Vec::with_capacity(key.len() + 32));
    out.extend_from_slice(b"{\"OPENAI_API_KEY\":");
    serde_json::to_writer(&mut *out, std::str::from_utf8(key)?)?;
    out.push(b'}');
    Ok(out)
}
fn capability(bytes: &[u8]) -> Result<()> {
    ensure!(
        bytes.len() == 77
            && bytes.starts_with(b"aq-responses-")
            && bytes[13..]
                .iter()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b)),
        "scoped Codex capability format"
    );
    Ok(())
}
fn verify_scoped_auth(auth: &[u8], pin: &ScopedAuthPin) -> Result<()> {
    pin.scope.validate()?;
    verify_local_capability_auth(auth, &pin.capability_digest)
}
pub(crate) fn verify_local_capability_auth(auth: &[u8], expected: &Digest) -> Result<()> {
    let key = auth
        .strip_prefix(b"{\"OPENAI_API_KEY\":\"")
        .and_then(|v| v.strip_suffix(b"\"}"))
        .context("scoped Codex auth encoding")?;
    capability(key)?;
    ensure!(
        Digest::of(key) == *expected,
        "scoped Codex capability changed"
    );
    Ok(())
}
impl PrivateLaunchInput {
    /// Validate the native Codex subscription credential document, without
    /// logging, returning or copying its token fields into public metadata.
    pub fn validate_chatgpt_auth_json(bytes: &[u8]) -> Result<()> {
        fn secret<'de, D: serde::Deserializer<'de>>(
            d: D,
        ) -> std::result::Result<Zeroizing<String>, D::Error> {
            String::deserialize(d).map(Zeroizing::new)
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Tokens {
            #[serde(deserialize_with = "secret")]
            id_token: Zeroizing<String>,
            #[serde(deserialize_with = "secret")]
            access_token: Zeroizing<String>,
            #[serde(deserialize_with = "secret")]
            refresh_token: Zeroizing<String>,
            #[serde(deserialize_with = "secret")]
            account_id: Zeroizing<String>,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Auth {
            auth_mode: String,
            #[serde(rename = "OPENAI_API_KEY")]
            api_key: Option<()>,
            tokens: Tokens,
            last_refresh: Option<String>,
        }
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_MATERIAL as usize,
            "Codex login JSON byte bound"
        );
        let auth: Auth = serde_json::from_slice(bytes)
            .map_err(|_| anyhow::anyhow!("unsupported private Codex login JSON"))?;
        ensure!(
            auth.auth_mode == "chatgpt" && auth.api_key.is_none(),
            "native ChatGPT login required"
        );
        for token in [
            &auth.tokens.id_token,
            &auth.tokens.access_token,
            &auth.tokens.refresh_token,
        ] {
            ensure!(
                !token.is_empty()
                    && token.len() <= 32768
                    && token.bytes().all(|b| b.is_ascii_graphic()),
                "Codex login token shape"
            );
        }
        ensure!(
            !auth.tokens.account_id.is_empty()
                && auth.tokens.account_id.len() <= 512
                && auth.tokens.account_id.bytes().all(|b| b.is_ascii_graphic())
                && auth
                    .last_refresh
                    .as_ref()
                    .is_none_or(|v| v.len() <= 128 && v.bytes().all(|b| b.is_ascii_graphic())),
            "Codex login account/refresh shape"
        );
        Ok(())
    }

    /// Existing scoped material ownership, with the original native ChatGPT
    /// auth document published only inside this launch's private CODEX_HOME.
    pub fn codex_chatgpt_auth_json(
        source: File,
        original: OriginalPrivateMaterial,
        binding: aq_session_protocol::LaunchBinding,
        launch_id: Uuid,
        deadline_boot_ms: u64,
    ) -> Result<Self> {
        binding.validate()?;
        ensure!(
            !launch_id.is_nil() && deadline_boot_ms > 0,
            "ChatGPT launch identity/deadline"
        );
        let m = private_directory(&source)?;
        let pin = SourcePin {
            owner: ownership::process_evidence(std::process::id())?,
            directory_fd: source.as_raw_fd(),
            device: m.dev(),
            inode: m.ino(),
            bytes: original.bytes,
            digest: original.digest,
            record_digest: original.record_digest,
        };
        let auth = read_source(&pin)?;
        Self::validate_chatgpt_auth_json(&auth)?;
        Ok(Self {
            source,
            pin,
            auth,
            scoped: None,
            chatgpt: Some(ChatgptAuthPin {
                binding,
                launch_id,
                deadline_boot_ms,
            }),
        })
    }
    pub fn codex_api_key(
        source: File,
        digest: Digest,
        bytes: u64,
        record_digest: Digest,
    ) -> Result<Self> {
        let m = private_directory(&source)?;
        let pin = SourcePin {
            owner: ownership::process_evidence(std::process::id())?,
            directory_fd: source.as_raw_fd(),
            device: m.dev(),
            inode: m.ino(),
            bytes,
            digest,
            record_digest,
        };
        let key = read_source(&pin)?;
        let auth = derive_auth(&key)?;
        Ok(Self {
            source,
            pin,
            auth,
            scoped: None,
            chatgpt: None,
        })
    }
    pub fn codex_scoped_capability(
        source: File,
        original: OriginalPrivateMaterial,
        scope: ScopedCodexAuthBinding,
        local_capability: Zeroizing<Vec<u8>>,
    ) -> Result<Self> {
        scope.validate()?;
        capability(&local_capability)?;
        let m = private_directory(&source)?;
        let pin = SourcePin {
            owner: ownership::process_evidence(std::process::id())?,
            directory_fd: source.as_raw_fd(),
            device: m.dev(),
            inode: m.ino(),
            bytes: original.bytes,
            digest: original.digest,
            record_digest: original.record_digest,
        };
        let original = read_source(&pin)?;
        ensure!(
            original.as_slice() != local_capability.as_slice(),
            "scoped capability must differ from original provider key"
        );
        let scoped = ScopedAuthPin {
            scope,
            capability_digest: Digest::of(&local_capability),
        };
        let auth = derive_auth(&local_capability)?;
        verify_scoped_auth(&auth, &scoped)?;
        Ok(Self {
            source,
            pin,
            auth,
            scoped: Some(scoped),
            chatgpt: None,
        })
    }
}
/// Empty files are reserved first; the record enters the existing host intent
/// before publication. This owner never adopts an existing auth directory.
pub(crate) struct Publication {
    input: PrivateLaunchInput,
    root: File,
    home: File,
    auth: File,
    record: PrivateInputRecord,
}
impl Publication {
    pub(crate) fn reserve(root: &Path, input: PrivateLaunchInput) -> Result<Self> {
        let root = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(root)?;
        private_directory(&root)?;
        ensure!(
            unsafe { libc::mkdirat(root.as_raw_fd(), c"codex-home".as_ptr(), 0o700) } == 0,
            "private home already exists or cannot be reserved"
        );
        let home = directory(&root, c"codex-home")?;
        let h = private_directory(&home)?;
        let fd = unsafe {
            libc::openat(
                home.as_raw_fd(),
                c"auth.json".as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        ensure!(fd >= 0, "private auth reservation refused");
        let auth = unsafe { File::from_raw_fd(fd) };
        let a = auth.metadata()?;
        let record = PrivateInputRecord {
            source: input.pin.clone(),
            home_device: h.dev(),
            home_inode: h.ino(),
            auth_device: a.dev(),
            auth_inode: a.ino(),
            auth_bytes: input.auth.len() as u64,
            auth_digest: Digest::of(&input.auth),
            scoped: input.scoped.clone(),
            chatgpt: input.chatgpt.clone(),
        };
        home.sync_all()?;
        root.sync_all()?;
        Ok(Self {
            input,
            root,
            home,
            auth,
            record,
        })
    }
    pub(crate) fn record(&self) -> PrivateInputRecord {
        self.record.clone()
    }
    pub(crate) fn publish(mut self) -> Result<File> {
        let original = read_source(&self.input.pin)?;
        if let Some(pin) = &self.input.scoped {
            verify_scoped_auth(&self.input.auth, pin)?;
        } else if self.input.chatgpt.is_some() {
            PrivateLaunchInput::validate_chatgpt_auth_json(&original)?;
            ensure!(
                original.as_slice() == self.input.auth.as_slice(),
                "private Codex login changed before publication"
            );
        } else {
            ensure!(
                derive_auth(&original)?.as_slice() == self.input.auth.as_slice(),
                "private input changed before publication"
            );
        }
        self.auth.write_all(&self.input.auth)?;
        self.auth.sync_all()?;
        self.home.sync_all()?;
        self.root.sync_all()?;
        verify_record(&self.root, &self.record, true)?;
        Ok(self.input.source)
    }
}
fn verify_record(root: &File, r: &PrivateInputRecord, original: bool) -> Result<()> {
    let home = directory(root, c"codex-home")?;
    let h = private_directory(&home)?;
    ensure!(
        h.dev() == r.home_device && h.ino() == r.home_inode,
        "private home replaced"
    );
    let (bytes, a) = read_member(
        &home,
        c"auth.json",
        if r.chatgpt.is_some() {
            MAX_MATERIAL
        } else {
            8192
        },
    )?;
    if r.chatgpt.is_some() && !original {
        // Native Codex may rotate this valid private login. Preserve any changed
        // document for explicit reconciliation; deleting it could lose refreshed
        // credentials. Never copy back or modify the operator's original input.
        ensure!(r.scoped.is_none(), "mixed Codex authentication profiles");
        PrivateLaunchInput::validate_chatgpt_auth_json(&bytes)?;
        ensure!(
            Digest::of(&bytes) == r.auth_digest,
            "changed private ChatGPT login preserved for credential reconciliation"
        );
        return Ok(());
    }
    ensure!(
        a.dev() == r.auth_device
            && a.ino() == r.auth_inode
            && bytes.len() as u64 == r.auth_bytes
            && Digest::of(&bytes) == r.auth_digest,
        "private auth replaced or changed"
    );
    if let Some(pin) = &r.scoped {
        verify_scoped_auth(&bytes, pin)?;
        if original {
            drop(read_source(&r.source)?);
        }
    } else if r.chatgpt.is_some() {
        ensure!(r.scoped.is_none(), "mixed Codex authentication profiles");
        PrivateLaunchInput::validate_chatgpt_auth_json(&bytes)?;
        if original {
            ensure!(
                read_source(&r.source)?.as_slice() == bytes.as_slice(),
                "private login/original material mismatch"
            );
        }
    } else if original {
        ensure!(
            derive_auth(&read_source(&r.source)?)?.as_slice() == bytes.as_slice(),
            "private auth/original material mismatch"
        );
    }
    Ok(())
}
pub(crate) fn verify_at_exec(config: &SealedLaunch, root: &Path) -> Result<()> {
    if let Some(r) = &config.private_input {
        let spec = config
            .observed
            .as_ref()
            .context("private inputs require observed native profile")?;
        ensure!(
            spec.native_exec()
                && r.source.owner == spec.owner
                && config.command.environment.get("CODEX_HOME")
                    == Some(
                        &root
                            .join("codex-home")
                            .to_str()
                            .context("private home UTF8")?
                            .to_owned()
                    ),
            "private input exact native owner/home"
        );
        if let Some(pin) = &r.scoped {
            pin.scope.verify_launch(config)?;
        }
        if let Some(pin) = &r.chatgpt {
            ensure!(r.scoped.is_none(), "mixed Codex authentication profiles");
            pin.verify_launch(config)?;
        }
        let root = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(root)?;
        private_directory(&root)?;
        verify_record(&root, r, true)?;
    }
    Ok(())
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PrivateCleanup {
    pub auth_removed: bool,
    pub home_removed: bool,
}
impl SessionHandle {
    pub fn cleanup_private_inputs(&mut self, proof: &StoppedProof) -> Result<PrivateCleanup> {
        ensure!(
            proof.guardian() == &self.identity
                && proof.observation().phase == HostPhase::Stopped
                && proof.observation().kernel_wait_echild,
            "private cleanup requires exact ECHILD proof"
        );
        let config: SealedLaunch = read_json(&self.directory.join("launch.json"))?;
        ensure!(
            config.intent()? == self.identity.intent,
            "private cleanup configuration changed"
        );
        let Some(r) = config.private_input else {
            return Ok(PrivateCleanup {
                auth_removed: false,
                home_removed: false,
            });
        };
        let root = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&self.directory)?;
        private_directory(&root)?;
        let _refresh_guard = reconciliation::cleanup_guard(&root, &r)?;
        match std::fs::symlink_metadata(self.directory.join("codex-home")) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.private_source.take();
                return Ok(PrivateCleanup {
                    auth_removed: true,
                    home_removed: true,
                });
            }
            Err(e) => return Err(e.into()),
            Ok(_) => {}
        }
        let home = directory(&root, c"codex-home")?;
        let h = private_directory(&home)?;
        ensure!(
            h.dev() == r.home_device && h.ino() == r.home_inode,
            "private cleanup home replaced"
        );
        let auth_exists =
            match std::fs::symlink_metadata(self.directory.join("codex-home/auth.json")) {
                Ok(_) => true,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
                Err(e) => return Err(e.into()),
            };
        if auth_exists {
            verify_record(&root, &r, false)?;
            ensure!(
                unsafe { libc::unlinkat(home.as_raw_fd(), c"auth.json".as_ptr(), 0) } == 0,
                "private auth cleanup failed"
            );
            home.sync_all()?;
        }
        let removed =
            unsafe { libc::unlinkat(root.as_raw_fd(), c"codex-home".as_ptr(), libc::AT_REMOVEDIR) }
                == 0;
        if !removed {
            ensure!(
                std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOTEMPTY),
                "private home cleanup failed"
            );
        }
        root.sync_all()?;
        self.private_source.take();
        Ok(PrivateCleanup {
            auth_removed: true,
            home_removed: removed,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    pub(super) fn login(refresh: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({"auth_mode":"chatgpt","OPENAI_API_KEY":null,"tokens":{"id_token":"offline-id","access_token":"offline-access","refresh_token":refresh,"account_id":"offline-account"},"last_refresh":"2026-11-01T00:00:00Z"})).unwrap()
    }
    #[test]
    fn subscription_login_is_explicit_closed_bounded_and_errors_hide_tokens() {
        PrivateLaunchInput::validate_chatgpt_auth_json(&login("offline-refresh")).unwrap();
        for bytes in [b"sk-api-key".to_vec(), login(""), vec![b'x';65537], br#"{"auth_mode":"chatgpt","auth_mode":"chatgpt","tokens":{"access_token":"private-value"}}"#.to_vec()] {
            let error=PrivateLaunchInput::validate_chatgpt_auth_json(&bytes).unwrap_err();
            assert!(!format!("{error:#}").contains("private-value"));
        }
    }
    #[test]
    fn subscription_publication_reuses_original_material_pins_and_private_home() {
        let dir = tempfile::tempdir().unwrap();
        let (source, _, _, record) = source(dir.path());
        let bytes = login("offline-refresh");
        std::fs::write(dir.path().join("source/material.bin"), &bytes).unwrap();
        let (scope, _) = scoped();
        let input = PrivateLaunchInput::codex_chatgpt_auth_json(
            source,
            OriginalPrivateMaterial {
                digest: Digest::of(&bytes),
                bytes: bytes.len() as u64,
                record_digest: record,
            },
            scope.binding,
            scope.launch_id,
            scope.deadline_boot_ms,
        )
        .unwrap();
        let root = root(dir.path());
        let publication = Publication::reserve(&root, input).unwrap();
        let record = publication.record();
        assert!(
            !serde_json::to_string(&record)
                .unwrap()
                .contains("offline-access")
        );
        let _held = publication.publish().unwrap();
        assert_eq!(
            std::fs::read(root.join("codex-home/auth.json")).unwrap(),
            bytes
        );
        assert_eq!(
            std::fs::metadata(root.join("codex-home/auth.json"))
                .unwrap()
                .mode()
                & 0o777,
            0o600
        );
        verify_record(&File::open(&root).unwrap(), &record, true).unwrap();
        let replacement = root.join("codex-home/new-auth");
        std::fs::write(&replacement, login("offline-rotated-refresh")).unwrap();
        std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::rename(&replacement, root.join("codex-home/auth.json")).unwrap();
        assert!(verify_record(&File::open(&root).unwrap(), &record, true).is_err());
        let refusal = verify_record(&File::open(&root).unwrap(), &record, false).unwrap_err();
        assert!(!format!("{refusal:#}").contains("offline-rotated-refresh"));
        assert_eq!(
            std::fs::read(root.join("codex-home/auth.json")).unwrap(),
            login("offline-rotated-refresh"),
            "rotated private login must remain available for reconciliation"
        );
        assert_eq!(
            std::fs::read(dir.path().join("source/material.bin")).unwrap(),
            bytes,
            "private refresh never overwrites operator material"
        );
    }
    #[test]
    fn subscription_publication_refuses_changed_original_before_writing() {
        let dir = tempfile::tempdir().unwrap();
        let (source, _, _, record) = source(dir.path());
        let bytes = login("offline-refresh");
        std::fs::write(dir.path().join("source/material.bin"), &bytes).unwrap();
        let (scope, _) = scoped();
        let input = PrivateLaunchInput::codex_chatgpt_auth_json(
            source,
            OriginalPrivateMaterial {
                digest: Digest::of(&bytes),
                bytes: bytes.len() as u64,
                record_digest: record,
            },
            scope.binding,
            scope.launch_id,
            scope.deadline_boot_ms,
        )
        .unwrap();
        let root = root(dir.path());
        let publication = Publication::reserve(&root, input).unwrap();
        std::fs::write(dir.path().join("source/material.bin"), login("changed")).unwrap();
        assert!(publication.publish().is_err());
        assert_eq!(
            std::fs::metadata(root.join("codex-home/auth.json"))
                .unwrap()
                .len(),
            0
        );
    }
    pub(super) fn scoped() -> (ScopedCodexAuthBinding, LaunchManifest) {
        let manifest: LaunchManifest =
            serde_json::from_str(include_str!("../tests/fixtures/manifest.json")).unwrap();
        let binding = aq_session_protocol::LaunchBinding {
            schema_version: 1,
            session: manifest.bindings.session.clone(),
            offer_id: Uuid::new_v4(),
            attempt_id: manifest.bindings.attempt_id,
            assignment_generation: manifest.bindings.assignment_generation,
            execution_spec_digest: Digest::of(b"offline exact execution"),
            launch_manifest_digest: canonical_digest(&manifest).unwrap(),
            graph_access_profile_digest: manifest.bindings.graph_access_profile.digest.clone(),
        };
        (
            ScopedCodexAuthBinding {
                binding,
                launch_id: Uuid::new_v4(),
                origin: "http://127.0.0.1:12345".into(),
                deadline_boot_ms: 10000,
            },
            manifest,
        )
    }
    pub(super) fn source(root: &Path) -> (File, Digest, u64, Digest) {
        let dir = root.join("source");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let key = b"sk-private-offline-only";
        let record = b"{\"schema_version\":1}";
        for (name, bytes) in [
            ("material.bin", key.as_slice()),
            ("record.json", record.as_slice()),
        ] {
            let p = dir.join(name);
            std::fs::write(&p, bytes).unwrap();
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        (
            File::open(dir).unwrap(),
            Digest::of(key),
            key.len() as u64,
            Digest::of(record),
        )
    }
    pub(super) fn root(path: &Path) -> PathBuf {
        let p = path.join("launch");
        std::fs::create_dir(&p).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700)).unwrap();
        p
    }
    #[test]
    fn reserved_metadata_has_no_auth_bytes_and_original_record_is_rechecked() {
        let dir = tempfile::tempdir().unwrap();
        let (fd, digest, bytes, record) = source(dir.path());
        let root = root(dir.path());
        let input = PrivateLaunchInput::codex_api_key(fd, digest, bytes, record).unwrap();
        let publication = Publication::reserve(&root, input).unwrap();
        assert_eq!(
            std::fs::metadata(root.join("codex-home/auth.json"))
                .unwrap()
                .len(),
            0
        );
        let record = publication.record();
        let metadata = serde_json::to_string(&record).unwrap();
        assert!(!metadata.contains("sk-private"));
        let _held = publication.publish().unwrap();
        let root_fd = File::open(&root).unwrap();
        verify_record(&root_fd, &record, true).unwrap();
        std::fs::write(dir.path().join("source/record.json"), b"changed record").unwrap();
        assert!(verify_record(&root_fd, &record, true).is_err());
        // Matching stopped cleanup may still verify the derived file independently.
        verify_record(&root_fd, &record, false).unwrap();
    }
    #[test]
    fn publication_refuses_changed_original_bytes_without_writing_auth() {
        let dir = tempfile::tempdir().unwrap();
        let (fd, digest, bytes, record) = source(dir.path());
        let root = root(dir.path());
        let publication = Publication::reserve(
            &root,
            PrivateLaunchInput::codex_api_key(fd, digest, bytes, record).unwrap(),
        )
        .unwrap();
        std::fs::write(dir.path().join("source/material.bin"), b"changed key").unwrap();
        assert!(publication.publish().is_err());
        assert_eq!(
            std::fs::metadata(root.join("codex-home/auth.json"))
                .unwrap()
                .len(),
            0
        );
    }
    #[test]
    fn symlink_replacement_and_nonprivate_auth_refuse() {
        let dir = tempfile::tempdir().unwrap();
        let (fd, digest, bytes, record) = source(dir.path());
        let root = root(dir.path());
        let publication = Publication::reserve(
            &root,
            PrivateLaunchInput::codex_api_key(fd, digest, bytes, record).unwrap(),
        )
        .unwrap();
        let record = publication.record();
        let _held = publication.publish().unwrap();
        std::fs::set_permissions(
            root.join("codex-home/auth.json"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(verify_record(&File::open(&root).unwrap(), &record, false).is_err());
        std::fs::remove_file(root.join("codex-home/auth.json")).unwrap();
        std::os::unix::fs::symlink(
            dir.path().join("source/material.bin"),
            root.join("codex-home/auth.json"),
        )
        .unwrap();
        assert!(verify_record(&File::open(&root).unwrap(), &record, false).is_err());
        assert_eq!(
            std::fs::read(dir.path().join("source/material.bin")).unwrap(),
            b"sk-private-offline-only"
        );
    }
    #[test]
    fn scoped_auth_contains_only_fresh_capability_and_rechecks_original() {
        let dir = tempfile::tempdir().unwrap();
        let (fd, digest, bytes, record_digest) = source(dir.path());
        let root = root(dir.path());
        let (scope, _) = scoped();
        let cap = Zeroizing::new(format!("aq-responses-{}", "a".repeat(64)).into_bytes());
        let input = PrivateLaunchInput::codex_scoped_capability(
            fd,
            OriginalPrivateMaterial {
                digest,
                bytes,
                record_digest,
            },
            scope,
            cap.clone(),
        )
        .unwrap();
        let publication = Publication::reserve(&root, input).unwrap();
        assert_eq!(
            std::fs::metadata(root.join("codex-home/auth.json"))
                .unwrap()
                .len(),
            0
        );
        let record = publication.record();
        let metadata = serde_json::to_string(&record).unwrap();
        assert!(!metadata.contains("sk-private"));
        assert!(!metadata.contains(std::str::from_utf8(&cap).unwrap()));
        let _source = publication.publish().unwrap();
        let auth = std::fs::read(root.join("codex-home/auth.json")).unwrap();
        assert_eq!(auth, *derive_auth(&cap).unwrap());
        assert!(!String::from_utf8_lossy(&auth).contains("sk-private"));
        verify_record(&File::open(&root).unwrap(), &record, true).unwrap();
        std::fs::write(
            dir.path().join("source/record.json"),
            b"changed original record",
        )
        .unwrap();
        assert!(verify_record(&File::open(&root).unwrap(), &record, true).is_err());
        verify_record(&File::open(&root).unwrap(), &record, false).unwrap();
    }
    #[test]
    fn scoped_auth_requires_exact_launch_manifest_deadline_and_single_route() {
        let (scope, manifest) = scoped();
        let mut config = SealedLaunch {
            launch_id: scope.launch_id,
            permit: ExecutionPermit {
                session: manifest.bindings.session.clone(),
                manifest_digest: canonical_digest(&manifest).unwrap(),
                sequence: 1,
                issued_boot_ms: 1,
                expires_boot_ms: scope.deadline_boot_ms,
            },
            resource_policy: ResourcePolicy::CallerAdmission(manifest.harness.environment.clone()),
            manifest: manifest.into(),
            workspace: "/work".into(),
            helper: "/helper".into(),
            tmux: "/usr/bin/tmux".into(),
            tmux_version: "tmux 3.4".into(),
            resource_qualification: None,
            rendered_startup: None,
            private_input: None,
            observed: None,
            command: LocalCommand {
                executable: "/opt/codex".into(),
                arguments: vec![
                    "--config".into(),
                    format!("openai_base_url=\"{}/v1\"", scope.origin),
                ],
                environment: Default::default(),
            },
        };
        scope.verify_launch(&config).unwrap();
        config.command.arguments.extend([
            "--config".into(),
            format!("openai_base_url=\"{}/v1\"", scope.origin),
        ]);
        assert!(scope.verify_launch(&config).is_err());
        config.command.arguments.truncate(2);
        config.permit.expires_boot_ms += 1;
        assert!(scope.verify_launch(&config).is_err());
        config.permit.expires_boot_ms -= 1;
        config.launch_id = Uuid::new_v4();
        assert!(scope.verify_launch(&config).is_err());
        config.launch_id = scope.launch_id;
        if let LaunchDocument::Task(m) = &mut config.manifest {
            m.model.max_output_tokens += 1;
        }
        assert!(scope.verify_launch(&config).is_err());
        let mut foreign = scope.clone();
        foreign.origin = "https://provider.example".into();
        assert!(foreign.validate().is_err());
    }
}
