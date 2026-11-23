//! Private file bounds and the pinned local PostgreSQL socket directory.
use super::*;
pub(super) fn private(path: &Path, max: u64) -> Result<Zeroizing<Vec<u8>>> {
    if !path.is_absolute() {
        return Err("playbook private file must be absolute");
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| "playbook private file unavailable")?;
    let m = file
        .metadata()
        .map_err(|_| "playbook private file unavailable")?;
    let owner_only = m.mode() & 0o077 == 0 && m.uid() == nix::unistd::geteuid().as_raw();
    let single_file = m.is_file() && m.nlink() == 1;
    if ![owner_only, single_file, (1..=max).contains(&m.len())]
        .into_iter()
        .all(|v| v)
    {
        return Err("unsafe playbook private file");
    }
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(max + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "playbook private file read failed")?;
    if bytes.len() as u64 > max {
        return Err("playbook private file bound");
    }
    Ok(bytes)
}
pub(super) struct LocalSocket {
    directory: File,
    directory_path: PathBuf,
    owner_uid: u32,
    port: u16,
}
impl LocalSocket {
    pub(super) fn connect(file: &Path, owner_uid: u32) -> Result<(pg::Config, Self)> {
        let dsn = private(file, 4096)?;
        let parsed: pg::Config = std::str::from_utf8(&dsn)
            .map_err(|_| "invalid playbook connection file")?
            .trim()
            .parse()
            .map_err(|_| "invalid playbook connection file")?;
        let (directory_path, port) = endpoint(&parsed)?;
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
            .open(&directory_path)
            .map_err(|_| "playbook socket directory unavailable")?;
        let m = directory
            .metadata()
            .map_err(|_| "playbook socket directory unavailable")?;
        let trusted_directory = m.is_dir() && m.mode() & 0o022 == 0;
        if !trusted_directory || ![0, owner_uid].contains(&m.uid()) {
            return Err("untrusted playbook socket directory");
        }

        let mut connection = pg::Config::new();
        connection
            .host_path(format!("/proc/self/fd/{}", directory.as_raw_fd()))
            .port(port)
            .user(parsed.get_user().unwrap())
            .dbname(parsed.get_dbname().unwrap())
            .ssl_mode(SslMode::Disable)
            .connect_timeout(Duration::from_millis(1000))
            .application_name("aq-playbook-owner");
        if let Some(password) = parsed.get_password() {
            connection.password(password);
        }
        Ok((
            connection,
            Self {
                directory,
                directory_path,
                owner_uid,
                port,
            },
        ))
    }
}
impl LocalSocket {
    pub(super) fn check(&self) -> Result<()> {
        let held = self
            .directory
            .metadata()
            .map_err(|_| "playbook socket directory unavailable")?;
        let current = std::fs::symlink_metadata(&self.directory_path)
            .map_err(|_| "playbook socket directory unavailable")?;
        let same_inode = (current.dev(), current.ino()) == (held.dev(), held.ino());
        let trusted_directory = current.is_dir() && current.mode() & 0o022 == 0;
        if !same_inode || !trusted_directory {
            return Err("playbook socket directory changed");
        }
        let socket = std::fs::symlink_metadata(format!(
            "/proc/self/fd/{}/.s.PGSQL.{}",
            self.directory.as_raw_fd(),
            self.port
        ))
        .map_err(|_| "playbook socket unavailable")?;
        if !socket.file_type().is_socket() || socket.uid() != self.owner_uid {
            return Err("untrusted playbook socket");
        }
        Ok(())
    }
}

fn endpoint(parsed: &pg::Config) -> Result<(PathBuf, u16)> {
    let directory_path = match parsed.get_hosts() {
        [Host::Unix(p)] if p.is_absolute() => p.clone(),
        _ => return Err("playbook owner requires one Unix socket endpoint"),
    };
    let port = match parsed.get_ports() {
        [port] if *port != 0 => *port,
        _ => return Err("invalid local playbook connection policy"),
    };
    if !local_policy(parsed) {
        return Err("invalid local playbook connection policy");
    }
    Ok((directory_path, port))
}

fn local_policy(parsed: &pg::Config) -> bool {
    let unix_only = parsed.get_ssl_mode() == SslMode::Disable
        && parsed.get_hostaddrs().is_empty()
        && parsed.get_options().is_none();
    let names = [parsed.get_user(), parsed.get_dbname()]
        .into_iter()
        .all(|name| name.is_some_and(|value| (1..=128).contains(&value.len())));
    unix_only && names
}
