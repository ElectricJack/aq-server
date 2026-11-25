//! Router-local bounded read cursors. Tokens authenticate membership; ACL is
//! independently rechecked on every page. Restart forgets snapshots safely.
use super::cursor::{Binding, CursorCodec, CursorError};
use super::{Error, Result, contract::*};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::{collections::BTreeMap, sync::Mutex, time::Duration};
use tokio::sync::Semaphore;
use uuid::Uuid;
pub const MEMBERS: usize = 256;
pub const PAGE: u16 = 25;
pub const MAX_PAGE: u16 = 100;
pub const TTL: u64 = 30000;
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Resource {
    Sessions,
    Projects,
    Search,
}
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub id: Uuid,
    pub actor: Uuid,
    pub partition: String,
    pub scope: Scope,
    pub resource: Resource,
    pub ids: Vec<Uuid>,
    pub filters: serde_json::Value,
    pub search: Option<super::search::SnapshotData>,
    pub truncated: bool,
    pub expires: u64,
    pub created: u64,
}
#[derive(Clone, Copy, PartialEq)]
pub(super) struct PageKey<'a> {
    pub actor: Uuid,
    pub partition: &'a str,
    pub scope: &'a Scope,
    pub resource: Resource,
    pub limit: u16,
    pub filters: &'a serde_json::Value,
}

impl PageKey<'_> {
    fn binding(&self) -> Binding {
        Binding {
            principal: self.actor,
            authorization_partition: self.partition.into(),
            scope: serde_json::json!(self.scope),
            filters: self.filters.clone(),
            sort: "snapshot_membership,id".into(),
            page_size: self.limit,
            resource: format!("{:?}", self.resource),
        }
    }
}

impl Snapshot {
    fn page_key(&self, limit: u16) -> PageKey<'_> {
        PageKey {
            actor: self.actor,
            partition: &self.partition,
            scope: &self.scope,
            resource: self.resource,
            limit,
            filters: &self.filters,
        }
    }

    fn member_count(&self) -> usize {
        self.search
            .as_ref()
            .map_or(self.ids.len(), |s| s.hits.len())
    }
}

pub struct Runtime {
    pub reviews: Option<std::sync::Arc<crate::reviews::Service>>,
    pub pool: PgPool,
    pub(super) performance_pool: PgPool,
    pub gate: Semaphore,
    pub quota: super::quota::Collector,
    pub(super) codec: CursorCodec,
    snapshots: Mutex<BTreeMap<Uuid, Snapshot>>,
}
impl Runtime {
    pub fn new(state: &crate::AppState) -> Self {
        let options = state
            .store
            .pool
            .connect_options()
            .as_ref()
            .clone()
            .application_name("aq-dashboard-read");
        let performance_options = options
            .clone()
            .application_name("aq-performance-read")
            .options([
                ("statement_timeout", "3500ms"),
                ("lock_timeout", "500ms"),
                ("default_transaction_read_only", "on"),
            ]);
        Self {
            performance_pool: PgPoolOptions::new()
                .max_connections(3)
                .acquire_timeout(Duration::from_millis(700))
                .connect_lazy_with(performance_options),
            pool: PgPoolOptions::new()
                .max_connections(2)
                .acquire_timeout(Duration::from_millis(500))
                .connect_lazy_with(options),
            gate: Semaphore::new(4),
            reviews: None,
            quota: super::quota::Collector::new(),
            codec: CursorCodec::default(),
            snapshots: Mutex::new(BTreeMap::new()),
        }
    }
    pub fn insert(&self, mut snapshot: Snapshot, now: u64) -> Result<Snapshot> {
        if snapshot.ids.len() > MEMBERS
            || snapshot
                .search
                .as_ref()
                .is_some_and(|s| s.hits.len() > MEMBERS)
        {
            return Err(Error::Unavailable);
        }
        let mut all = self.snapshots.lock().map_err(|_| Error::Unavailable)?;
        all.retain(|_, s| s.expires > now);
        let own = all.values().filter(|s| s.actor == snapshot.actor).count();
        if own >= 4 {
            let oldest = all
                .values()
                .filter(|s| s.actor == snapshot.actor)
                .min_by_key(|s| (s.created, s.id))
                .map(|s| s.id)
                .ok_or(Error::Unavailable)?;
            all.remove(&oldest);
        }
        if all.len() >= 64 {
            return Err(Error::Unavailable);
        }
        snapshot.id = Uuid::new_v4();
        all.insert(snapshot.id, snapshot.clone());
        Ok(snapshot)
    }
    pub fn next(&self, s: &Snapshot, offset: usize, limit: u16) -> Result<String> {
        if offset > s.member_count() {
            return Err(Error::Invalid);
        }
        self.codec
            .encode(s.page_key(limit).binding().position(
                &s.id.to_string(),
                &offset.to_string(),
                s.created..s.expires,
            ))
            .map_err(|_| Error::Unavailable)
    }
    pub(super) fn resume(
        &self,
        token: &str,
        key: PageKey<'_>,
        now: u64,
    ) -> Result<(Snapshot, usize)> {
        let c = self
            .codec
            .decode(token, &key.binding(), now)
            .map_err(|error| match error {
                CursorError::AuthorizationChanged => Error::AuthorizationChanged,
                CursorError::RestartRequired => Error::Restart,
                CursorError::Unavailable => Error::Unavailable,
            })?;
        let snapshot = c.snapshot.parse::<Uuid>().map_err(|_| Error::Restart)?;
        let offset = c.position.parse::<usize>().map_err(|_| Error::Restart)?;
        let all = self.snapshots.lock().map_err(|_| Error::Unavailable)?;
        let s = all
            .get(&snapshot)
            .filter(|s| s.expires > now)
            .ok_or(Error::Restart)?;
        let same_page = s.page_key(key.limit) == key && s.expires == c.expires;
        if !same_page || offset > s.member_count() {
            return Err(Error::Restart);
        }
        Ok((s.clone(), offset))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn runtime() -> Runtime {
        Runtime {
            performance_pool: PgPoolOptions::new()
                .connect_lazy("postgres://localhost/unused")
                .unwrap(),
            pool: PgPoolOptions::new()
                .connect_lazy("postgres://localhost/unused")
                .unwrap(),
            gate: Semaphore::new(4),
            reviews: None,
            quota: super::super::quota::Collector::new(),
            codec: CursorCodec::default(),
            snapshots: Mutex::new(BTreeMap::new()),
        }
    }
    fn snapshot(actor: Uuid, now: u64) -> Snapshot {
        Snapshot {
            id: Uuid::nil(),
            actor,
            partition: "1".into(),
            scope: Scope::AllAuthorized,
            resource: Resource::Sessions,
            ids: vec![Uuid::new_v4(); 3],
            filters: serde_json::json!({}),
            search: None,
            truncated: false,
            expires: now + TTL,
            created: now,
        }
    }
    #[tokio::test]
    async fn cursor_authenticates_actor_partition_scope_resource_limit_expiry_and_bytes() {
        let r = runtime();
        let actor = Uuid::new_v4();
        let s = r.insert(snapshot(actor, 1), 1).unwrap();
        let t = r.next(&s, 1, 1).unwrap();
        let key = s.page_key(1);
        assert_eq!(r.resume(&t, key, 2).unwrap().1, 1);
        let project_scope = Scope::Project {
            project_id: Uuid::new_v4(),
        };
        for changed in [
            PageKey {
                actor: Uuid::new_v4(),
                ..key
            },
            PageKey {
                partition: "2",
                ..key
            },
            PageKey {
                scope: &project_scope,
                ..key
            },
            PageKey {
                resource: Resource::Projects,
                ..key
            },
            PageKey { limit: 2, ..key },
        ] {
            assert!(r.resume(&t, changed, 2).is_err());
        }
        assert!(r.resume(&t, key, TTL + 1).is_err());
        let mut restarted = runtime();
        restarted.codec = CursorCodec::default();
        assert!(restarted.resume(&t, key, 2).is_err());
        let mut bad = t.into_bytes();
        bad[30] = if bad[30] == b'a' { b'b' } else { b'a' };
        assert!(
            r.resume(std::str::from_utf8(&bad).unwrap(), key, 2)
                .is_err()
        );
    }
    #[tokio::test]
    async fn snapshot_caps_are_per_principal_and_global_without_cross_actor_eviction() {
        let r = runtime();
        let actor = Uuid::new_v4();
        let first = r.insert(snapshot(actor, 1), 1).unwrap();
        let token = r.next(&first, 1, 1).unwrap();
        for n in 2..=5 {
            r.insert(snapshot(actor, n), n).unwrap();
        }
        assert!(r.resume(&token, first.page_key(1), 6).is_err());
        let mut too_many = snapshot(actor, 6);
        too_many.ids = vec![Uuid::new_v4(); MEMBERS + 1];
        assert!(r.insert(too_many, 6).is_err());
        assert_eq!(r.snapshots.lock().unwrap().len(), 4);
        for n in 0..60 {
            r.insert(snapshot(Uuid::new_v4(), 10 + n), 10 + n).unwrap();
        }
        assert!(r.insert(snapshot(Uuid::new_v4(), 100), 100).is_err());
        assert_eq!(r.snapshots.lock().unwrap().len(), 64);
    }
}
