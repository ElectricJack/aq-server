//! Readiness verification for the selected release.

use super::{Check, EngineSettings, rt};
use crate::runtime::Runtime;
use aq_release::{ReadinessReport, ReleaseManifest};

/// A readiness report passes only if it describes exactly `m`, the database
/// is reachable, and authentication is configured as `auth_mode`.
pub fn check_ready(
    r: &ReadinessReport,
    m: &ReleaseManifest,
    auth_mode: &str,
) -> Result<(), String> {
    let mut bad = Vec::new();
    if r.version != m.version {
        bad.push(format!("version {} (expected {})", r.version, m.version));
    }
    if r.source_revision != m.source_revision {
        bad.push(format!(
            "source_revision {:?} (expected {:?})",
            r.source_revision, m.source_revision
        ));
    }
    if r.state_format != m.state_format {
        bad.push(format!(
            "state_format {} (expected {})",
            r.state_format, m.state_format
        ));
    }
    if r.database != "ok" {
        bad.push(format!("database {:?} (expected \"ok\")", r.database));
    }
    if (r.protocol_min, r.protocol_max) != (m.protocol.min, m.protocol.max) {
        bad.push(format!(
            "protocol {}..{} (expected {}..{})",
            r.protocol_min, r.protocol_max, m.protocol.min, m.protocol.max
        ));
    }
    if r.auth_mode != auth_mode {
        bad.push(format!(
            "auth_mode {:?} (expected {auth_mode:?})",
            r.auth_mode
        ));
    }
    if bad.is_empty() {
        Ok(())
    } else {
        Err(format!("readiness mismatch: {}", bad.join(", ")))
    }
}

pub(super) async fn probe<R: Runtime>(
    runtime: &R,
    cfg: &EngineSettings,
    m: &ReleaseManifest,
) -> Check {
    if let Err(e) = rt(runtime.wait_healthy(cfg.readiness_deadline).await)? {
        return Ok(Err(format!("wait_healthy: {e}")));
    }
    match rt(runtime.readiness().await)? {
        Ok(report) => Ok(check_ready(&report, m, &cfg.expected_auth_mode)),
        Err(e) => Ok(Err(format!("readiness: {e}"))),
    }
}

pub(super) async fn ensure_running<R: Runtime>(
    runtime: &R,
    cfg: &EngineSettings,
    m: &ReleaseManifest,
) -> Check {
    if let Err(e) = rt(runtime.start_server(&m.images.server).await)? {
        return Ok(Err(format!("start_server: {e}")));
    }
    probe(runtime, cfg, m).await
}
