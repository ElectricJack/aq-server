//! Names, media types, and declared limits of the files in a dashboard release.
use anyhow::{Context, Result, ensure};
use serde::{
    Deserialize,
    de::{MapAccess, Visitor},
};
use std::collections::BTreeMap;

const MAX_FILES: usize = 512;
pub const MAX_TOTAL: u64 = 32 * 1024 * 1024;
pub struct Files(BTreeMap<String, Entry>);
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub sha256: String,
    pub bytes: u64,
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
impl Files {
    /// Check the complete declaration before the loader opens any listed member.
    pub fn validated(self) -> Result<BTreeMap<String, Entry>> {
        ensure!(self.0.contains_key("index.html"), "dashboard index missing");
        let mut total = 0u64;
        for (name, entry) in &self.0 {
            ensure!(
                mime(name).is_some(),
                "unsupported dashboard asset path/type"
            );
            ensure!(
                super::lowercase_hex(&entry.sha256, 64),
                "dashboard digest must be lowercase SHA256"
            );
            total = total
                .checked_add(entry.bytes)
                .context("dashboard size overflow")?;
            ensure!(total <= MAX_TOTAL, "dashboard bundle byte quota");
        }
        Ok(self.0)
    }
}
pub fn valid_segment(value: &str, max_bytes: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_bytes
        && !matches!(value, "." | "..")
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}
pub fn mime(name: &str) -> Option<&'static str> {
    if name == "index.html" {
        return Some("text/html; charset=utf-8");
    }
    let file = name.strip_prefix("assets/")?;
    if !valid_segment(file, 240) {
        return None;
    }
    let (base, extension) = file.rsplit_once('.')?;
    if base.is_empty() {
        return None;
    }
    const CONTENT_TYPES: &[(&str, &str)] = &[
        ("js", "text/javascript; charset=utf-8"),
        ("css", "text/css; charset=utf-8"),
        ("woff", "font/woff"),
        ("woff2", "font/woff2"),
        ("png", "image/png"),
        ("svg", "image/svg+xml"),
        ("jpg", "image/jpeg"),
        ("webp", "image/webp"),
        ("ico", "image/x-icon"),
    ];
    CONTENT_TYPES
        .iter()
        .find(|(ext, _)| *ext == extension)
        .map(|(_, mime)| *mime)
}
pub fn asset_cache_control(name: &str) -> &'static str {
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
