//! Validate Docker references and read release image metadata.

use super::{MAX_RELEASE_FILE, RuntimeResult, failed};

pub(super) fn tag_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}$").expect("static"))
}

fn container_id_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^[0-9a-f]{12,64}$").expect("static"))
}

fn fetch_container_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^aq-release-fetch-[0-9a-f]{32}$").expect("static"))
}

/// A fresh name for the temporary container `fetch_release` creates, so it
/// can be removed by name whatever `docker create` printed (or if it never
/// finished).
pub fn fetch_container_name() -> String {
    format!("aq-release-fetch-{}", uuid::Uuid::new_v4().simple())
}

pub(super) fn fetch_container(name: &str) -> RuntimeResult<&str> {
    if fetch_container_re().is_match(name) {
        Ok(name)
    } else {
        Err(failed(format!(
            "unexpected fetch container name {name:.100?}"
        )))
    }
}

pub(super) fn digest_ref(image: &str) -> RuntimeResult<&str> {
    if aq_release::is_digest_ref(image) {
        Ok(image)
    } else {
        Err(failed(format!(
            "refusing image reference {:.200?}: not digest-pinned (host[:port]/path@sha256:<64 hex>)",
            image
        )))
    }
}

/// A repository name with no tag or digest, as the part of a digest ref
/// before `@`.
pub(super) fn repository(repo: &str) -> RuntimeResult<&str> {
    if aq_release::is_digest_ref(&format!("{repo}@sha256:{}", "0".repeat(64))) {
        Ok(repo)
    } else {
        Err(failed(format!(
            "refusing repository {repo:.200?}: not host[:port]/path with no tag or digest"
        )))
    }
}

/// Digest refs from `docker image ls --digests` lines; images without a
/// digest (`<none>`) are skipped.
pub fn parse_image_list(out: &[u8]) -> Vec<String> {
    let mut v: Vec<String> = String::from_utf8_lossy(out)
        .lines()
        .map(str::trim)
        .filter(|l| aq_release::is_digest_ref(l))
        .map(str::to_string)
        .collect();
    v.sort();
    v.dedup();
    v
}

pub(super) fn container_id(id: &str) -> RuntimeResult<&str> {
    if container_id_re().is_match(id) {
        Ok(id)
    } else {
        Err(failed(format!(
            "unexpected container id {id:.100?} from docker"
        )))
    }
}

/// The content of the regular file `name` from a `docker cp <id>:/path -`
/// tar stream, at most `MAX_RELEASE_FILE` bytes.
pub fn extract_tar_file(tar_bytes: &[u8], name: &str) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut archive = tar::Archive::new(tar_bytes);
    let entries = archive
        .entries()
        .map_err(|e| format!("not a tar stream: {e}"))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("bad tar stream: {e}"))?;
        let path = entry
            .path()
            .map_err(|e| format!("bad tar entry name: {e}"))?;
        if path.file_name().and_then(|n| n.to_str()) != Some(name) {
            continue;
        }
        if entry.header().entry_type() != tar::EntryType::Regular {
            return Err(format!("{name} in the release image is not a regular file"));
        }
        if entry.size() > MAX_RELEASE_FILE as u64 {
            return Err(format!(
                "{name} in the release image is larger than {MAX_RELEASE_FILE} bytes"
            ));
        }
        let mut data = Vec::new();
        entry
            .take(MAX_RELEASE_FILE as u64 + 1)
            .read_to_end(&mut data)
            .map_err(|e| format!("cannot read {name} from the tar stream: {e}"))?;
        if data.len() > MAX_RELEASE_FILE {
            return Err(format!(
                "{name} in the release image is larger than {MAX_RELEASE_FILE} bytes"
            ));
        }
        return Ok(data);
    }
    Err(format!("{name} not found in the release image"))
}
