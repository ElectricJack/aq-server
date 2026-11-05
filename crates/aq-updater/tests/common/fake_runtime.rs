//! Scriptable in-memory `Runtime` plus a `MaintenanceSink` that shares its
//! call log, so tests can assert the exact order of runtime and maintenance
//! calls.
//!
//! Failures are keyed by method name and per-method call index (0-based,
//! reset by `clear_log`). `fail` returns `RuntimeError::Failed`; `crash`
//! returns `RuntimeError::Interrupted` before the call has any effect, which
//! makes the engine stop with the journal as written, like a killed process;
//! `crash_after` lets the call take effect and then reports the crash (the
//! process died right after the action, before its result was seen).
//!
//! The fake models a host: which server image is running and which state
//! format the database holds. Starting a release whose readable formats
//! include the database's format "migrates" it to the release's format;
//! a release that cannot read the database reports `database: "error"`.
//! `backup` writes the database format into the dump and `restore` reads it
//! back, so a missing restore is observable. `selected` is the image
//! `release.env` names (what a manual `docker compose up` would start).
#![allow(dead_code)]

use aq_release::{ReadinessReport, ReleaseManifest};
use aq_updater::engine::MaintenanceSink;
use aq_updater::runtime::{ReleaseSelector, Runtime, RuntimeError};
use async_trait::async_trait;
use semver::Version;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

pub const STATE_FORMAT_LABEL: &str = "io.agent-q.state-format";

#[derive(Clone)]
enum Script {
    Fail(String),
    Crash,
    CrashAfter,
}

struct Inner {
    log: Vec<String>,
    counts: HashMap<String, usize>,
    scripts: HashMap<(String, usize), Script>,
    releases: HashMap<Version, (Vec<u8>, Vec<u8>)>,
    channel: Option<Version>,
    by_image: HashMap<String, ReleaseManifest>,
    labels: HashMap<String, HashMap<String, String>>,
    reports: HashMap<String, ReadinessReport>,
    free: u64,
    /// Values for successive `free_bytes` calls; then `free`.
    free_seq: std::collections::VecDeque<u64>,
    /// Local images (digest refs); `pull` adds, `remove_image` removes.
    images: Vec<String>,
    backup_size: usize,
    running: Option<String>,
    selected: Option<String>,
    db_format: Option<u32>,
    /// Starting this image leaves the database in this format, whatever
    /// its manifest says (a release that migrates without a format bump).
    start_migrates: HashMap<String, u32>,
    maintenance: bool,
    auth_mode: String,
    /// Set by a `CrashAfter` script; reported when the call finishes.
    pending_crash: Option<String>,
}

#[derive(Clone)]
pub struct FakeRuntime(Arc<Mutex<Inner>>);

impl Default for FakeRuntime {
    fn default() -> Self {
        FakeRuntime(Arc::new(Mutex::new(Inner {
            log: Vec::new(),
            counts: HashMap::new(),
            scripts: HashMap::new(),
            releases: HashMap::new(),
            channel: None,
            by_image: HashMap::new(),
            labels: HashMap::new(),
            reports: HashMap::new(),
            free: 100 * 1024 * 1024 * 1024,
            free_seq: Default::default(),
            images: Vec::new(),
            backup_size: 100,
            running: None,
            selected: None,
            db_format: None,
            start_migrates: HashMap::new(),
            maintenance: false,
            auth_mode: "cloudflare_access".into(),
            pending_crash: None,
        })))
    }
}

impl FakeRuntime {
    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.0.lock().unwrap()
    }

    /// Log the call and apply any script for this (method, index).
    fn call(&self, method: &str, arg: &str) -> Result<(), RuntimeError> {
        let mut i = self.inner();
        i.log.push(if arg.is_empty() {
            method.to_string()
        } else {
            format!("{method} {arg}")
        });
        let n = i.counts.entry(method.to_string()).or_insert(0);
        let idx = *n;
        *n += 1;
        match i.scripts.get(&(method.to_string(), idx)) {
            None => Ok(()),
            Some(Script::Fail(m)) => Err(RuntimeError::Failed(format!("{method}: {m}"))),
            Some(Script::CrashAfter) => {
                i.pending_crash = Some(format!("simulated crash after {method}"));
                Ok(())
            }
            Some(Script::Crash) => Err(RuntimeError::Interrupted(format!(
                "simulated crash in {method}"
            ))),
        }
    }

    /// Report a `crash_after` once the call's effect has been applied.
    fn finish<T>(&self, r: Result<T, RuntimeError>) -> Result<T, RuntimeError> {
        match self.inner().pending_crash.take() {
            Some(m) => Err(RuntimeError::Interrupted(m)),
            None => r,
        }
    }

    // ---- scripting ----

    /// Make `method`'s `idx`-th call fail.
    pub fn fail(&self, method: &str, idx: usize, msg: &str) {
        self.inner()
            .scripts
            .insert((method.into(), idx), Script::Fail(msg.into()));
    }

    /// Make `method`'s `idx`-th call "crash" (RuntimeError::Interrupted).
    pub fn crash(&self, method: &str, idx: usize) {
        self.inner()
            .scripts
            .insert((method.into(), idx), Script::Crash);
    }

    /// Let `method`'s `idx`-th call take effect, then "crash".
    pub fn crash_after(&self, method: &str, idx: usize) {
        self.inner()
            .scripts
            .insert((method.into(), idx), Script::CrashAfter);
    }

    pub fn set_auth_mode(&self, mode: &str) {
        self.inner().auth_mode = mode.into();
    }

    /// Change the marker behind the engine's back (e.g. an operator removed it).
    pub fn set_maintenance(&self, on: bool) {
        self.inner().maintenance = on;
    }

    /// Publish a release: fetchable by version, startable by its server image,
    /// labelled with its state format.
    pub fn add_release(&self, m: &ReleaseManifest, manifest: Vec<u8>, bundle: Vec<u8>) {
        let mut i = self.inner();
        i.releases.insert(m.version.clone(), (manifest, bundle));
        i.by_image.insert(m.images.server.clone(), m.clone());
        i.labels.insert(
            m.images.server.clone(),
            HashMap::from([(STATE_FORMAT_LABEL.to_string(), m.state_format.to_string())]),
        );
    }

    pub fn set_channel(&self, v: Version) {
        self.inner().channel = Some(v);
    }
    pub fn set_labels(&self, image: &str, labels: HashMap<String, String>) {
        self.inner().labels.insert(image.into(), labels);
    }
    /// Override the readiness report the given server image returns.
    pub fn set_report(&self, image: &str, r: ReadinessReport) {
        self.inner().reports.insert(image.into(), r);
    }
    pub fn set_free(&self, bytes: u64) {
        self.inner().free = bytes;
    }
    /// `free_bytes` returns these values in order, then the `set_free` value.
    pub fn set_free_seq(&self, seq: &[u64]) {
        self.inner().free_seq = seq.iter().copied().collect();
    }
    /// Add local images (digest refs), as if pulled earlier.
    pub fn add_images(&self, images: &[&str]) {
        let mut i = self.inner();
        for im in images {
            if !i.images.iter().any(|x| x == im) {
                i.images.push(im.to_string());
            }
        }
    }
    pub fn images(&self) -> Vec<String> {
        self.inner().images.clone()
    }
    pub fn set_backup_size(&self, bytes: usize) {
        self.inner().backup_size = bytes;
    }
    pub fn set_db_format(&self, f: Option<u32>) {
        self.inner().db_format = f;
    }
    /// Starting `image` leaves the database in format `fmt`.
    pub fn set_start_migrates(&self, image: &str, fmt: u32) {
        self.inner().start_migrates.insert(image.into(), fmt);
    }

    // ---- inspection ----

    pub fn log(&self) -> Vec<String> {
        self.inner().log.clone()
    }
    /// Forget the call log and reset per-method call indexes and scripts.
    pub fn clear_log(&self) {
        let mut i = self.inner();
        i.log.clear();
        i.counts.clear();
        i.scripts.clear();
    }
    /// Number of logged calls whose entry starts with `prefix`.
    pub fn count(&self, prefix: &str) -> usize {
        self.log().iter().filter(|c| c.starts_with(prefix)).count()
    }
    /// Index of the first logged call that starts with `prefix`.
    pub fn position(&self, prefix: &str) -> Option<usize> {
        self.log().iter().position(|c| c.starts_with(prefix))
    }
    pub fn running(&self) -> Option<String> {
        self.inner().running.clone()
    }
    /// The image `release.env` names.
    pub fn selected(&self) -> Option<String> {
        self.inner().selected.clone()
    }
    pub fn db_format(&self) -> Option<u32> {
        self.inner().db_format
    }
    pub fn maintenance_on(&self) -> bool {
        self.inner().maintenance
    }
    pub fn maintenance(&self) -> FakeMaintenance {
        FakeMaintenance(self.clone())
    }
}

/// Method bodies; the `Runtime` impl below wraps each in `finish`.
impl FakeRuntime {
    async fn fetch_release_impl(
        &self,
        selector: ReleaseSelector,
    ) -> Result<(Vec<u8>, Vec<u8>), RuntimeError> {
        let arg = match &selector {
            ReleaseSelector::Version(v) => v.to_string(),
            ReleaseSelector::Channel => "channel".into(),
        };
        self.call("fetch_release", &arg)?;
        let i = self.inner();
        let v = match selector {
            ReleaseSelector::Version(v) => v,
            ReleaseSelector::Channel => i
                .channel
                .clone()
                .ok_or_else(|| RuntimeError::Failed("no channel release".into()))?,
        };
        i.releases
            .get(&v)
            .cloned()
            .ok_or_else(|| RuntimeError::Failed(format!("release {v} not found")))
    }

    async fn pull_impl(&self, image: &str) -> Result<(), RuntimeError> {
        self.call("pull", image)?;
        self.add_images(&[image]);
        Ok(())
    }

    async fn list_images_impl(&self, repository: &str) -> Result<Vec<String>, RuntimeError> {
        self.call("list_images", repository)?;
        let prefix = format!("{repository}@");
        Ok(self
            .inner()
            .images
            .iter()
            .filter(|i| i.starts_with(&prefix))
            .cloned()
            .collect())
    }

    async fn remove_image_impl(&self, image: &str) -> Result<(), RuntimeError> {
        self.call("remove_image", image)?;
        let mut i = self.inner();
        if i.running.as_deref() == Some(image) {
            return Err(RuntimeError::Failed(format!("{image} is in use")));
        }
        i.images.retain(|x| x != image);
        Ok(())
    }

    async fn image_labels_impl(
        &self,
        image: &str,
    ) -> Result<HashMap<String, String>, RuntimeError> {
        self.call("image_labels", image)?;
        Ok(self.inner().labels.get(image).cloned().unwrap_or_default())
    }

    async fn free_bytes_impl(&self, path: &Path) -> Result<u64, RuntimeError> {
        self.call("free_bytes", &path.display().to_string())?;
        let mut i = self.inner();
        Ok(i.free_seq.pop_front().unwrap_or(i.free))
    }

    async fn backup_impl(&self, dest: &Path) -> Result<u64, RuntimeError> {
        self.call("backup", &dest.display().to_string())?;
        let (fmt, size) = {
            let i = self.inner();
            (i.db_format, i.backup_size)
        };
        let mut body =
            format!("fmt:{}\n", fmt.map_or("none".into(), |f| f.to_string())).into_bytes();
        if body.len() < size {
            body.resize(size, b'.');
        }
        std::fs::write(dest, &body).map_err(|e| RuntimeError::Failed(e.to_string()))?;
        Ok(body.len() as u64)
    }

    async fn restore_impl(&self, src: &Path) -> Result<(), RuntimeError> {
        self.call("restore", &src.display().to_string())?;
        let mut i = self.inner();
        if i.running.is_some() {
            return Err(RuntimeError::Failed(
                "restore while the server is running".into(),
            ));
        }
        let text = std::fs::read_to_string(src).map_err(|e| RuntimeError::Failed(e.to_string()))?;
        let fmt = text
            .lines()
            .next()
            .and_then(|l| l.strip_prefix("fmt:"))
            .ok_or_else(|| RuntimeError::Failed("not a fake dump".into()))?;
        i.db_format = fmt.parse().ok();
        Ok(())
    }

    async fn start_server_impl(&self, image: &str) -> Result<(), RuntimeError> {
        self.call("start_server", image)?;
        let mut i = self.inner();
        let m = i
            .by_image
            .get(image)
            .cloned()
            .ok_or_else(|| RuntimeError::Failed(format!("unknown image {image}")))?;
        i.running = Some(image.to_string());
        i.selected = Some(image.to_string());
        // The server migrates data it can read up to its own format.
        if i.db_format
            .is_none_or(|f| m.readable_state_formats.contains(&f))
        {
            i.db_format = Some(
                i.start_migrates
                    .get(image)
                    .copied()
                    .unwrap_or(m.state_format),
            );
        }
        Ok(())
    }

    async fn select_release_impl(&self, image: &str) -> Result<(), RuntimeError> {
        self.call("select_release", image)?;
        self.inner().selected = Some(image.to_string());
        Ok(())
    }

    async fn stop_server_impl(&self) -> Result<(), RuntimeError> {
        self.call("stop_server", "")?;
        self.inner().running = None;
        Ok(())
    }

    async fn wait_healthy_impl(&self, deadline: Duration) -> Result<(), RuntimeError> {
        let _ = deadline;
        self.call("wait_healthy", "")?;
        if self.inner().running.is_some() {
            Ok(())
        } else {
            Err(RuntimeError::Failed("server is not running".into()))
        }
    }

    async fn readiness_impl(&self) -> Result<ReadinessReport, RuntimeError> {
        self.call("readiness", "")?;
        let i = self.inner();
        let image = i
            .running
            .clone()
            .ok_or_else(|| RuntimeError::Failed("server is not running".into()))?;
        if let Some(r) = i.reports.get(&image) {
            return Ok(r.clone());
        }
        let m = &i.by_image[&image];
        let readable = i
            .db_format
            .is_some_and(|f| m.readable_state_formats.contains(&f));
        Ok(ReadinessReport {
            version: m.version.clone(),
            source_revision: m.source_revision.clone(),
            state_format: m.state_format,
            protocol_min: m.protocol.min,
            protocol_max: m.protocol.max,
            database: if readable { "ok" } else { "error" }.into(),
            auth_mode: i.auth_mode.clone(),
        })
    }

    async fn server_state_format_impl(&self) -> Result<Option<u32>, RuntimeError> {
        self.call("server_state_format", "")?;
        Ok(self.inner().db_format)
    }
}

#[async_trait]
impl Runtime for FakeRuntime {
    async fn fetch_release(
        &self,
        selector: ReleaseSelector,
    ) -> Result<(Vec<u8>, Vec<u8>), RuntimeError> {
        let r = self.fetch_release_impl(selector).await;
        self.finish(r)
    }
    async fn pull(&self, image: &str) -> Result<(), RuntimeError> {
        let r = self.pull_impl(image).await;
        self.finish(r)
    }
    async fn image_labels(&self, image: &str) -> Result<HashMap<String, String>, RuntimeError> {
        let r = self.image_labels_impl(image).await;
        self.finish(r)
    }
    async fn free_bytes(&self, path: &Path) -> Result<u64, RuntimeError> {
        let r = self.free_bytes_impl(path).await;
        self.finish(r)
    }
    async fn backup(&self, dest: &Path) -> Result<u64, RuntimeError> {
        let r = self.backup_impl(dest).await;
        self.finish(r)
    }
    async fn restore(&self, src: &Path) -> Result<(), RuntimeError> {
        let r = self.restore_impl(src).await;
        self.finish(r)
    }
    async fn start_server(&self, image: &str) -> Result<(), RuntimeError> {
        let r = self.start_server_impl(image).await;
        self.finish(r)
    }
    async fn select_release(&self, image: &str) -> Result<(), RuntimeError> {
        let r = self.select_release_impl(image).await;
        self.finish(r)
    }
    async fn stop_server(&self) -> Result<(), RuntimeError> {
        let r = self.stop_server_impl().await;
        self.finish(r)
    }
    async fn wait_healthy(&self, deadline: Duration) -> Result<(), RuntimeError> {
        let r = self.wait_healthy_impl(deadline).await;
        self.finish(r)
    }
    async fn readiness(&self) -> Result<ReadinessReport, RuntimeError> {
        let r = self.readiness_impl().await;
        self.finish(r)
    }
    async fn server_state_format(&self) -> Result<Option<u32>, RuntimeError> {
        let r = self.server_state_format_impl().await;
        self.finish(r)
    }
    async fn list_images(&self, repository: &str) -> Result<Vec<String>, RuntimeError> {
        let r = self.list_images_impl(repository).await;
        self.finish(r)
    }
    async fn remove_image(&self, image: &str) -> Result<(), RuntimeError> {
        let r = self.remove_image_impl(image).await;
        self.finish(r)
    }
}

pub struct FakeMaintenance(FakeRuntime);

impl MaintenanceSink for FakeMaintenance {
    fn enter(&self) -> Result<(), RuntimeError> {
        self.0.call("enter", "")?;
        self.0.inner().maintenance = true;
        self.0.finish(Ok(()))
    }
    fn leave(&self) -> Result<(), RuntimeError> {
        self.0.call("leave", "")?;
        self.0.inner().maintenance = false;
        self.0.finish(Ok(()))
    }
}
