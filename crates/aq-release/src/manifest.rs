use chrono::{DateTime, Utc};
use regex::Regex;
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

const MANIFEST_VERSION: u32 = 1;

fn digest_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // A host starting with a letter or digit (never `-`, so it cannot read as
    // an option), an optional `:port`, then a path whose segments cannot
    // hold `:` (so no tag), then the digest.
    RE.get_or_init(|| {
        Regex::new(r"^[a-z0-9][a-z0-9.-]*(:[0-9]+)?/[a-z0-9._/-]+@sha256:[0-9a-f]{64}$")
            .expect("static regex")
    })
}

/// True when `image` is a digest-pinned reference
/// (`host[:port]/path@sha256:<64 hex>`, no tag),
/// the only form a manifest may carry and the updater may pass to `docker`.
pub fn is_digest_ref(image: &str) -> bool {
    digest_re().is_match(image)
}

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("manifest is not valid: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("unsupported manifest_version {0}; this updater understands only version 1")]
    UnsupportedManifestVersion(u32),
    #[error(
        "{role} image {image:?} is not digest-pinned (expected host[:port]/path@sha256:<64 hex>)"
    )]
    InvalidImage { role: &'static str, image: String },
    #[error(
        "state_format {state_format} is not in readable_state_formats {readable:?}; add it to the manifest"
    )]
    StateFormatNotReadable {
        state_format: u32,
        readable: Vec<u32>,
    },
    #[error("protocol range is inverted (min {min} > max {max})")]
    ProtocolRange { min: u32, max: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageRefs {
    pub server: String,
    pub worker: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolRange {
    pub min: u32,
    pub max: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseManifest {
    pub manifest_version: u32,
    pub version: Version,
    pub source_revision: String,
    pub images: ImageRefs,
    pub protocol: ProtocolRange,
    pub state_format: u32,
    pub readable_state_formats: Vec<u32>,
    pub upgrade_from: VersionReq,
    pub created_at: DateTime<Utc>,
}

impl ReleaseManifest {
    /// Parse and validate a manifest.
    pub fn from_json(bytes: &[u8]) -> Result<Self, ManifestError> {
        let m: Self = serde_json::from_slice(bytes)?;
        m.validate()?;
        Ok(m)
    }

    /// Check the invariants `from_json` enforces. Use it on manifests that
    /// were deserialized some other way (for example from the updater journal).
    pub fn validate(&self) -> Result<(), ManifestError> {
        let m = self;
        if m.manifest_version != MANIFEST_VERSION {
            return Err(ManifestError::UnsupportedManifestVersion(
                m.manifest_version,
            ));
        }
        for (role, image) in [("server", &m.images.server), ("worker", &m.images.worker)] {
            if !digest_re().is_match(image) {
                return Err(ManifestError::InvalidImage {
                    role,
                    image: image.clone(),
                });
            }
        }
        if !m.readable_state_formats.contains(&m.state_format) {
            return Err(ManifestError::StateFormatNotReadable {
                state_format: m.state_format,
                readable: m.readable_state_formats.clone(),
            });
        }
        if m.protocol.min > m.protocol.max {
            return Err(ManifestError::ProtocolRange {
                min: m.protocol.min,
                max: m.protocol.max,
            });
        }
        Ok(())
    }

    /// The exact bytes that get signed: object keys sorted, no whitespace.
    pub fn to_canonical_json(&self) -> Vec<u8> {
        let mut value = serde_json::to_value(self).expect("manifest serializes");
        value.sort_all_objects();
        serde_json::to_vec(&value).expect("JSON value serializes")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradePlan {
    /// True when rollback must restore the pre-update backup (state format changes).
    pub needs_restore_on_rollback: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("release is already installed; nothing to do")]
    SameVersion,
    #[error(
        "release requires upgrading from {allowed}, but {current} is installed; install an intermediate release in that range first"
    )]
    NotInUpgradePath {
        current: Version,
        allowed: VersionReq,
    },
    #[error(
        "installed data uses state format {found}, which the target reads only as {readable:?}; install a release that reads format {found}, or restore a compatible backup"
    )]
    UnreadableState { found: u32, readable: Vec<u32> },
    #[error(
        "{target} is older than installed {current}; downgrades are refused by install, use `aq-updater restore-previous` instead"
    )]
    Downgrade { current: Version, target: Version },
    #[error(
        "the target speaks worker protocols {}..{} but the installed release speaks {}..{}, with no protocol in common, so no worker could talk to both; upgrade the workers first, or install an intermediate release whose protocol range overlaps both",
        .target.min, .target.max, .current.min, .current.max
    )]
    ProtocolGap {
        current: ProtocolRange,
        target: ProtocolRange,
    },
}

/// Decide whether `target` may be installed over `current`.
pub fn check_upgrade(
    current: Option<&ReleaseManifest>,
    current_state_format: Option<u32>,
    target: &ReleaseManifest,
) -> Result<UpgradePlan, Refusal> {
    if let Some(cur) = current {
        check_release_compatibility(cur, target)?;
    }
    let found = current_state_format.or(current.map(|c| c.state_format));
    if let Some(found) = found
        && !target.readable_state_formats.contains(&found)
    {
        return Err(Refusal::UnreadableState {
            found,
            readable: target.readable_state_formats.clone(),
        });
    }
    Ok(UpgradePlan {
        needs_restore_on_rollback: found.is_some_and(|f| f != target.state_format),
    })
}

fn check_release_compatibility(
    cur: &ReleaseManifest,
    target: &ReleaseManifest,
) -> Result<(), Refusal> {
    if cur.version == target.version {
        return Err(Refusal::SameVersion);
    }
    if target.version < cur.version {
        return Err(Refusal::Downgrade {
            current: cur.version.clone(),
            target: target.version.clone(),
        });
    }
    if !target.upgrade_from.matches(&cur.version) {
        return Err(Refusal::NotInUpgradePath {
            current: cur.version.clone(),
            allowed: target.upgrade_from.clone(),
        });
    }
    // Workers speak one protocol of the active release's range; a target
    // whose range shares none would strand every enrolled worker.
    let (c, t) = (cur.protocol, target.protocol);
    if t.min > c.max || c.min > t.max {
        return Err(Refusal::ProtocolGap {
            current: c,
            target: t,
        });
    }
    Ok(())
}
