//! Checks over aq-git's public source observations, moved out of the
//! aq-server profiled source reader. Pure: no I/O and no decision. A `false`
//! or `None` means the observation does not prove what the caller needs.
mod head;
pub use head::SourceHead;
#[cfg(feature = "source_observation")]
pub mod observation;
use serde_json::{Value, json};
use uuid::Uuid;

/// 40 lowercase hex digits, not all zero: a SHA-1 object name.
pub fn hex40(s: &str) -> bool {
    s.len() == 40
        && s.bytes().any(|b| b != b'0')
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Repository identity used for fixed aq-git operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Repository {
    pub id: Uuid,
    pub generation: u64,
}
pub fn commit_matches(value: &Value, hex: &str) -> bool {
    let oid = json!({"algorithm":"sha1","hex":hex});
    let tree = &value["tree"];
    value["oid"] == oid && tree["algorithm"] == "sha1" && hex40(tree["hex"].as_str().unwrap_or(""))
}
/// The commit a complete, original, fresh `repo.observation` reports for
/// `reference` at `generation`, as of `now`.
pub fn resolve_ref(value: &Value, reference: &str, generation: u64, now: u64) -> Option<String> {
    let observation = RefObservation(value);
    let completed = *observation.interval()?.end();
    if completed > now || now - completed > 30_000 {
        return None;
    }
    observation.oid(reference, generation).map(str::to_owned)
}

/// The retained SHA-1 observation of this exact ref and generation, without a
/// freshness requirement. Adoption also checks the forge's current default.
pub(crate) fn observed_ref<'a>(
    value: &'a Value,
    reference: &str,
    generation: u64,
) -> Option<&'a str> {
    let observation = RefObservation(value);
    observation.interval()?;
    observation.oid(reference, generation)
}

struct RefObservation<'a>(&'a Value);
impl<'a> RefObservation<'a> {
    fn oid(&self, reference: &str, generation: u64) -> Option<&'a str> {
        let scoped = self.covers(reference) && self.has_identity(generation);
        let success = self.0.get("last_refresh_error") == Some(&Value::Null);
        if !scoped || !success {
            return None;
        }
        let hex = self.0["refs"][reference].as_str()?;
        hex40(hex).then_some(hex)
    }
    fn covers(&self, reference: &str) -> bool {
        self.0["covered_refs"]
            .as_array()
            .is_some_and(|refs| refs.len() <= 128 && refs.contains(&json!(reference)))
    }
    fn has_identity(&self, generation: u64) -> bool {
        let id = self.0["observation_id"]
            .as_str()
            .and_then(|s| Uuid::parse_str(s).ok());
        id.is_some_and(|id| !id.is_nil()) && self.0["binding_generation"] == generation
    }
    fn interval(&self) -> Option<std::ops::RangeInclusive<u64>> {
        let start = self.0["started_at_ms"].as_u64()?;
        let end = self.0["completed_at_ms"].as_u64()?;
        (start > 0 && start <= end).then_some(start..=end)
    }
}
/// A `retention.inspect` reply proves `handle` is an exact, active, complete
/// retention of the commit `hex` in this repository binding.
pub fn retention_holds(value: &Value, handle: Uuid, repository: Repository, hex: &str) -> bool {
    let Some(refs) = value["refs"].as_array() else {
        return false;
    };
    let record = crate::source_transport::RetentionRecord(value);
    let matches =
        record.matches_handle(handle, repository.id, repository.generation) && record.unreleased();
    let active = value["generation"].as_u64().is_some_and(|g| g > 0) && value["state"] == "active";
    matches && active && retained_refs(refs, hex)
}
fn retained_refs(refs: &[Value], hex: &str) -> bool {
    let mut names = std::collections::BTreeSet::new();
    let valid = refs.iter().all(|r| {
        let named = r["name"]
            .as_str()
            .is_some_and(|name| (1..=4096).contains(&name.len()) && names.insert(name));
        named && hex40(r["oid"].as_str().unwrap_or(""))
    });
    valid && refs.len() <= 256 && refs.iter().any(|r| r["oid"] == hex)
}
/// The logical identity a `repo.show` reply gives this central SHA-1
/// repository binding.
pub fn repository_identity(value: &Value, repository: Uuid, generation: u64) -> Option<String> {
    let identity = value["identity"].as_str()?;
    let bound = value["repository_id"] == repository.to_string()
        && value["binding_generation"] == generation;
    let central = value["mode"] == "central" && value["object_format"] == "sha1";
    let valid_name = !identity.trim().is_empty()
        && identity.len() <= 512
        && !identity.chars().any(char::is_control);
    if ![bound, central, valid_name].into_iter().all(|valid| valid) {
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
        let binding = Repository {
            id: repository,
            generation: 2,
        };
        let hex = "a".repeat(40);
        let valid = json!({"version":1,"id":handle,"repository_id":repository,"binding_generation":2,"generation":1,"state":"active","refs":[{"name":"refs/aq-git/checkpoints/test","oid":hex}],"release_operation":null,"release_reason":null});
        assert!(retention_holds(&valid, handle, binding, &hex));
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
            assert!(!retention_holds(&value, handle, binding, &hex), "{field}");
        }
        let mut incomplete = valid.clone();
        incomplete
            .as_object_mut()
            .unwrap()
            .remove("release_operation");
        assert!(!retention_holds(&incomplete, handle, binding, &hex));
    }
}
