//! C4 environment templates: the operator's C3b allowlist, and resolving an
//! execution profile's environment to exactly one allowlisted image digest
//! when an offer is made. Arbitrary images are never accepted; a missing or
//! ambiguous entry resolves to nothing. Nothing is stored: a running attempt
//! keeps the digest frozen in its launch manifest.
use aq_admission::Digest;
use serde::Deserialize;
use std::sync::Arc;

const MAX_ALLOWLIST_BYTES: u64 = 1024 * 1024;
const MAX_ENTRIES: usize = 1024;

#[derive(Debug)]
pub struct Allowlist {
    entries: Vec<Entry>,
}
#[derive(Debug, Clone)]
struct Entry {
    name: String,
    version: String,
    digest: Digest,
}
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Resolution {
    pub environment: String,
    pub template_digest: Digest,
}

#[derive(Deserialize)]
struct File {
    schema_version: u32,
    entries: Vec<FileEntry>,
}
/// Provenance fields (harness versions, base image, build recipe) belong to
/// aq-worker and are not interpreted here.
#[derive(Deserialize)]
struct FileEntry {
    name: String,
    version: String,
    image_digest: String,
}

impl FileEntry {
    fn into_entry(self) -> Result<Entry, &'static str> {
        fn bounded(s: &str) -> bool {
            !s.is_empty() && s.len() <= 128 && !s.contains('@') && !s.chars().any(char::is_control)
        }
        let hex = self
            .image_digest
            .strip_prefix("sha256:")
            .ok_or("template image_digest must be sha256:<hex>")?;
        let digest = serde_json::from_value::<Digest>(serde_json::Value::String(hex.into()))
            .map_err(|_| "template image_digest must be sha256:<64 lowercase hex>")?;
        if !bounded(&self.name) || !bounded(&self.version) {
            return Err("template name or version invalid");
        }
        Ok(Entry {
            name: self.name,
            version: self.version,
            digest,
        })
    }
}

impl Allowlist {
    pub fn parse(bytes: &[u8]) -> Result<Self, &'static str> {
        if bytes.len() as u64 > MAX_ALLOWLIST_BYTES {
            return Err("template allowlist exceeds 1 MiB");
        }
        let file: File =
            serde_json::from_slice(bytes).map_err(|_| "template allowlist is not valid JSON")?;
        if file.schema_version != 1 {
            return Err("unsupported template allowlist schema_version");
        }
        if file.entries.len() > MAX_ENTRIES {
            return Err("template allowlist exceeds 1024 entries");
        }
        let entries = file
            .entries
            .into_iter()
            .map(FileEntry::into_entry)
            .collect::<Result<_, _>>()?;
        Ok(Self { entries })
    }
    /// Each entry's environment name, version and image digest, in file order.
    pub fn entries(&self) -> impl Iterator<Item = (&str, &str, &Digest)> {
        (self.entries.iter()).map(|e| (e.name.as_str(), e.version.as_str(), &e.digest))
    }
}

/// Pins each environment reference (`name@version` exactly, or a bare `name`)
/// to one digest. Duplicate entries naming one digest are not ambiguous.
pub fn resolve<'a>(
    environments: impl IntoIterator<Item = &'a str>,
    allowlist: &Allowlist,
) -> Result<Vec<Resolution>, &'static str> {
    let mut out: Vec<Resolution> = Vec::new();
    for environment in environments {
        if out.iter().any(|r| r.environment == environment) {
            continue;
        }
        let (name, version) = match environment.split_once('@') {
            Some((name, version)) => (name, Some(version)),
            None => (environment, None),
        };
        let mut digests: Vec<&Digest> = allowlist
            .entries
            .iter()
            .filter(|e| e.name == name && version.is_none_or(|v| e.version == v))
            .map(|e| &e.digest)
            .collect();
        digests.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        digests.dedup();
        match digests.as_slice() {
            [] => return Err("environment_template_missing"),
            [digest] => out.push(Resolution {
                environment: environment.into(),
                template_digest: (*digest).clone(),
            }),
            _ => return Err("environment_template_ambiguous"),
        }
    }
    out.sort_by(|a, b| a.environment.cmp(&b.environment));
    Ok(out)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    allowlist: std::path::PathBuf,
}

/// `[templates] allowlist = "<path>"`, read and validated once at startup.
pub fn deserialize<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<Arc<Allowlist>>, D::Error> {
    Option::<Config>::deserialize(d)?
        .map(|config| {
            use std::io::Read;
            let file = std::fs::File::open(&config.allowlist)
                .map_err(|_| serde::de::Error::custom("template allowlist unavailable"))?;
            let mut bytes = Vec::new();
            file.take(MAX_ALLOWLIST_BYTES + 1)
                .read_to_end(&mut bytes)
                .map_err(serde::de::Error::custom)?;
            Allowlist::parse(&bytes)
                .map(Arc::new)
                .map_err(serde::de::Error::custom)
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn hex(c: char) -> String {
        std::iter::repeat_n(c, 64).collect()
    }
    fn list(entries: &[(&str, &str, char)]) -> Allowlist {
        let entries: Vec<_> = entries
            .iter()
            .map(|(n, v, d)| serde_json::json!({"name":n,"version":v,"image_digest":format!("sha256:{}",hex(*d)),"harness_versions":{"codex":"1"}}))
            .collect();
        Allowlist::parse(
            &serde_json::to_vec(&serde_json::json!({"schema_version":1,"entries":entries}))
                .unwrap(),
        )
        .unwrap()
    }
    #[test]
    fn resolves_each_environment_to_exactly_one_digest() {
        let allowlist = list(&[
            ("ci", "1", 'a'),
            ("ci", "2", 'b'),
            ("dev", "1", 'c'),
            ("dev", "1", 'c'),
            ("dup", "1", 'd'),
            ("dup", "1", 'e'),
        ]);
        let got = resolve(["ci@2", "dev", "dev@1"], &allowlist).unwrap();
        assert_eq!(
            got.iter()
                .map(|r| (
                    r.environment.as_str(),
                    r.template_digest.as_str().to_owned()
                ))
                .collect::<Vec<_>>(),
            [("ci@2", hex('b')), ("dev", hex('c')), ("dev@1", hex('c'))]
        );
        assert_eq!(
            resolve(["ci"], &allowlist),
            Err("environment_template_ambiguous")
        );
        assert_eq!(
            resolve(["dup@1"], &allowlist),
            Err("environment_template_ambiguous")
        );
        assert_eq!(
            resolve(["ci@3"], &allowlist),
            Err("environment_template_missing")
        );
        assert_eq!(
            resolve(["absent"], &allowlist),
            Err("environment_template_missing")
        );
        for bad in [
            serde_json::json!({"schema_version":2,"entries":[]}),
            serde_json::json!({"schema_version":1,"entries":[{"name":"x","version":"1","image_digest":hex('a')}]}),
            serde_json::json!({"schema_version":1,"entries":[{"name":"x","version":"1","image_digest":"sha256:ABC"}]}),
            serde_json::json!({"schema_version":1,"entries":[{"name":"x@1","version":"1","image_digest":format!("sha256:{}",hex('a'))}]}),
        ] {
            assert!(
                Allowlist::parse(&serde_json::to_vec(&bad).unwrap()).is_err(),
                "{bad}"
            );
        }
    }
}
