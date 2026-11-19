use super::*;
use crate::dashboard_api::cursor::{Binding as SharedBinding, CursorCodec};
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Binding {
    pub actor: Uuid,
    pub project: Uuid,
    pub revision: u64,
    pub workspace: Uuid,
    pub kind: String,
    pub limit: u16,
    pub q: Option<String>,
}
pub(super) struct Codec {
    shared: CursorCodec,
}
impl Codec {
    pub fn new() -> Self {
        Self {
            shared: CursorCodec::default(),
        }
    }
    fn binding(b: &Binding) -> SharedBinding {
        SharedBinding {
            principal: b.actor,
            authorization_partition: b.revision.to_string(),
            scope: serde_json::json!({"project": b.project, "workspace": b.workspace}),
            filters: serde_json::json!({"q": b.q}),
            sort: "graph_owner_order,id".into(),
            page_size: b.limit,
            resource: b.kind.clone(),
        }
    }
    pub fn encode(&self, b: &Binding, upstream: &str, now: i64) -> Result<String> {
        self.encode_for(b, upstream, now, 30000)
    }
    pub fn encode_for(&self, b: &Binding, upstream: &str, now: i64, ttl: u64) -> Result<String> {
        if upstream.is_empty() || upstream.len() > 256 || upstream.chars().any(char::is_control) {
            return Err(unavailable());
        }
        let now = u64::try_from(now).map_err(|_| unavailable())?;
        self.shared
            .encode(
                &Self::binding(b),
                "graph_owner_snapshot",
                upstream,
                now,
                now.checked_add(ttl).ok_or_else(unavailable)?,
            )
            .map_err(|_| unavailable())
    }
    pub fn decode(&self, token: &str, b: &Binding, now: i64) -> Result<String> {
        if token.len() > 4096 {
            return Err(invalid());
        }
        self.shared
            .decode(
                token,
                &Self::binding(b),
                u64::try_from(now).map_err(|_| conflict())?,
            )
            .map(|v| v.position)
            .map_err(|_| conflict())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cursors_bind_actor_project_revision_filters_clock_and_boot() {
        let c = Codec::new();
        let b = Binding {
            actor: Uuid::new_v4(),
            project: Uuid::new_v4(),
            revision: 1,
            workspace: Uuid::new_v4(),
            kind: "task".into(),
            limit: 1,
            q: None,
        };
        let token = c.encode(&b, "private-upstream-cursor", 1000).unwrap();
        assert_eq!(
            c.decode(&token, &b, 1001).unwrap(),
            "private-upstream-cursor"
        );
        assert!(!token.contains("private-upstream-cursor"));
        for changed in [
            Binding {
                actor: Uuid::new_v4(),
                ..b.clone()
            },
            Binding {
                revision: 2,
                ..b.clone()
            },
            Binding {
                project: Uuid::new_v4(),
                ..b.clone()
            },
            Binding {
                kind: "knowledge".into(),
                ..b.clone()
            },
            Binding {
                limit: 2,
                ..b.clone()
            },
            Binding {
                q: Some("different".into()),
                ..b.clone()
            },
        ] {
            assert!(c.decode(&token, &changed, 1001).is_err())
        }
        assert!(c.decode(&token, &b, 999).is_err());
        assert!(c.decode(&token, &b, 31000).is_err());
        assert!(Codec::new().decode(&token, &b, 1001).is_err());
        let mut tampered = token.into_bytes();
        tampered[20] = if tampered[20] == b'A' { b'B' } else { b'A' };
        assert!(
            c.decode(std::str::from_utf8(&tampered).unwrap(), &b, 1001)
                .is_err()
        );
    }
}
