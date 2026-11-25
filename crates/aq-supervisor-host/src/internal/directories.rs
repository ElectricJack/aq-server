use super::*;

/// At start, before anything runs (one host owns both directories):
/// removes the run directories, named by a UUID, that a host which stopped
/// mid-run left under `homes` and `scratch`. Anything else there is left
/// alone. A Codex login such a run refreshed is first offered back to the
/// login file ([`recover_login`]); a home whose newer login could not be
/// copied back, or that holds a login while Codex is not configured, is kept.
/// An answer run's login is `<run>/auth.json`, an agent run's
/// `<run>/home/auth.json`, owned by the agent's uid.
pub fn recover_run_dirs(config: &InternalConfig) {
    for parent in [&config.homes, &config.scratch] {
        if private_parent(parent, config.identity).is_err() {
            continue;
        }
        recover_parent(config, parent);
    }
}

fn recover_parent(config: &InternalConfig, parent: &Path) {
    for entry in std::fs::read_dir(parent).into_iter().flatten().flatten() {
        if !is_run_dir(&entry.file_name()) {
            continue;
        }
        let path = entry.path();
        if parent == config.homes && keep_home(config, &path) {
            eprintln!("internal work: kept a run home holding a login not copied back");
            continue;
        }
        if std::fs::remove_dir_all(&path).is_err() {
            let _ = std::fs::remove_file(&path);
        }
    }
}

fn keep_home(config: &InternalConfig, path: &Path) -> bool {
    let logins = [
        (path.join("auth.json"), config.identity),
        (path.join("home").join("auth.json"), config.identity.agent()),
    ];
    logins.iter().any(|(login, identity)| {
        // Without Codex configured there is no login file to check
        // it against, so it is kept too.
        std::fs::symlink_metadata(login).is_ok()
            && !config
                .codex
                .as_ref()
                .is_some_and(|codex| recover_login(&codex.secret_file, login, *identity))
    })
}

/// Whether `name` is a run directory's: a UUID in its hyphenated form.
fn is_run_dir(name: &std::ffi::OsStr) -> bool {
    let name = name.to_str().unwrap_or_default();
    uuid::Uuid::try_parse(name).is_ok_and(|id| id.hyphenated().to_string() == name)
}

/// Where a run happens. For an answer run `home` is `root`. For agent work
/// `root` stays root-owned (0711) and holds what configures the harness,
/// which the agent cannot change: Claude's settings and `bin`, the shims of
/// the executables its shell may run. `home` is `root/home`, the agent's own.
pub(crate) struct Place {
    pub(crate) root: PathBuf,
    pub(crate) home: PathBuf,
    pub(crate) cwd: PathBuf,
}

impl Place {
    /// The `PATH` of a run: an agent's shims come first.
    pub(crate) fn path(&self, agent: bool) -> String {
        let system = "/usr/local/bin:/usr/bin:/bin";
        match agent {
            true => format!("{}:{system}", self.root.join("bin").display()),
            false => system.to_owned(),
        }
    }

    /// The temporary directory of agent work, inside its workspace, so it
    /// goes with the workspace and is shared with no other run.
    pub(crate) fn tmp(&self) -> PathBuf {
        self.cwd.join(agent::TMP)
    }
}

/// Checks the run directories' parents and the native of `h`, makes a fresh
/// private home under `homes` and an empty working directory under `scratch`
/// (owned by the native's user), writes agent work's files into the
/// directory, and runs `attempt` with the verified executable, the home and
/// the directory. Both are removed afterwards, whatever the outcome.
pub(super) async fn in_run_dirs<T>(
    h: &Harness,
    run: Run<'_>,
    attempt: impl AsyncFnOnce(&std::fs::File, &Place) -> Result<T, Failure>,
) -> Result<T, Failure> {
    let (homes, scratch) = (&run.config.homes, &run.config.scratch);
    let identity = run.identity;
    for parent in [homes, scratch] {
        private_parent(parent, identity)
            .map_err(|_| failure(Reason::ProviderError, "internal work directories rejected"))?;
    }
    let executable = h
        .verified_native(identity)
        .map_err(|_| failure(Reason::ProviderUnavailable, "native executable refused"))?;
    let name = uuid::Uuid::new_v4().to_string();
    let (root, cwd) = (homes.join(&name), scratch.join(&name));
    let made = run_dirs(&root, &cwd)
        .map_err(|_| failure(Reason::ProviderError, "native run could not be prepared"));
    // Removed however this ends, a panic included.
    let _dirs = RunDirs([&root, &cwd]);
    let place = made.and_then(|()| {
        let work = run.claimed.request.agent.as_ref();
        let prepared = match work {
            Some(work) => agent::prepare(&root, &cwd, work, run),
            None => answer_place(&root, &cwd, identity),
        };
        prepared.map_err(|_| match work {
            Some(_) => failure(
                Reason::ProviderError,
                "agent workspace could not be prepared",
            ),
            None => failure(Reason::ProviderError, "native run could not be prepared"),
        })
    });
    match place {
        Ok(place) => attempt(&executable, &place).await,
        Err(failure) => Err(failure),
    }
}

/// An answer run's place: its home is the run's root, and its empty working
/// directory belongs to the native's user.
fn answer_place(root: &Path, cwd: &Path, identity: Identity) -> std::io::Result<Place> {
    if identity.drop_privileges {
        let id = Some(identity.run_as);
        std::os::unix::fs::chown(cwd, id, id)?;
    }
    Ok(Place {
        root: root.to_owned(),
        home: root.to_owned(),
        cwd: cwd.to_owned(),
    })
}

/// A run's home and working directory, removed on drop.
struct RunDirs<'a>([&'a Path; 2]);

impl Drop for RunDirs<'_> {
    fn drop(&mut self) {
        for dir in self.0 {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Creates `root` and `cwd` owner-only, owned by the host until the run's
/// place is prepared.
fn run_dirs(root: &Path, cwd: &Path) -> std::io::Result<()> {
    for dir in [root, cwd] {
        std::fs::create_dir(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// `dir` is a canonical directory owned by the executable owner and not
/// writable by group or others, so nobody else can plant a run directory.
fn private_parent(dir: &Path, identity: Identity) -> anyhow::Result<()> {
    let meta = std::fs::symlink_metadata(dir)?;
    anyhow::ensure!(
        meta.is_dir()
            && meta.uid() == identity.executable_owner
            && meta.mode() & 0o022 == 0
            && dir.canonicalize()? == dir,
        "internal work directory ownership"
    );
    Ok(())
}
