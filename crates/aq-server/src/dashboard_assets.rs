//! Optional startup-verified immutable dashboard bundle; never a filesystem server.
mod manifest;
use crate::{AppState, authz::CurrentUser, error::AppError};
use anyhow::{Context, Result, ensure};
use axum::{
    Router,
    body::{Body, Bytes},
    extract::{FromRequestParts, Request, State},
    http::{HeaderValue, header},
    response::{IntoResponse, Response},
    routing::get,
};
use manifest::Manifest;
use serde::{
    Deserialize,
    de::{MapAccess, Visitor},
};
use sha2::{Digest as _, Sha256};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::Read,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt},
        },
    },
    path::Path,
    sync::Arc,
};
const MAX_FILES: usize = 512;
const MAX_TOTAL: u64 = 32 * 1024 * 1024;
const MAX_MANIFEST: u64 = 256 * 1024;
struct Files(BTreeMap<String, Entry>);
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    sha256: String,
    bytes: u64,
}
impl<'de> Deserialize<'de> for Files {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct Entries;
        impl<'de> Visitor<'de> for Entries {
            type Value = Files;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a bounded unique asset map")
            }
            fn visit_map<M: MapAccess<'de>>(
                self,
                mut m: M,
            ) -> std::result::Result<Files, M::Error> {
                let mut files = BTreeMap::new();
                while let Some((name, entry)) = m.next_entry::<String, Entry>()? {
                    if files.len() >= MAX_FILES || files.insert(name, entry).is_some() {
                        return Err(serde::de::Error::custom(
                            "duplicate asset or file count limit",
                        ));
                    }
                }
                Ok(Files(files))
            }
        }
        d.deserialize_map(Entries)
    }
}
#[derive(Clone)]
struct Asset {
    bytes: Bytes,
    mime: &'static str,
    cache_control: &'static str,
}
/// A configured trusted operator bundle, verified once before background owners
/// start. Cloned responses share bounded bytes; requests never open a pathname.
pub struct DashboardAssets {
    digest: String,
    files: BTreeMap<String, Asset>,
    manifest: Asset,
}
impl DashboardAssets {
    /// Lowercase SHA256 of the exact verified manifest bytes, not a re-encoding.
    pub fn digest(&self) -> &str {
        &self.digest
    }
    pub fn load(directory: &Path) -> Result<Self> {
        ensure!(
            !directory.as_os_str().is_empty(),
            "dashboard directory must not be empty"
        );
        let root = OpenOptions::new()
            .read(true)
            .custom_flags(
                nix::libc::O_DIRECTORY
                    | nix::libc::O_NOFOLLOW
                    | nix::libc::O_NONBLOCK
                    | nix::libc::O_CLOEXEC,
            )
            .open(directory)
            .context("dashboard root directory unavailable")?;
        let original = root.metadata()?;
        let bytes = read_file(&root, "dashboard-manifest.json", MAX_MANIFEST, None)?;
        let manifest: Manifest =
            serde_json::from_slice(&bytes).context("invalid dashboard manifest")?;
        manifest.validate_compatibility()?;
        ensure!(
            !manifest.files.0.is_empty() && manifest.files.0.contains_key("index.html"),
            "dashboard index missing"
        );
        let mut total = 0u64;
        for (name, entry) in &manifest.files.0 {
            ensure!(
                mime(name).is_some(),
                "unsupported dashboard asset path/type"
            );
            ensure!(
                entry.sha256.len() == 64
                    && entry
                        .sha256
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "dashboard digest must be lowercase SHA256"
            );
            total = total
                .checked_add(entry.bytes)
                .context("dashboard size overflow")?;
            ensure!(total <= MAX_TOTAL, "dashboard bundle byte quota");
        }
        let assets = if manifest.files.0.keys().any(|p| p.starts_with("assets/")) {
            Some(open_at(&root, "assets", true)?)
        } else {
            None
        };
        let mut files = BTreeMap::new();
        for (name, entry) in manifest.files.0 {
            let bytes = if let Some(file) = name.strip_prefix("assets/") {
                read_file(
                    assets
                        .as_ref()
                        .context("dashboard assets directory missing")?,
                    file,
                    entry.bytes,
                    Some(entry.bytes),
                )?
            } else {
                read_file(&root, &name, entry.bytes, Some(entry.bytes))?
            };
            ensure!(
                hex::encode(Sha256::digest(&bytes)) == entry.sha256,
                "dashboard asset integrity mismatch"
            );
            ensure!(
                name != "index.html" || !bytes.is_empty(),
                "empty dashboard index"
            );
            files.insert(
                name.clone(),
                Asset {
                    bytes: Bytes::from(bytes),
                    mime: mime(&name).context("dashboard MIME missing")?,
                    cache_control: asset_cache_control(&name),
                },
            );
        }
        let current = std::fs::symlink_metadata(directory)?;
        ensure!(
            current.is_dir() && (current.dev(), current.ino()) == (original.dev(), original.ino()),
            "dashboard root changed while loading"
        );
        Ok(Self {
            files,
            digest: hex::encode(Sha256::digest(&bytes)),
            manifest: Asset {
                bytes: Bytes::from(bytes),
                mime: "application/json",
                cache_control: "no-cache",
            },
        })
    }
}
fn open_at(directory: &File, name: &str, is_directory: bool) -> Result<File> {
    ensure!(
        !name.is_empty()
            && !name.contains('/')
            && !name.contains('\0')
            && name != "."
            && name != "..",
        "dashboard member name"
    );
    let name = std::ffi::CString::new(std::ffi::OsStr::new(name).as_bytes())?;
    let flags = nix::libc::O_RDONLY
        | nix::libc::O_NOFOLLOW
        | nix::libc::O_NONBLOCK
        | nix::libc::O_CLOEXEC
        | if is_directory {
            nix::libc::O_DIRECTORY
        } else {
            0
        };
    let fd = unsafe { nix::libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
    ensure!(fd >= 0, "dashboard member unavailable");
    Ok(unsafe { File::from_raw_fd(fd) })
}
fn read_file(directory: &File, name: &str, limit: u64, exact: Option<u64>) -> Result<Vec<u8>> {
    ensure!(limit <= MAX_TOTAL, "dashboard file bound");
    let file = open_at(directory, name, false)?;
    let before = file.metadata()?;
    ensure!(
        before.is_file()
            && before.nlink() == 1
            && before.len() <= limit
            && exact.is_none_or(|n| n == before.len()),
        "dashboard member must be a bounded regular single-link file"
    );
    let mut bytes = Vec::with_capacity(before.len() as usize);
    file.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit && exact.is_none_or(|n| n == bytes.len() as u64),
        "dashboard file changed size"
    );
    Ok(bytes)
}
fn mime(name: &str) -> Option<&'static str> {
    if name == "index.html" {
        return Some("text/html; charset=utf-8");
    }
    let file = name.strip_prefix("assets/")?;
    if file.is_empty()
        || file.len() > 240
        || !file
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        || matches!(file, "." | "..")
    {
        return None;
    }
    let (base, extension) = file.rsplit_once('.')?;
    if base.is_empty() {
        return None;
    }
    match extension {
        "js" => Some("text/javascript; charset=utf-8"),
        "css" => Some("text/css; charset=utf-8"),
        "woff" => Some("font/woff"),
        "woff2" => Some("font/woff2"),
        "png" => Some("image/png"),
        "svg" => Some("image/svg+xml"),
        "jpg" => Some("image/jpeg"),
        "webp" => Some("image/webp"),
        "ico" => Some("image/x-icon"),
        _ => None,
    }
}
fn segment(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && !matches!(value, "." | "..")
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}
fn spa(path: &str) -> bool {
    if path.len() > 512 {
        return false;
    }
    let Some(path) = path.strip_prefix('/') else {
        return false;
    };
    match path.split('/').collect::<Vec<_>>().as_slice() {
        [""]
        | [
            "overview" | "supervisor" | "work" | "results" | "decisions" | "agents" | "playbooks"
            | "knowledge" | "settings",
        ] => true,
        ["work" | "decisions" | "agents" | "knowledge", id] | ["playbooks", "runs", id] => {
            segment(id)
        }
        ["results", id, "revisions", revision] => segment(id) && segment(revision),
        _ => false,
    }
}
fn asset_cache_control(name: &str) -> &'static str {
    let Some(base) = name
        .strip_prefix("assets/")
        .and_then(|file| file.rsplit_once('.').map(|(base, _)| base))
    else {
        return "no-cache";
    };
    // Vite's content hashes use URL-safe base64, including '_' and '-'.
    if base.match_indices('-').any(|(offset, _)| {
        let hash = &base[offset + 1..];
        offset > 0
            && hash.len() >= 8
            && hash
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
    }) {
        "private, max-age=31536000, immutable"
    } else {
        "no-cache"
    }
}
fn handler(asset: Asset, spa_route: bool) -> axum::routing::MethodRouter<AppState> {
    get(move |State(state): State<AppState>, request: Request| {
        let asset = asset.clone();
        async move {
            if spa_route && !spa(request.uri().path()) {
                return Err(AppError::NotFound("no such route".into()));
            }
            let (mut parts, _) = request.into_parts();
            CurrentUser::from_request_parts(&mut parts, &state).await?;
            let mut response = Body::from(asset.bytes.clone()).into_response();
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static(asset.mime));
            response
                .headers_mut()
                .insert(header::CONTENT_LENGTH, HeaderValue::from(asset.bytes.len()));
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static(asset.cache_control),
            );
            Ok::<Response, AppError>(response)
        }
    })
}
pub(crate) fn routes(assets: Arc<DashboardAssets>) -> Router<AppState> {
    let index = assets
        .files
        .get("index.html")
        .expect("validated dashboard index")
        .clone();
    let mut router = Router::new()
        .route("/index.html", handler(index.clone(), false))
        .route(
            "/dashboard-manifest.json",
            handler(assets.manifest.clone(), false),
        );
    for path in [
        "/",
        "/overview",
        "/supervisor",
        "/work",
        "/work/{taskId}",
        "/results",
        "/results/{resultId}/revisions/{revision}",
        "/decisions",
        "/decisions/{decisionId}",
        "/agents",
        "/agents/{sessionId}",
        "/playbooks",
        "/playbooks/runs/{runId}",
        "/knowledge",
        "/knowledge/{nodeId}",
        "/settings",
    ] {
        router = router.route(path, handler(index.clone(), true));
    }
    for (name, asset) in &assets.files {
        if name.starts_with("assets/") {
            router = router.route(&format!("/{name}"), handler(asset.clone(), false));
        }
    }
    router
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::{PermissionsExt, symlink};
    pub(super) fn bundle() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::create_dir(root.path().join("assets")).unwrap();
        std::fs::write(
            root.path().join("index.html"),
            b"<html>trusted dashboard</html>",
        )
        .unwrap();
        std::fs::write(root.path().join("assets/app.js"), b"console.log('fixture')").unwrap();
        let entries: [(&str, &[u8]); 2] = [
            ("index.html", b"<html>trusted dashboard</html>"),
            ("assets/app.js", b"console.log('fixture')"),
        ];
        let files: serde_json::Map<String, serde_json::Value> = entries
            .into_iter()
            .map(|(p, b)| {
                (
                    p.into(),
                    json!({"sha256":hex::encode(Sha256::digest(b)),"bytes":b.len()}),
                )
            })
            .collect();
        std::fs::write(
            root.path().join("dashboard-manifest.json"),
            serde_json::to_vec(
                &json!({"schema_version":1,"api_version":"dashboard-read-1","files":files}),
            )
            .unwrap(),
        )
        .unwrap();
        root
    }
    #[test]
    fn verified_exact_manifest_digest_and_loaded_bytes_survive_backing_changes() {
        let root = bundle();
        let manifest = std::fs::read(root.path().join("dashboard-manifest.json")).unwrap();
        let assets = DashboardAssets::load(root.path()).unwrap();
        assert_eq!(assets.digest(), hex::encode(Sha256::digest(&manifest)));
        std::fs::write(root.path().join("index.html"), b"replacement").unwrap();
        std::fs::remove_file(root.path().join("assets/app.js")).unwrap();
        assert_eq!(
            &assets.files["index.html"].bytes[..],
            b"<html>trusted dashboard</html>"
        );
        assert_eq!(
            &assets.files["assets/app.js"].bytes[..],
            b"console.log('fixture')"
        );
    }
    #[test]
    fn corrupt_length_versions_unknown_keys_and_duplicate_entries_refuse() {
        for mode in [
            "digest",
            "length",
            "schema",
            "api",
            "unknown",
            "entry-unknown",
            "duplicate",
        ] {
            let root = bundle();
            let path = root.path().join("dashboard-manifest.json");
            let mut manifest: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            match mode {
                "digest" => manifest["files"]["index.html"]["sha256"] = json!("0".repeat(64)),
                "length" => manifest["files"]["index.html"]["bytes"] = json!(1),
                "schema" => manifest["schema_version"] = json!(2),
                "api" => manifest["api_version"] = json!("dashboard-read-2"),
                "unknown" => manifest["unexpected"] = json!(1),
                "entry-unknown" => manifest["files"]["index.html"]["unexpected"] = json!(1),
                _ => {}
            }
            let mut bytes = serde_json::to_string(&manifest).unwrap();
            if mode == "duplicate" {
                bytes=bytes.replace("\"files\":{","\"files\":{\"index.html\":{\"bytes\":1,\"sha256\":\"0000000000000000000000000000000000000000000000000000000000000000\"},");
            }
            std::fs::write(path, bytes).unwrap();
            assert!(DashboardAssets::load(root.path()).is_err(), "{mode}");
        }
    }
    #[test]
    fn member_symlink_hardlink_fifo_and_symlink_directory_refuse_without_blocking() {
        for mode in [
            "file-symlink",
            "hardlink",
            "fifo",
            "directory-symlink",
            "root-symlink",
        ] {
            let root = bundle();
            let path = root.path().join("assets/app.js");
            match mode {
                "file-symlink" => {
                    std::fs::remove_file(&path).unwrap();
                    symlink("../index.html", &path).unwrap();
                }
                "hardlink" => std::fs::hard_link(&path, root.path().join("other")).unwrap(),
                "fifo" => {
                    std::fs::remove_file(&path).unwrap();
                    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
                    assert_eq!(unsafe { nix::libc::mkfifo(name.as_ptr(), 0o600) }, 0);
                }
                "directory-symlink" => {
                    std::fs::rename(root.path().join("assets"), root.path().join("moved")).unwrap();
                    symlink("moved", root.path().join("assets")).unwrap();
                }
                _ => {
                    let parent = tempfile::tempdir().unwrap();
                    let link = parent.path().join("linked");
                    symlink(root.path(), &link).unwrap();
                    assert!(DashboardAssets::load(&link).is_err());
                    continue;
                }
            }
            assert!(DashboardAssets::load(root.path()).is_err(), "{mode}");
        }
    }
    #[test]
    fn manifest_and_total_quotas_and_exact_path_grammar_refuse() {
        assert!(
            DashboardAssets::load(Path::new(""))
                .err()
                .unwrap()
                .to_string()
                .contains("must not be empty")
        );
        let root = bundle();
        let path = root.path().join("dashboard-manifest.json");
        let mut files = serde_json::Map::new();
        for i in 0..513 {
            files.insert(
                format!("assets/{i}.js"),
                json!({"bytes":1,"sha256":"0".repeat(64)}),
            );
        }
        std::fs::write(
            &path,
            serde_json::to_vec(
                &json!({"schema_version":1,"api_version":"dashboard-read-1","files":files}),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(
            format!("{:#}", DashboardAssets::load(root.path()).err().unwrap())
                .contains("file count limit")
        );
        std::fs::write(&path,serde_json::to_vec(&json!({"schema_version":1,"api_version":"dashboard-read-1","files":{"index.html":{"bytes":MAX_TOTAL+1,"sha256":"0".repeat(64)}}})).unwrap()).unwrap();
        assert!(
            DashboardAssets::load(root.path())
                .err()
                .unwrap()
                .to_string()
                .contains("bundle byte quota")
        );
        std::fs::write(&path, vec![b' '; MAX_MANIFEST as usize + 1]).unwrap();
        assert!(DashboardAssets::load(root.path()).is_err());
        for p in [
            "assets/../index.html",
            "/index.html",
            "assets/nested/app.js",
            "assets/app.js.map",
            "assets/.map",
            "assets/.js",
            "assets/app.html",
            "assets/a%2ejs",
            "assets/{id}.js",
        ] {
            assert!(mime(p).is_none(), "{p}");
        }
        assert!(spa(&format!("/work/{}", "a".repeat(128))));
        assert!(!spa(&format!("/work/{}", "a".repeat(129))));
        for p in [
            "/work/..",
            "/work/.",
            "/work/%2e%2e",
            "/work/a/b",
            "/api/missing",
            "/projects",
            "/index.html",
            "/settings/",
            "/assets/app.js",
        ] {
            assert!(!spa(p), "{p}");
        }
    }
}

#[cfg(test)]
mod release_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_hash_named_assets_are_immutable() {
        for name in [
            "assets/index-abcd1234.js",
            "assets/TaskGraph-CA-h6BEc.js",
            "assets/agents-040M5_Uq.js",
            "assets/index-ABCD1234.css",
        ] {
            assert_eq!(
                asset_cache_control(name),
                "private, max-age=31536000, immutable"
            );
        }
        for name in [
            "index.html",
            "dashboard-manifest.json",
            "assets/app.js",
            "assets/app-short.js",
            "assets/short.js",
            "assets/.js",
        ] {
            assert_eq!(asset_cache_control(name), "no-cache");
        }
    }
    #[test]
    fn schema_two_checks_required_contracts_capabilities_and_provenance() {
        let root = super::tests::bundle();
        let path = root.path().join("dashboard-manifest.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        manifest["schema_version"] = json!(2);
        manifest["version"] = json!("0.1.0");
        manifest["source"] = json!({"revision":"a".repeat(40), "dirty":false, "official":true});
        manifest["toolchain"] =
            json!({"node":"v24.15.0", "pnpm":"10.34.5", "lockfile_sha256":"b".repeat(64)});
        manifest["required_api_versions"] = json!(["dashboard-read-1", "dashboard-actions-1"]);
        manifest["required_capabilities"] =
            json!(["dashboard.overview", "tasks.read", "results.media.read"]);
        std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert!(DashboardAssets::load(root.path()).is_ok());
        for mode in [
            "api",
            "capability",
            "unimplemented-capability",
            "missing",
            "missing-version",
            "missing-source",
            "missing-toolchain",
            "missing-contracts",
            "no-read-contract",
            "revision",
            "lockfile",
            "dirty-official",
            "unknown",
            "future",
        ] {
            let mut invalid = manifest.clone();
            match mode {
                "api" => invalid["required_api_versions"] = json!(["dashboard-read-2"]),
                "capability" => invalid["required_capabilities"] = json!(["future.capability"]),
                "unimplemented-capability" => {
                    invalid["required_capabilities"] = json!(["supervisor.conversation"])
                }
                "missing-version" => {
                    invalid.as_object_mut().unwrap().remove("version");
                }
                "missing-source" => {
                    invalid.as_object_mut().unwrap().remove("source");
                }
                "missing-toolchain" => {
                    invalid.as_object_mut().unwrap().remove("toolchain");
                }
                "missing-contracts" => {
                    invalid
                        .as_object_mut()
                        .unwrap()
                        .remove("required_api_versions");
                }
                "no-read-contract" => {
                    invalid["required_api_versions"] = json!(["dashboard-actions-1"])
                }
                "missing" => {
                    invalid
                        .as_object_mut()
                        .unwrap()
                        .remove("required_capabilities");
                }
                "revision" => invalid["source"]["revision"] = json!("main"),
                "lockfile" => invalid["toolchain"]["lockfile_sha256"] = json!("bad"),
                "dirty-official" => invalid["source"]["dirty"] = json!(true),
                "unknown" => invalid["toolchain"]["unexpected"] = json!(true),
                "future" => invalid["schema_version"] = json!(3),
                _ => unreachable!(),
            }
            std::fs::write(&path, serde_json::to_vec(&invalid).unwrap()).unwrap();
            assert!(DashboardAssets::load(root.path()).is_err(), "{mode}");
        }
        manifest["source"]["dirty"] = json!(true);
        manifest["source"]["official"] = json!(false);
        std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert!(DashboardAssets::load(root.path()).is_ok());
    }
}
