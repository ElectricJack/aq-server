//! `[workers.install]`: where a new machine gets its worker from.
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;

/// `origin` is the HTTPS origin that serves the worker installer;
/// `manifest_file` is the JSON file the publishing step writes beside the
/// bundle (`installer_sha256`, `bundle_sha256`, `worker_revision`). It is read
/// on every call, so publishing a new bundle changes the command the
/// dashboard shows without a restart. Both unset: no hosted installer.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct WorkerInstall {
    pub origin: Option<String>,
    pub manifest_file: Option<PathBuf>,
}

impl WorkerInstall {
    /// The installer as it is published now: its origin and the digest that
    /// pins it. `None` when none is configured, the manifest cannot be read,
    /// or it names no well-formed installer digest.
    pub fn published(&self) -> Option<Value> {
        let (origin, file) = self.origin.as_ref().zip(self.manifest_file.as_ref())?;
        let manifest: Value = serde_json::from_str(&std::fs::read_to_string(file).ok()?).ok()?;
        let digest = |key: &str| {
            manifest[key].as_str().filter(|d| {
                d.len() == 64
                    && d.bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            })
        };
        let revision = manifest["worker_revision"]
            .as_str()
            .filter(|r| r.len() <= 64);
        Some(
            json!({"origin": origin, "installer_sha256": digest("installer_sha256")?,
            "bundle_sha256": digest("bundle_sha256"), "worker_revision": revision}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::WorkerInstall;

    #[test]
    fn the_published_installer_is_read_from_the_manifest_each_time() {
        let dir = std::env::temp_dir().join(format!("aq-install-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("install-manifest.json");
        let install = WorkerInstall {
            origin: Some("https://aq.example".into()),
            manifest_file: Some(file.clone()),
        };
        assert!(install.published().is_none(), "no manifest yet");
        let write = |digest: &str, revision: &str| {
            let body = serde_json::json!({"installer_sha256": digest, "worker_revision": revision});
            std::fs::write(&file, body.to_string()).unwrap();
        };
        write(&"a".repeat(64), "0123456789ab");
        let first = install.published().unwrap();
        assert_eq!(first["origin"], "https://aq.example");
        assert_eq!(first["installer_sha256"], "a".repeat(64));
        assert_eq!(first["worker_revision"], "0123456789ab");
        assert!(first["bundle_sha256"].is_null());
        write(&"b".repeat(64), "ba9876543210");
        assert_eq!(
            install.published().unwrap()["installer_sha256"],
            "b".repeat(64)
        );
        write("not-a-digest", "x");
        assert!(install.published().is_none());
        assert!(WorkerInstall::default().published().is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
