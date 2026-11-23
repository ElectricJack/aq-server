use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InternalConfig {
    pub database_url: String,
    /// Parent of the per-run native homes; root-owned, not group or world writable.
    pub homes: PathBuf,
    /// Parent of the per-run empty working directories; same ownership as `homes`.
    pub scratch: PathBuf,
    pub poll_ms: u64,
    pub codex: Option<Harness>,
    pub claude: Option<Harness>,
    #[serde(skip)]
    pub(super) identity: Identity,
    /// Where the executables an agent's shell may run are installed, off
    /// `PATH`; the agent reaches each only through its shim in the run directory.
    #[serde(skip, default = "libexec")]
    pub(super) libexec: PathBuf,
}

fn libexec() -> PathBuf {
    PathBuf::from("/usr/local/libexec")
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Harness {
    pub native: PathBuf,
    pub native_digest: Digest,
    /// The login file. Codex: an `auth.json`; each copy in and back holds a
    /// `flock` on its directory, which needs to be writable only for a
    /// refresh to be written back. Claude: the one-line setup token, read
    /// only and passed to the native in its environment.
    pub secret_file: PathBuf,
    /// The native's file when its digest last matched.
    #[serde(skip)]
    verified: std::sync::Mutex<Option<Stamp>>,
    /// Whether the Codex sandbox started when last probed, and for which
    /// native file ([`Harness::sandbox_starts`]).
    #[serde(skip)]
    sandbox: std::sync::Mutex<Option<(Stamp, bool)>>,
    /// How many of the next runs panic once the native has run (tests only).
    #[cfg(feature = "test-support")]
    #[serde(skip)]
    pub(crate) panics: std::sync::atomic::AtomicUsize,
}

/// What shows a change to a file's content without reading it: its device,
/// inode, size, mtime and ctime. Every write moves the ctime, and no one can
/// set it back.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Stamp {
    dev: u64,
    ino: u64,
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl Stamp {
    fn of(m: &std::fs::Metadata) -> Self {
        Stamp {
            dev: m.dev(),
            ino: m.ino(),
            size: m.size(),
            mtime: (m.mtime(), m.mtime_nsec()),
            ctime: (m.ctime(), m.ctime_nsec()),
        }
    }
}

impl Harness {
    /// The native, opened for exec through its descriptor and checked
    /// against `native_digest`. It is hashed when the config is loaded and
    /// again only when its [`Stamp`] differs from the last match.
    pub(super) fn verified_native(&self, identity: Identity) -> Result<std::fs::File> {
        let executable = open_native(&self.native, identity)?;
        let stamp = Stamp::of(&executable.metadata()?);
        let verified = || self.verified.lock().unwrap_or_else(|p| p.into_inner());
        // Not held while hashing, so another run is not blocked meanwhile.
        let known = *verified();
        if known != Some(stamp) {
            let matches = file_digest(&executable)? == self.native_digest.as_str();
            *verified() = matches.then_some(stamp);
            ensure!(matches, "native digest mismatch");
        }
        Ok(executable)
    }
}

/// How long the Codex sandbox probe may take.
const SANDBOX_PROBE: Duration = Duration::from_secs(10);

impl Harness {
    /// Whether this host can start the sandbox Codex agent work runs in:
    /// one sandboxed no-op (`codex sandbox`, workspace-write, no network)
    /// as the agent's user in the run's workspace, which needs no login.
    /// The answer is kept until the native's file changes, so a host whose
    /// sandbox cannot start (no user namespaces in a hardened container,
    /// say) fails a Codex agent candidate at once instead of after its
    /// budget. Never a reason to run with a weaker sandbox.
    pub(crate) async fn sandbox_starts(
        &self,
        executable: &std::fs::File,
        place: &Place,
        identity: Identity,
    ) -> bool {
        let stamp = executable.metadata().ok().map(|m| Stamp::of(&m));
        let known = *self.sandbox.lock().unwrap_or_else(|p| p.into_inner());
        if let (Some(stamp), Some((at, starts))) = (stamp, known)
            && at == stamp
        {
            return starts;
        }
        let probe = place.tmp().join("sandbox-probe");
        let write = format!("echo ok > '{}'", probe.display());
        let args = [
            "sandbox",
            "-c",
            "sandbox_mode=\"workspace-write\"",
            "-c",
            "sandbox_workspace_write.network_access=false",
            "-c",
            "sandbox_workspace_write.exclude_slash_tmp=true",
            "--",
            "/bin/sh",
            "-c",
            &write,
        ];
        let args = args.iter().map(|a| a.to_string()).collect();
        let mut command = crate::subscription::native_command(
            executable,
            args,
            &place.home,
            &place.cwd,
            identity,
        );
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let mut command = tokio::process::Command::from(command);
        command.kill_on_drop(true);
        let ran = tokio::time::timeout(SANDBOX_PROBE, command.status()).await;
        let starts = matches!(ran, Ok(Ok(status)) if status.success()) && probe.exists();
        let _ = std::fs::remove_file(&probe);
        if let Some(stamp) = stamp {
            *self.sandbox.lock().unwrap_or_else(|p| p.into_inner()) = Some((stamp, starts));
        }
        starts
    }
}

impl InternalConfig {
    /// Reads `path` with the checks `Config::load` applies: an absolute,
    /// canonical, owner-only regular file of at most 64 KiB, no unknown
    /// fields, absolute installed paths, and no login file under a directory
    /// the native child is given. Returns the file's digest too. Each native
    /// is hashed here once; a native that does not match fails its runs.
    pub fn load(path: &Path) -> Result<(Self, Digest)> {
        let bytes = protected_read(path, 65536)?;
        let config: Self = serde_json::from_slice(&bytes)?;
        ensure!(
            (1..=60_000).contains(&config.poll_ms),
            "poll_ms must be between 1 and 60000"
        );
        let harnesses = [&config.codex, &config.claude];
        let mut paths = vec![&config.homes, &config.scratch];
        for h in harnesses.iter().copied().flatten() {
            paths.extend([&h.native, &h.secret_file]);
        }
        ensure!(
            paths.iter().all(|p| p.is_absolute()),
            "absolute installed paths required"
        );
        ensure!(
            harnesses.iter().copied().flatten().all(|h| {
                !h.secret_file.starts_with(&config.homes)
                    && !h.secret_file.starts_with(&config.scratch)
            }),
            "login files overlap the native directories"
        );
        for h in harnesses.iter().copied().flatten() {
            if h.verified_native(config.identity).is_err() {
                eprintln!("internal work: a native executable was refused");
            }
        }
        Ok((config, Digest::of(&bytes)))
    }

    /// Runs the native as the current user, without the privilege drop or
    /// chown, so tests that are not root can drive a fake native.
    #[cfg(feature = "test-support")]
    pub fn for_tests(mut self) -> Self {
        self.identity = Identity::current();
        self
    }

    /// Takes the executables an agent's shell may run from `dir`.
    #[cfg(feature = "test-support")]
    pub fn with_libexec(mut self, dir: PathBuf) -> Self {
        self.libexec = dir;
        self
    }

    /// Makes the next `runs` Codex runs panic once the native has run, before
    /// anything is copied back.
    #[cfg(feature = "test-support")]
    pub fn panic_in_codex_runs(self, runs: usize) -> Self {
        if let Some(codex) = &self.codex {
            codex
                .panics
                .store(runs, std::sync::atomic::Ordering::SeqCst);
        }
        self
    }
}
