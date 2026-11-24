//! Persist the release settings Compose uses.

use super::release_image::digest_ref;
use super::{RuntimeResult, failed, fsync_parent};
use std::path::Path;

pub const RELEASE_ENV: &str = "release.env";

/// The `AQ_SERVER_IMAGE` in `release.env` while no release is installed:
/// digest-pinned (so it is a valid value) on a reserved `.invalid` host, so
/// Compose can interpolate `compose.yaml` for postgres, psql and pg_dump on
/// a fresh host, while starting the server with it can only fail.
pub const NO_RELEASE_IMAGE: &str = "no-release-installed.invalid/agent-q-server@sha256:0000000000000000000000000000000000000000000000000000000000000000";

fn env_path_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^/[A-Za-z0-9._/+@:-]*$").expect("static"))
}

/// Can `path` be written unquoted as a Compose env-file value? Absolute, and
/// only characters that need no quoting and cannot interpolate (`$`), start
/// a comment (`#`) or add a line.
pub fn is_env_safe_path(path: &Path) -> bool {
    path.to_str().is_some_and(|p| env_path_re().is_match(p))
}

/// The full `release.env`: the server image (digest-pinned, which also rules
/// out anything that could add another variable) and the spool the updater
/// uses, so the server's spool mounts always match it.
fn release_env_body(image: &str, spool: &Path) -> RuntimeResult<String> {
    let image = digest_ref(image)?;
    if !is_env_safe_path(spool) {
        return Err(failed(format!(
            "refusing spool path {:.200?} for release.env: it must be absolute and use only A-Z a-z 0-9 . _ / + @ : -",
            spool
        )));
    }
    Ok(format!(
        "AQ_SERVER_IMAGE={image}\nAQ_UPDATER_SPOOL={}\n",
        spool.display()
    ))
}

/// Write `release.env` (`AQ_SERVER_IMAGE=<image>`, `AQ_UPDATER_SPOOL=<spool>`)
/// to `path` atomically (temp file, fsync, rename, fsync dir), mode 0640.
pub fn write_release_env(path: &Path, image: &str, spool: &Path) -> RuntimeResult<()> {
    write_env_file(path, release_env_body(image, spool)?.as_bytes())
}

fn write_env_file(path: &Path, body: &[u8]) -> RuntimeResult<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let err = |e: std::io::Error| failed(format!("cannot write {}: {e}", path.display()));
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o640)
        .open(&tmp)
        .map_err(err)?;
    f.set_permissions(std::fs::Permissions::from_mode(0o640))
        .map_err(err)?;
    f.write_all(body).map_err(err)?;
    f.sync_all().map_err(err)?;
    drop(f);
    std::fs::rename(&tmp, path).map_err(err)?;
    fsync_parent(path).map_err(err)
}

/// The digest-pinned `AQ_SERVER_IMAGE` an existing `release.env` names.
fn release_env_image(text: &str) -> Option<&str> {
    text.lines()
        .find_map(|l| l.strip_prefix("AQ_SERVER_IMAGE="))
        .filter(|i| aq_release::is_digest_ref(i))
}

/// Make sure `release.env` is complete before Compose reads it. The image
/// it already names is kept (it is the one last started); otherwise `image`
/// (the active release) is used, else [`NO_RELEASE_IMAGE`]. The spool line
/// is always `spool`. A file that is already right is not rewritten.
pub fn ensure_release_env(path: &Path, image: Option<&str>, spool: &Path) -> RuntimeResult<()> {
    let existing = match std::fs::read_to_string(path) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(failed(format!("cannot read {}: {e}", path.display()))),
    };
    let kept = existing
        .as_deref()
        .and_then(release_env_image)
        // A placeholder gives way to a known active release.
        .filter(|i| *i != NO_RELEASE_IMAGE || image.is_none());
    let body = release_env_body(kept.or(image).unwrap_or(NO_RELEASE_IMAGE), spool)?;
    if existing.as_deref() == Some(body.as_str()) {
        return Ok(());
    }
    write_env_file(path, body.as_bytes())
}
