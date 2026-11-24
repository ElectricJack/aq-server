//! The spool shared with the server (`<state_dir>/spool`):
//! - `requests/` (mode 2770, group 10001): the server writes one
//!   `<uuid>.json` per update request (via a `.tmp-*` dotfile and rename);
//!   the updater reads and deletes them.
//! - `status/` (mode 0755, mounted read-only into the server): the updater
//!   publishes `status.json` and the `maintenance` marker.

use crate::engine::{Engine, Halted, MaintenanceSink, UPDATE_IN_PROGRESS};
use crate::runtime::{Runtime, RuntimeError};
use crate::verify::ReleaseVerifier;
use aq_release::{HistoryEntry, Outcome, UpdateRequest, UpdateStatus};
use semver::Version;
use std::fs::{File, OpenOptions, Permissions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use uuid::Uuid;

/// Largest request file accepted, in bytes.
pub const REQUEST_LIMIT: u64 = 4096;
/// Group of the server container's user; it owns `requests/`.
pub const REQUESTS_GID: u32 = 10001;
pub const REQUESTS_MODE: u32 = 0o2770;
pub const STATUS_MODE: u32 = 0o755;

pub fn spool_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("spool")
}
pub fn requests_dir(state_dir: &Path) -> PathBuf {
    spool_dir(state_dir).join("requests")
}
pub fn status_dir(state_dir: &Path) -> PathBuf {
    spool_dir(state_dir).join("status")
}

fn fsync_dir(dir: &Path) -> std::io::Result<()> {
    File::open(dir)?.sync_all()
}

/// Create the spool directories with their modes (set explicitly, so the
/// umask does not matter). `requests/` is given group 10001 only when running
/// as root; otherwise its group is left as is.
pub fn ensure_dirs(state_dir: &Path) -> std::io::Result<()> {
    let spool = spool_dir(state_dir);
    let requests = requests_dir(state_dir);
    let status = status_dir(state_dir);
    for d in [&spool, &requests, &status] {
        std::fs::create_dir_all(d)?;
    }
    std::fs::set_permissions(&spool, Permissions::from_mode(0o755))?;
    std::fs::set_permissions(&status, Permissions::from_mode(STATUS_MODE))?;
    if nix::unistd::geteuid().is_root() {
        nix::unistd::chown(
            &requests,
            None,
            Some(nix::unistd::Gid::from_raw(REQUESTS_GID)),
        )
        .map_err(std::io::Error::from)?;
    } else {
        tracing::info!(
            dir = %requests.display(),
            "not running as root: leaving the group of the requests directory as is (the server needs group {REQUESTS_GID})"
        );
    }
    // After chown, which may clear the setgid bit.
    std::fs::set_permissions(&requests, Permissions::from_mode(REQUESTS_MODE))?;
    Ok(())
}

/// A request file that was read and deleted but not accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpoolReject {
    /// File name within `requests/`.
    pub file: String,
    /// Taken from the file name (`<uuid>.json`) for an oversized file; `None`
    /// when the content could not be parsed.
    pub request_id: Option<Uuid>,
    pub reason: String,
}

impl std::fmt::Display for SpoolReject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "request file {:?} rejected: {}", self.file, self.reason)
    }
}

enum Taken {
    Request(UpdateRequest),
    Reject(String, Option<Uuid>),
    /// A directory, symlink, FIFO or other non-regular entry.
    NotAFile,
}

/// The id a `<uuid>.json` file name carries.
fn id_from_name(name: &std::ffi::OsStr) -> Option<Uuid> {
    name.to_str()?
        .strip_suffix(".json")
        .and_then(|s| Uuid::parse_str(s).ok())
}

fn read_one(path: &Path, id_from_name: Option<Uuid>) -> std::io::Result<Taken> {
    // O_NOFOLLOW: a symlink is never followed; O_NONBLOCK: a FIFO cannot hang
    // the open. The type is checked on the opened file.
    let f = match OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.raw_os_error() == Some(nix::libc::ELOOP) => return Ok(Taken::NotAFile),
        Err(e) => return Err(e),
    };
    if !f.metadata()?.is_file() {
        return Ok(Taken::NotAFile);
    }
    let mut buf = Vec::new();
    f.take(REQUEST_LIMIT + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > REQUEST_LIMIT {
        return Ok(Taken::Reject(
            format!("larger than {REQUEST_LIMIT} bytes"),
            id_from_name,
        ));
    }
    Ok(match serde_json::from_slice::<UpdateRequest>(&buf) {
        Ok(r) => Taken::Request(r),
        // Debug-escaped: the error can quote the file's content.
        Err(e) => Taken::Reject(
            format!("not a valid update request: {:?}", e.to_string()),
            id_from_name,
        ),
    })
}

/// Request entries in `dir`, oldest first (by mtime, then name). Dotfiles
/// (the server's temp files and set-aside rejects) are skipped.
fn pending(dir: &Path) -> Vec<std::ffi::OsString> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => {
            tracing::warn!(dir = %dir.display(), error = %e, "cannot list update requests");
            return Vec::new();
        }
    };
    let mut files: Vec<(SystemTime, std::ffi::OsString)> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name();
            if name.as_encoded_bytes().starts_with(b".") {
                return None;
            }
            let mtime = e
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            Some((mtime, name))
        })
        .collect();
    files.sort();
    files.into_iter().map(|(_, n)| n).collect()
}

/// Entries this process could not remove or set aside. Each is logged (and,
/// for a set-aside failure, reported) once, then left alone.
static STUCK: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

/// Record `path` as stuck; true the first time (log and report it then).
fn first_time_stuck(path: &Path, what: &str, e: &std::io::Error) -> bool {
    let mut stuck = STUCK.lock().unwrap_or_else(|p| p.into_inner());
    if stuck.iter().any(|p| p == path) {
        return false;
    }
    tracing::error!(path = ?path, error = %e, "{what}; ignoring it");
    stuck.push(path.to_path_buf());
    true
}

/// Set a non-file entry aside as `.rejected-<name>` (ignored from then on).
/// If even that fails, it is reported once for this process and then left.
fn set_aside(dir: &Path, name: &std::ffi::OsStr) -> bool {
    let path = dir.join(name);
    let mut aside = std::ffi::OsString::from(".rejected-");
    aside.push(name);
    match std::fs::rename(&path, dir.join(aside)) {
        Ok(()) => true,
        Err(e) => first_time_stuck(&path, "cannot set aside a rejected request entry", &e),
    }
}

fn remove_request(path: &Path) -> bool {
    match std::fs::remove_file(path) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            first_time_stuck(path, "cannot delete update request", &e);
            false
        }
    }
}

fn non_file_reason(path: &Path, name: &std::ffi::OsStr) -> std::io::Result<Option<&'static str>> {
    let meta = std::fs::symlink_metadata(path)?;
    if name.to_str().is_none() {
        Ok(Some("the file name is not UTF-8"))
    } else if !meta.file_type().is_file() {
        Ok(Some("not a regular file"))
    } else {
        Ok(None)
    }
}

/// Read and remove one request entry. `None`: nothing to report (it is gone,
/// could not be removed, or was already reported).
fn take_one(dir: &Path, name: &std::ffi::OsStr) -> Option<Result<UpdateRequest, SpoolReject>> {
    let path = dir.join(name);
    let id = id_from_name(name);
    let reject = |reason: &str| SpoolReject {
        file: name.to_string_lossy().into_owned(),
        request_id: id,
        reason: reason.to_string(),
    };
    let odd = match non_file_reason(&path, name) {
        Ok(reason) => reason,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            first_time_stuck(&path, "cannot inspect update request entry", &e);
            return None;
        }
    };
    // Re-checked on the opened file, in case the entry was swapped.
    let read = match odd {
        Some(_) => Ok(Taken::NotAFile),
        None => read_one(&path, id),
    };
    if let Ok(Taken::NotAFile) = read {
        return set_aside(dir, name).then(|| Err(reject(odd.unwrap_or("not a regular file"))));
    }
    if !remove_request(&path) {
        return None;
    }
    Some(match read {
        Ok(Taken::Request(r)) => Ok(r),
        Ok(Taken::Reject(reason, request_id)) => Err(SpoolReject {
            request_id,
            ..reject(&reason)
        }),
        Ok(Taken::NotAFile) => Err(reject("not a regular file")),
        Err(e) => Err(reject(&format!("cannot be read: {e}"))),
    })
}

/// Read and remove every request in `dir`, oldest first (by mtime, then
/// name). Dotfiles are skipped. Regular files are deleted; anything else
/// (directories, symlinks, FIFOs, names that are not UTF-8) is rejected once
/// and set aside as `.rejected-<name>`. An entry that cannot be removed is
/// skipped and logged, so it is never acted on twice.
///
/// Test only: `process_requests` takes entries one at a time with the same
/// `take_one`, so these tests cover its intake.
#[cfg(test)]
fn take_requests(dir: &Path) -> Vec<Result<UpdateRequest, SpoolReject>> {
    let out: Vec<_> = pending(dir)
        .iter()
        .filter_map(|n| take_one(dir, n))
        .collect();
    sync_requests(dir, !out.is_empty());
    out
}

fn sync_requests(dir: &Path, changed: bool) {
    if changed && let Err(e) = fsync_dir(dir) {
        tracing::warn!(dir = %dir.display(), error = %e, "cannot fsync the requests directory");
    }
}

/// Write `status.json` atomically (temp file, fsync, rename, fsync dir),
/// mode 0644: the server reads it through a read-only mount.
pub fn publish_status(dir: &Path, status: &UpdateStatus) -> std::io::Result<()> {
    let tmp = dir.join(".status.json.tmp");
    let bytes = serde_json::to_vec_pretty(status).map_err(std::io::Error::other)?;
    let mut f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o644)
        .open(&tmp)?;
    f.set_permissions(Permissions::from_mode(0o644))?;
    f.write_all(&bytes)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, dir.join("status.json"))?;
    fsync_dir(dir)
}

/// The last published status, if there is a readable one.
pub fn read_status(dir: &Path) -> Option<UpdateStatus> {
    let bytes = std::fs::read(dir.join("status.json")).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// `MaintenanceSink` backed by the `status/maintenance` marker file.
#[derive(Debug, Clone)]
pub struct SpoolMaintenance {
    dir: PathBuf,
}

impl SpoolMaintenance {
    /// `status_dir`: the spool's `status/` directory.
    pub fn new(status_dir: &Path) -> Self {
        SpoolMaintenance {
            dir: status_dir.to_path_buf(),
        }
    }

    pub fn marker(&self) -> PathBuf {
        self.dir.join("maintenance")
    }
}

fn failed(what: &str, path: &Path, e: std::io::Error) -> RuntimeError {
    RuntimeError::Failed(format!("cannot {what} {}: {e}", path.display()))
}

impl MaintenanceSink for SpoolMaintenance {
    fn enter(&self) -> Result<(), RuntimeError> {
        let marker = self.marker();
        let f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o644)
            .open(&marker)
            .map_err(|e| failed("create", &marker, e))?;
        f.set_permissions(Permissions::from_mode(0o644))
            .and_then(|()| f.sync_all())
            .map_err(|e| failed("create", &marker, e))?;
        fsync_dir(&self.dir).map_err(|e| failed("fsync", &self.dir, e))
    }

    fn leave(&self) -> Result<(), RuntimeError> {
        let marker = self.marker();
        match std::fs::remove_file(&marker) {
            Ok(()) => fsync_dir(&self.dir).map_err(|e| failed("fsync", &self.dir, e)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(failed("remove", &marker, e)),
        }
    }
}

/// What the last install in a pass asked for and how it ended.
struct Finished {
    request_id: Uuid,
    asked: Option<Version>,
    got: Option<Version>,
    outcome: Outcome,
}

struct RequestPass<'a> {
    dir: &'a Path,
    history: Vec<HistoryEntry>,
    last: Option<Finished>,
    // Entries that vanish or get stuck must not be retried in a tight loop.
    attempted: std::collections::HashSet<std::ffi::OsString>,
}

/// Install pending requests one at a time until the spool is empty or
/// `stop` is set. `stop` is checked before each request is taken, so once it
/// is set no new install starts and untaken requests stay on disk.
///
/// Malformed entries are recorded as `Rejected`. After an install finishes,
/// a queued request that repeats it (the same version, or the channel when
/// the channel offers nothing newer than what was just installed) is
/// recorded as `Rejected` instead of being installed: `update_in_progress`
/// after a success, `duplicate of request <id>, just processed (<outcome>)`
/// otherwise. Anything else is installed in turn. Requests written while an
/// install runs are picked up by the next round of this loop.
pub async fn process_requests<R: Runtime, V: ReleaseVerifier>(
    engine: &mut Engine<R, V>,
    dir: &Path,
    stop: &tokio::sync::watch::Receiver<bool>,
) -> Result<Vec<HistoryEntry>, Halted> {
    let mut pass = RequestPass {
        dir,
        history: Vec::new(),
        last: None,
        attempted: std::collections::HashSet::new(),
    };
    loop {
        let names: Vec<_> = pending(dir)
            .into_iter()
            .filter(|n| !pass.attempted.contains(n))
            .collect();
        if names.is_empty() || *stop.borrow() {
            return Ok(pass.history);
        }
        if !pass.process_batch(engine, stop, names).await? {
            return Ok(pass.history);
        }
    }
}

impl RequestPass<'_> {
    async fn process_batch<R: Runtime, V: ReleaseVerifier>(
        &mut self,
        engine: &mut Engine<R, V>,
        stop: &tokio::sync::watch::Receiver<bool>,
        names: Vec<std::ffi::OsString>,
    ) -> Result<bool, Halted> {
        let previous_count = self.history.len();
        for name in names {
            if *stop.borrow() {
                tracing::info!(
                    "shutdown requested; leaving queued update requests for the next start"
                );
                return Ok(false);
            }
            self.attempted.insert(name.clone());
            if let Some(item) = take_one(self.dir, &name) {
                sync_requests(self.dir, true);
                self.history
                    .push(handle_request(engine, item, &mut self.last).await?);
            }
        }
        Ok(self.history.len() > previous_count)
    }
}

async fn handle_request<R: Runtime, V: ReleaseVerifier>(
    engine: &mut Engine<R, V>,
    item: Result<UpdateRequest, SpoolReject>,
    last: &mut Option<Finished>,
) -> Result<HistoryEntry, Halted> {
    let req = match item {
        Ok(r) => r,
        Err(rej) => {
            tracing::warn!(file = ?rej.file, reason = ?rej.reason, "rejected update request");
            return engine.reject(rej.request_id, None, rej.to_string());
        }
    };
    if let Some(prev) = last.as_ref()
        && repeats(engine, &req, &prev.asked, prev.got.as_ref()).await
    {
        let detail = prev.duplicate_detail();
        tracing::info!(request_id = %req.id, %detail, "repeat of the update just finished; rejected");
        return engine.reject(Some(req.id), req.version, detail);
    }
    tracing::info!(request_id = %req.id, version = ?req.version, by = ?req.requested_by, "installing");
    let asked = req.version.clone();
    let request_id = req.id;
    let entry = engine.install(req).await?;
    tracing::info!(outcome = ?entry.outcome, detail = ?entry.detail, "update finished");
    *last = Some(Finished {
        request_id,
        asked,
        got: entry.to.clone(),
        outcome: entry.outcome,
    });
    Ok(entry)
}

impl Finished {
    fn duplicate_detail(&self) -> String {
        if self.outcome == Outcome::Succeeded {
            UPDATE_IN_PROGRESS.to_string()
        } else {
            format!(
                "duplicate of request {}, just processed ({})",
                self.request_id,
                outcome_name(self.outcome)
            )
        }
    }
}

/// `Outcome` as it is serialized (`rolled_back`, ...).
fn outcome_name(o: Outcome) -> String {
    serde_json::to_value(o)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{o:?}"))
}

/// Does `req` ask for what the install that just finished already did?
async fn repeats<R: Runtime, V: ReleaseVerifier>(
    engine: &mut Engine<R, V>,
    req: &UpdateRequest,
    asked: &Option<Version>,
    got: Option<&Version>,
) -> bool {
    if req.version == *asked {
        return true;
    }
    match (&req.version, got) {
        (Some(v), Some(g)) => v == g,
        (Some(_), None) => false,
        // The channel: a repeat unless it now offers something newer. If the
        // channel cannot be read, install normally (which records why).
        (None, Some(g)) => match engine.fetch_channel().await {
            Ok(m) => m.version <= *g,
            Err(e) => {
                tracing::warn!(error = %e, "cannot read the channel to compare a queued request");
                false
            }
        },
        (None, None) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::time::Duration;

    fn request(version: Option<&str>) -> UpdateRequest {
        UpdateRequest {
            id: Uuid::new_v4(),
            version: version.map(|v| v.parse().unwrap()),
            requested_by: "owner@example.com".into(),
            requested_at: Utc::now(),
        }
    }

    /// Write a request the way the server does, with an explicit mtime.
    fn drop_request(dir: &Path, r: &UpdateRequest, age_secs: u64) {
        let path = dir.join(format!("{}.json", r.id));
        std::fs::write(&path, serde_json::to_vec(r).unwrap()).unwrap();
        set_age(&path, age_secs);
    }

    fn set_age(path: &Path, age_secs: u64) {
        let f = std::fs::File::options().write(true).open(path).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(age_secs))
            .unwrap();
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn take_requests_rejects_oversized_and_malformed() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path();
        let good = request(Some("1.2.3"));
        drop_request(dir, &good, 30);

        // Oversized: the id still comes from the file name.
        let big_id = Uuid::new_v4();
        let big = dir.join(format!("{big_id}.json"));
        std::fs::write(&big, vec![b' '; REQUEST_LIMIT as usize + 1]).unwrap();
        set_age(&big, 20);

        // Not JSON, and JSON with an unknown field: no id is trusted.
        let junk_id = Uuid::new_v4();
        let junk = dir.join(format!("{junk_id}.json"));
        std::fs::write(&junk, b"{not json").unwrap();
        set_age(&junk, 10);
        let extra = dir.join("extra.json");
        let mut v = serde_json::to_value(request(None)).unwrap();
        v["image"] = "evil@sha256:00".into();
        std::fs::write(&extra, serde_json::to_vec(&v).unwrap()).unwrap();
        set_age(&extra, 5);

        // Exactly at the limit is accepted (padded with whitespace).
        let edge = request(Some("2.0.0"));
        let mut body = serde_json::to_vec(&edge).unwrap();
        body.resize(REQUEST_LIMIT as usize, b' ');
        let edge_path = dir.join(format!("{}.json", edge.id));
        std::fs::write(&edge_path, &body).unwrap();
        set_age(&edge_path, 1);

        let got = take_requests(dir);
        assert_eq!(got.len(), 5, "{got:?}");
        assert_eq!(got[0], Ok(good));
        let big_rej: &SpoolReject = got[1].as_ref().unwrap_err();
        assert_eq!(big_rej.request_id, Some(big_id));
        assert!(big_rej.reason.contains("4096"), "{}", big_rej.reason);
        // Malformed content keeps the id a `<uuid>.json` name carries.
        for (r, id) in got[2..4].iter().zip([Some(junk_id), None]) {
            let rej = r.as_ref().unwrap_err();
            assert_eq!(rej.request_id, id, "{rej:?}");
            assert!(rej.reason.contains("not a valid update request"), "{rej:?}");
        }
        assert_eq!(got[4], Ok(edge));
        assert!(names(dir).is_empty(), "{:?}", names(dir));
    }

    #[test]
    fn take_requests_rejects_non_regular_files() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path();
        let target = t.path().join("..").join("outside");
        std::os::unix::fs::symlink(&target, dir.join("link.json")).unwrap();
        nix::unistd::mkfifo(&dir.join("fifo.json"), nix::sys::stat::Mode::S_IRWXU).unwrap();
        let sub_id = Uuid::new_v4();
        std::fs::create_dir(dir.join(format!("{sub_id}.json"))).unwrap();
        std::fs::write(dir.join("inner"), b"x").unwrap();
        std::fs::rename(dir.join("inner"), dir.join(format!("{sub_id}.json/inner"))).unwrap();
        // A name that is not UTF-8, with valid content.
        use std::os::unix::ffi::OsStrExt;
        let odd = std::ffi::OsStr::from_bytes(b"req-\xff.json");
        std::fs::write(dir.join(odd), serde_json::to_vec(&request(None)).unwrap()).unwrap();

        let got = take_requests(dir);
        assert_eq!(got.len(), 4, "{got:?}");
        assert!(got.iter().all(|r| r.is_err()), "{got:?}");
        let sub = got
            .iter()
            .map(|r| r.as_ref().unwrap_err())
            .find(|r| r.file.contains(&sub_id.to_string()))
            .unwrap();
        assert_eq!(sub.request_id, Some(sub_id));
        // Recorded once, then set aside as dotfiles so nothing retries them.
        let left = names(dir);
        assert_eq!(left.len(), 4, "{left:?}");
        assert!(left.iter().all(|n| n.starts_with(".rejected-")), "{left:?}");
        assert!(take_requests(dir).is_empty());
        assert!(dir.join(format!(".rejected-{sub_id}.json/inner")).exists());
    }

    #[test]
    fn take_requests_deletes_processed() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path();
        let (old, new, same_a, same_b) = (
            request(Some("1.0.0")),
            request(None),
            request(Some("1.0.1")),
            request(Some("1.0.2")),
        );
        drop_request(dir, &new, 10);
        drop_request(dir, &old, 100);
        // Equal mtimes fall back to name order.
        let mtime = SystemTime::now() - Duration::from_secs(50);
        for r in [&same_a, &same_b] {
            let p = dir.join(format!("{}.json", r.id));
            std::fs::write(&p, serde_json::to_vec(r).unwrap()).unwrap();
            std::fs::File::options()
                .write(true)
                .open(&p)
                .unwrap()
                .set_modified(mtime)
                .unwrap();
        }
        // The server's in-progress temp files are dotfiles and are left alone.
        std::fs::write(dir.join(format!(".tmp-{}", Uuid::new_v4())), b"partial").unwrap();

        let got: Vec<UpdateRequest> = take_requests(dir).into_iter().map(Result::unwrap).collect();
        let mut tie = [same_a.clone(), same_b.clone()];
        tie.sort_by_key(|r| format!("{}.json", r.id));
        assert_eq!(got, vec![old, tie[0].clone(), tie[1].clone(), new]);
        let left = names(dir);
        assert_eq!(left.len(), 1);
        assert!(left[0].starts_with(".tmp-"), "{left:?}");
        assert!(take_requests(dir).is_empty());
    }
}
