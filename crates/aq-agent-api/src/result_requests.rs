//! Request bodies of the project-scoped result reads (`result.read`): the
//! attempt's project always comes from its binding, never from these bodies.
use serde::Deserialize;
use uuid::Uuid;

/// `api_version` of the request, which only 1 satisfies.
pub trait Versioned {
    fn api_version(&self) -> u32;
}
/// The request in `body`, or the agent error code that refuses it: malformed
/// or unknown fields are `invalid_request`, any `api_version` but 1 is
/// `unsupported_api_version`.
pub fn parse<Q: for<'de> Deserialize<'de> + Versioned>(body: &[u8]) -> Result<Q, &'static str> {
    let request: Q = serde_json::from_slice(body).map_err(|_| "invalid_request")?;
    match request.api_version() {
        1 => Ok(request),
        _ => Err("unsupported_api_version"),
    }
}
macro_rules! versioned {
    ($($name:ident),*) => {$(impl Versioned for $name {
        fn api_version(&self) -> u32 {
            self.api_version
        }
    })*};
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct List {
    pub api_version: u32,
    pub after: Option<Uuid>,
    pub limit: Option<u32>,
}
/// One exact revision of a report.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Show {
    pub api_version: u32,
    pub report_id: Uuid,
    pub revision: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Patch {
    pub api_version: u32,
    pub report_id: Uuid,
    pub revision: String,
    /// Raw patch bytes the caller will accept: 1..=ceiling, default the ceiling.
    pub max_bytes: Option<u64>,
}
versioned!(List, Show, Patch);
impl Patch {
    /// The byte bound that applies, or `None` when the caller's is outside 1..=`ceiling`.
    pub fn bound(&self, ceiling: u64) -> Option<u64> {
        let bound = self.max_bytes.unwrap_or(ceiling);
        (1..=ceiling).contains(&bound).then_some(bound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_patch_bound_defaults_to_and_never_exceeds_the_ceiling() {
        let patch = |max_bytes| Patch {
            api_version: 1,
            report_id: Uuid::nil(),
            revision: String::new(),
            max_bytes,
        };
        assert_eq!(patch(None).bound(100), Some(100));
        assert_eq!(patch(Some(1)).bound(100), Some(1));
        assert_eq!(patch(Some(100)).bound(100), Some(100));
        assert_eq!(patch(Some(0)).bound(100), None);
        assert_eq!(patch(Some(101)).bound(100), None);
    }
    #[test]
    fn unknown_fields_are_refused() {
        let id = "00000000-0000-0000-0000-000000000001";
        let body = |extra: &str| {
            format!(r#"{{"api_version":1,"report_id":"{id}","revision":"x"{extra}}}"#)
        };
        assert!(serde_json::from_str::<Show>(&body("")).is_ok());
        assert!(serde_json::from_str::<Show>(&body(r#","project_id":"{id}""#)).is_err());
    }
}
