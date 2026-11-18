//! The observation catalog entry of a launch, carried as one `manifest.modules` reference
//! beside the pinned documents (`OfferedLaunch` and `NativeOffer` keep their bytes; a launch
//! without an entry has no such module). The server appends it when the owner's accounting
//! catalog covers the selected provider and harness; the worker reads it back and derives the
//! source identity the server enrolls. A worker that does not know the module ignores it.
//!
//! The module carries the whole entry, so no retained bytes need to be fetched:
//! id `aq.observation-source/v1/<entry digest>/<entry id>`, version = the entry's version,
//! digest = SHA-256 of the entry's canonical JSON (which is also the retained body).
//!
//! Both sides derive the source identity here, so they cannot disagree. The launch-time
//! source is generation 1 (the server's source generation, not the assignment generation).
use crate::{ArtifactRef, Digest, Invalid, Result, Text};
use uuid::Uuid;

pub const MODULE_PREFIX: &str = "aq.observation-source/v1/";
pub const SOURCE_DOMAIN: &str = "aq.observation-source/v1";

/// SHA-256 of the sorted-key compact JSON `[SOURCE_DOMAIN, attempt, generation, entry]`, first
/// 16 bytes, with version 8 and the RFC 4122 variant. Deterministic, so enrollment is idempotent.
pub fn source_id(attempt: Uuid, generation: u64, entry: &ArtifactRef) -> Result<Uuid> {
    let mut value = serde_json::json!([SOURCE_DOMAIN, attempt, generation, entry]);
    value.sort_all_objects();
    let bytes = serde_json::to_vec(&value).map_err(|_| Invalid("observation source"))?;
    let digest = Digest::of(&bytes);
    let mut id = [0u8; 16];
    for (i, b) in id.iter_mut().enumerate() {
        *b = u8::from_str_radix(&digest.as_str()[i * 2..i * 2 + 2], 16)
            .map_err(|_| Invalid("observation source digest"))?;
    }
    id[6] = (id[6] & 0x0f) | 0x80;
    id[8] = (id[8] & 0x3f) | 0x80;
    Ok(Uuid::from_bytes(id))
}

/// The session key event provenance carries: `aq.attempt/<attempt>/<generation>`.
pub fn session_key(attempt: Uuid, generation: u64) -> Result<Text> {
    Text::new(format!("aq.attempt/{attempt}/{generation}"))
        .map_err(|_| Invalid("observation session key"))
}

fn body(entry: &ArtifactRef) -> Result<Vec<u8>> {
    let mut value = serde_json::to_value(entry).map_err(|_| Invalid("observation source"))?;
    value.sort_all_objects();
    serde_json::to_vec(&value).map_err(|_| Invalid("observation source"))
}

/// The module reference for `entry` and the body the server retains under it.
pub fn module(entry: &ArtifactRef) -> Result<(ArtifactRef, Vec<u8>)> {
    let body = body(entry)?;
    let id = format!(
        "{MODULE_PREFIX}{}/{}",
        entry.digest.as_str(),
        entry.id.as_str()
    );
    let module = ArtifactRef {
        id: Text::new(id).map_err(|_| Invalid("observation source id too long"))?,
        version: entry.version.clone(),
        digest: Digest::of(&body),
    };
    Ok((module, body))
}

/// The catalog entry among a manifest's modules. `None` when the launch delivers none; more
/// than one, or a module that does not reproduce its own digest, is refused.
pub fn entry(modules: &[ArtifactRef]) -> Result<Option<ArtifactRef>> {
    let mut found = modules
        .iter()
        .filter(|m| m.id.as_str().starts_with(MODULE_PREFIX));
    let Some(delivered) = found.next() else {
        return Ok(None);
    };
    if found.next().is_some() {
        return Err(Invalid("more than one observation source module"));
    }
    let (digest, id) = delivered.id.as_str()[MODULE_PREFIX.len()..]
        .split_once('/')
        .ok_or(Invalid("observation source module id"))?;
    let entry = ArtifactRef {
        id: Text::new(id).map_err(|_| Invalid("observation source module id"))?,
        version: delivered.version.clone(),
        digest: serde_json::from_value(digest.into())
            .map_err(|_| Invalid("observation source module digest"))?,
    };
    if module(&entry)?.0 != *delivered {
        return Err(Invalid("observation source module digest"));
    }
    Ok(Some(entry))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// aq-worker `profiled_source_id_matches_server_derivation_vector`.
    #[test]
    fn source_identity_matches_the_worker_vector() {
        let entry: ArtifactRef = serde_json::from_value(serde_json::json!({
            "id": "aq.observation-catalog/codex", "version": "1", "digest": "ab".repeat(32)
        }))
        .unwrap();
        let attempt = Uuid::from_u128(4);
        assert_eq!(
            source_id(attempt, 1, &entry).unwrap().to_string(),
            "2070c132-6c63-8df1-925e-6ebd36112419"
        );
        assert_eq!(
            session_key(attempt, 1).unwrap().as_str(),
            "aq.attempt/00000000-0000-0000-0000-000000000004/1"
        );
    }

    #[test]
    fn module_round_trips_and_refuses_forgery_and_duplicates() {
        let entry_ref = ArtifactRef {
            id: Text::new("aq.accounting.source/anthropic-claude").unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(b"catalog"),
        };
        let (module_ref, body) = module(&entry_ref).unwrap();
        // The pinned vector the worker's mirror must reproduce.
        assert_eq!(
            String::from_utf8(body).unwrap(),
            format!(
                r#"{{"digest":"{}","id":"aq.accounting.source/anthropic-claude","version":"1"}}"#,
                entry_ref.digest.as_str()
            )
        );
        assert_eq!(
            module_ref.id.as_str(),
            format!(
                "aq.observation-source/v1/{}/aq.accounting.source/anthropic-claude",
                entry_ref.digest.as_str()
            )
        );
        let other = ArtifactRef {
            id: Text::new("aq.startup-delivery/x").unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(b"other"),
        };
        assert_eq!(entry(std::slice::from_ref(&other)).unwrap(), None);
        assert_eq!(
            entry(&[other.clone(), module_ref.clone()]).unwrap(),
            Some(entry_ref.clone())
        );
        let mut forged = module_ref.clone();
        forged.version = Text::new("2").unwrap();
        assert!(entry(&[forged]).is_err());
        let mut second = entry_ref.clone();
        second.id = Text::new("another").unwrap();
        assert!(entry(&[module_ref, module(&second).unwrap().0]).is_err());
        let mut long = entry_ref;
        long.id = Text::new("x".repeat(500)).unwrap();
        assert!(module(&long).is_err());
    }
}
