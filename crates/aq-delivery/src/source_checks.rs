//! Checks over aq-git's public source observations, moved out of the
//! aq-server profiled source reader. Pure: no I/O and no decision. A `false`
//! or `None` means the observation does not prove what the caller needs.
use serde_json::{Value, json};
use uuid::Uuid;

/// 40 lowercase hex digits, not all zero: a SHA-1 object name.
pub fn hex40(s: &str) -> bool {
    s.len() == 40
        && s.bytes().any(|b| b != b'0')
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The commit a complete, original, fresh `repo.observation` reports for
/// `reference` at `generation`, as of `now`.
pub fn resolve_ref(value: &Value, reference: &str, generation: u64, now: u64) -> Option<String> {
    let completed = value["completed_at_ms"].as_u64()?;
    let started = value["started_at_ms"].as_u64()?;
    if started == 0
        || completed < started
        || completed > now
        || now - completed > 30000
        || value["binding_generation"] != generation
        || value["observation_id"]
            .as_str()
            .and_then(|s| Uuid::parse_str(s).ok())
            .is_none_or(|id| id.is_nil())
        || value["covered_refs"]
            .as_array()
            .is_none_or(|a| a.len() > 128 || !a.contains(&json!(reference)))
        || value.get("last_refresh_error").is_none_or(|v| !v.is_null())
    {
        return None;
    }
    let hex = value["refs"][reference].as_str()?;
    hex40(hex).then(|| hex.into())
}

/// A `retention.inspect` reply proves `handle` is an exact, active, complete
/// retention of the commit `hex` in this repository binding.
pub fn retention_holds(
    value: &Value,
    handle: Uuid,
    repository: Uuid,
    generation: u64,
    hex: &str,
) -> bool {
    let Some(refs) = value["refs"].as_array() else {
        return false;
    };
    let mut names = std::collections::BTreeSet::new();
    if refs.iter().any(|r| {
        r["name"]
            .as_str()
            .is_none_or(|name| name.is_empty() || name.len() > 4096 || !names.insert(name))
            || !hex40(r["oid"].as_str().unwrap_or(""))
    }) {
        return false;
    }
    !(value["version"] != 1
        || value["id"] != handle.to_string()
        || value["repository_id"] != repository.to_string()
        || value["binding_generation"] != generation
        || value["generation"].as_u64().is_none_or(|g| g == 0)
        || value["state"] != "active"
        || refs.len() > 256
        || !refs.iter().any(|r| r["oid"] == hex)
        || value.get("release_operation").is_none_or(|v| !v.is_null())
        || value.get("release_reason").is_none_or(|v| !v.is_null()))
}

/// The logical identity a `repo.show` reply gives this central SHA-1
/// repository binding.
pub fn repository_identity(value: &Value, repository: Uuid, generation: u64) -> Option<String> {
    let identity = value["identity"].as_str()?;
    if value["repository_id"] != repository.to_string()
        || value["binding_generation"] != generation
        || value["mode"] != "central"
        || value["object_format"] != "sha1"
        || identity.trim().is_empty()
        || identity.len() > 512
        || identity.chars().any(char::is_control)
    {
        return None;
    }
    Some(identity.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reference_observation_requires_original_complete_fresh_exact_generation() {
        let reference = "refs/heads/main";
        let valid = json!({"observation_id":Uuid::new_v4(),"binding_generation":2,"started_at_ms":1000,"completed_at_ms":1100,"covered_refs":[reference],"refs":{reference:"a".repeat(40)},"last_refresh_error":null});
        assert_eq!(
            resolve_ref(&valid, reference, 2, 1200),
            Some("a".repeat(40))
        );
        for (field, bad) in [
            ("observation_id", json!(Uuid::nil())),
            ("binding_generation", json!(3)),
            ("started_at_ms", json!(1200)),
            ("completed_at_ms", json!(1300)),
            ("covered_refs", json!([])),
            ("last_refresh_error", json!({"code":"offline"})),
        ] {
            let mut value = valid.clone();
            value[field] = bad;
            assert!(resolve_ref(&value, reference, 2, 1200).is_none(), "{field}");
        }
        assert!(resolve_ref(&valid, reference, 2, 31_101).is_none());
    }
    #[test]
    fn retention_is_exact_active_complete_and_never_inferred_from_missing_fields() {
        let handle = Uuid::new_v4();
        let repository = Uuid::new_v4();
        let hex = "a".repeat(40);
        let valid = json!({"version":1,"id":handle,"repository_id":repository,"binding_generation":2,"generation":1,"state":"active","refs":[{"name":"refs/aq-git/checkpoints/test","oid":hex}],"release_operation":null,"release_reason":null});
        assert!(retention_holds(&valid, handle, repository, 2, &hex));
        for (field, bad) in [
            ("state", json!("released")),
            ("id", json!(Uuid::new_v4())),
            ("repository_id", json!(Uuid::new_v4())),
            ("binding_generation", json!(3)),
            ("generation", json!(0)),
            ("refs", json!([])),
            ("release_operation", json!(Uuid::new_v4())),
        ] {
            let mut value = valid.clone();
            value[field] = bad;
            assert!(
                !retention_holds(&value, handle, repository, 2, &hex),
                "{field}"
            );
        }
        let mut incomplete = valid.clone();
        incomplete
            .as_object_mut()
            .unwrap()
            .remove("release_operation");
        assert!(!retention_holds(&incomplete, handle, repository, 2, &hex));
    }
}
