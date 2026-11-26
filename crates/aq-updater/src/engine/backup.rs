//! Backup space checks and retention.

use super::{Check, Engine, rt};
use crate::journal::InFlight;
use crate::runtime::Runtime;
use crate::verify::ReleaseVerifier;
use aq_release::Step;
use chrono::Utc;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

/// The stamp a new backup's name starts with: now, or one second after the
/// newest dump when that is not older. Names stay unique and name order stays
/// creation order even for two backups within one second.
fn next_stamp(dir: &Path) -> String {
    const FORMAT: &str = "%Y%m%dT%H%M%SZ";
    let newest = dumps(dir).first().and_then(|p| {
        let name = p.file_name()?.to_str()?;
        chrono::NaiveDateTime::parse_from_str(name.get(..16)?, FORMAT).ok()
    });
    let now = Utc::now().naive_utc();
    let next = newest.map_or(now, |n| now.max(n + chrono::TimeDelta::seconds(1)));
    next.format(FORMAT).to_string()
}

/// `*.dump` files in `dir`, newest first. Names start with a UTC timestamp
/// that `next_stamp` keeps increasing, so name order is creation order.
fn dumps(dir: &Path) -> Vec<PathBuf> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(dir = %dir.display(), error = %e, "cannot list backups");
            }
            return Vec::new();
        }
    };
    let mut v: Vec<PathBuf> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "dump"))
        .collect();
    v.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    v
}

/// The updater will not start a backup (DEP8): the backup filesystem (the
/// root filesystem in the standard layout) has less free space than the
/// newest backup plus 10%.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "not enough free space to start the backup in {}: {free} bytes free, {need} needed (the newest backup, {newest} bytes, plus 10%); free some space and retry",
    dir.display()
)]
pub struct InsufficientBackupSpace {
    pub dir: PathBuf,
    pub free: u64,
    pub newest: u64,
    pub need: u64,
}

/// Free bytes a backup needs: the newest backup's size plus 10%, rounded up.
pub fn backup_space_needed(newest: u64) -> u64 {
    newest.saturating_add(newest.div_ceil(10))
}

/// Refuse to start a backup in `dir` when `free` is below the newest
/// backup's size (`newest`) plus 10%.
pub fn check_backup_space(
    dir: &Path,
    free: u64,
    newest: u64,
) -> Result<(), InsufficientBackupSpace> {
    let need = backup_space_needed(newest);
    if free < need {
        return Err(InsufficientBackupSpace {
            dir: dir.to_path_buf(),
            free,
            newest,
            need,
        });
    }
    Ok(())
}

fn last_backup_size(dir: &Path) -> u64 {
    dumps(dir)
        .first()
        .and_then(|p| std::fs::metadata(p).ok())
        .map_or(0, |m| m.len())
}

/// Backups hold the whole database: owner-only.
fn create_backup_dir(dir: &Path) -> Result<(), String> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))
}

async fn take_backup<R: Runtime>(runtime: &R, dir: &Path, dest: &Path) -> Check<u64> {
    if let Err(e) = create_backup_dir(dir) {
        return Ok(Err(e));
    }
    rt(runtime.backup(dest).await)
}

pub(super) async fn initial_space_check<R: Runtime>(
    runtime: &R,
    dir: &Path,
    minimum: u64,
) -> Check {
    // statvfs needs the directory to exist on the first run.
    if let Err(e) = create_backup_dir(dir) {
        return Ok(Err(e));
    }
    let need = minimum.max(last_backup_size(dir).saturating_mul(3));
    Ok(match rt(runtime.free_bytes(dir).await)? {
        Ok(free) if free >= need => Ok(()),
        Ok(free) => Err(format!(
            "not enough free space in {}: {free} bytes free, {need} needed (the larger of min_free_bytes and 3x the last backup); free some space and retry",
            dir.display()
        )),
        Err(e) => Err(format!("cannot check free space in {}: {e}", dir.display())),
    })
}

/// DEP8 checks again after the pull has used its space and before anything
/// stops. The dump needs the newest backup's size plus 10%.
pub(super) async fn post_pull_space_check<R: Runtime>(runtime: &R, dir: &Path) -> Check {
    Ok(match rt(runtime.free_bytes(dir).await)? {
        Ok(free) => check_backup_space(dir, free, last_backup_size(dir)).map_err(|e| e.to_string()),
        Err(e) => Err(format!("cannot check free space in {}: {e}", dir.display())),
    })
}

impl<R: Runtime, V: ReleaseVerifier> Engine<R, V> {
    pub(super) async fn back_up_update(&mut self, cur: &mut InFlight, dir: &Path) -> Check {
        // BackedUp: the path is persisted before the dump starts.
        let from = self
            .active_version()
            .map_or_else(|| "none".to_string(), |v| v.to_string());
        let dest = dir.join(format!(
            "{}-{from}-{}.dump",
            next_stamp(dir),
            cur.target.version
        ));
        // Checked before the path is journaled: the in-flight backup (which
        // failure and recovery delete) is always a file this attempt made.
        if dest.exists() {
            let detail = format!(
                "backup failed, update abandoned before replacement: {} already exists",
                dest.display()
            );
            return Ok(Err(detail));
        }
        cur.backup = Some(dest.clone());
        self.set_step(cur, Step::BackedUp)?;
        if let Err(e) = take_backup(&self.runtime, dir, &dest).await? {
            self.remove_backup(Some(&dest));
            let detail = format!(
                "backup failed, update abandoned before replacement{}: {e}",
                self.active_version()
                    .map_or(String::new(), |v| format!(" ({v} left running)"))
            );
            return Ok(Err(detail));
        }
        self.prune_backups(&dest);
        Ok(Ok(()))
    }

    pub(super) fn remove_backup(&self, path: Option<&Path>) {
        let Some(p) = path else { return };
        if self.journal.state().active_backup.as_deref() == Some(p) {
            return;
        }
        match std::fs::remove_file(p) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(path = %p.display(), error = %e, "cannot remove partial backup")
            }
        }
    }

    /// Keep the newest `backups_keep` dumps; never delete `active_backup` or
    /// the in-flight backup.
    fn prune_backups(&self, in_flight: &Path) {
        let protected = self.journal.state().active_backup.as_deref();
        for p in dumps(&self.settings.backup_dir)
            .into_iter()
            .skip(self.settings.backups_keep)
        {
            if p == in_flight || Some(p.as_path()) == protected {
                continue;
            }
            if let Err(e) = std::fs::remove_file(&p) {
                tracing::warn!(path = %p.display(), error = %e, "cannot prune backup");
            }
        }
    }
}
