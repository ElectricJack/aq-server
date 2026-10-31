//! Router-local bounded read cursors. Tokens authenticate membership; ACL is
//! independently rechecked on every page. Restart forgets snapshots safely.
use super::{Error, Result, contract::*};
use chacha20poly1305::{
    ChaCha20Poly1305, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::{collections::BTreeMap, sync::Mutex, time::Duration};
use tokio::sync::Semaphore;
use uuid::Uuid;
use zeroize::Zeroizing;
pub const MEMBERS: usize = 256;
pub const PAGE: u16 = 25;
pub const MAX_PAGE: u16 = 100;
pub const TTL: u64 = 30000;
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Resource {
    Sessions,
    Projects,
}
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub id: Uuid,
    pub actor: Uuid,
    pub partition: String,
    pub scope: Scope,
    pub resource: Resource,
    pub ids: Vec<Uuid>,
    pub truncated: bool,
    pub expires: u64,
    pub created: u64,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    snapshot: Uuid,
    actor: Uuid,
    partition: String,
    scope: Scope,
    resource: Resource,
    offset: u16,
    limit: u16,
    expires: u64,
}
pub struct Runtime {
    pub pool: PgPool,
    pub gate: Semaphore,
    key: Zeroizing<[u8; 32]>,
    snapshots: Mutex<BTreeMap<Uuid, Snapshot>>,
}
impl Runtime {
    pub fn new(state: &crate::AppState) -> Self {
        let mut key = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut key);
        let options = state
            .store
            .pool
            .connect_options()
            .as_ref()
            .clone()
            .application_name("aq-dashboard-read");
        Self {
            pool: PgPoolOptions::new()
                .max_connections(2)
                .acquire_timeout(Duration::from_millis(500))
                .connect_lazy_with(options),
            gate: Semaphore::new(4),
            key: Zeroizing::new(key),
            snapshots: Mutex::new(BTreeMap::new()),
        }
    }
    pub fn insert(&self, mut snapshot: Snapshot, now: u64) -> Result<Snapshot> {
        if snapshot.ids.len() > MEMBERS {
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
        let cursor = Cursor {
            snapshot: s.id,
            actor: s.actor,
            partition: s.partition.clone(),
            scope: s.scope.clone(),
            resource: s.resource,
            offset: offset.try_into().map_err(|_| Error::Invalid)?,
            limit,
            expires: s.expires,
        };
        let bytes = serde_json::to_vec(&cursor).map_err(|_| Error::Unavailable)?;
        if bytes.len() > 1024 {
            return Err(Error::Unavailable);
        }
        let mut nonce = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut nonce);
        let cipher = ChaCha20Poly1305::new((&*self.key).into());
        let encrypted = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &bytes,
                    aad: b"aq.dashboard-read-1.cursor",
                },
            )
            .map_err(|_| Error::Unavailable)?;
        let mut token = nonce.to_vec();
        token.extend(encrypted);
        Ok(hex::encode(token))
    }
    pub fn resume(
        &self,
        token: &str,
        actor: Uuid,
        partition: &str,
        target: (&Scope, Resource),
        limit: u16,
        now: u64,
    ) -> Result<(Snapshot, usize)> {
        let (scope, resource) = target;
        if token.len() > 4096 || token.len() < 56 {
            return Err(Error::Restart);
        }
        let token = hex::decode(token).map_err(|_| Error::Restart)?;
        let cipher = ChaCha20Poly1305::new((&*self.key).into());
        let bytes = cipher
            .decrypt(
                Nonce::from_slice(&token[..12]),
                Payload {
                    msg: &token[12..],
                    aad: b"aq.dashboard-read-1.cursor",
                },
            )
            .map_err(|_| Error::Restart)?;
        let c: Cursor = serde_json::from_slice(&bytes).map_err(|_| Error::Restart)?;
        if c.actor != actor || c.partition != partition {
            return Err(Error::AuthorizationChanged);
        }
        if &c.scope != scope || c.resource != resource || c.limit != limit || c.expires <= now {
            return Err(Error::Restart);
        }
        let all = self.snapshots.lock().map_err(|_| Error::Unavailable)?;
        let s = all
            .get(&c.snapshot)
            .filter(|s| s.expires > now)
            .ok_or(Error::Restart)?;
        if s.actor != actor
            || s.partition != partition
            || &s.scope != scope
            || s.resource != resource
            || s.expires != c.expires
            || c.offset as usize > s.ids.len()
        {
            return Err(Error::Restart);
        }
        Ok((s.clone(), c.offset as usize))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn runtime() -> Runtime {
        Runtime {
            pool: PgPoolOptions::new()
                .connect_lazy("postgres://localhost/unused")
                .unwrap(),
            gate: Semaphore::new(4),
            key: Zeroizing::new([7; 32]),
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
        assert_eq!(
            r.resume(
                &t,
                actor,
                "1",
                (&Scope::AllAuthorized, Resource::Sessions),
                1,
                2
            )
            .unwrap()
            .1,
            1
        );
        for (who, partition, scope, resource, limit, now) in [
            (
                Uuid::new_v4(),
                "1",
                Scope::AllAuthorized,
                Resource::Sessions,
                1,
                2,
            ),
            (actor, "2", Scope::AllAuthorized, Resource::Sessions, 1, 2),
            (
                actor,
                "1",
                Scope::Project {
                    project_id: Uuid::new_v4(),
                },
                Resource::Sessions,
                1,
                2,
            ),
            (actor, "1", Scope::AllAuthorized, Resource::Projects, 1, 2),
            (actor, "1", Scope::AllAuthorized, Resource::Sessions, 2, 2),
            (
                actor,
                "1",
                Scope::AllAuthorized,
                Resource::Sessions,
                1,
                TTL + 1,
            ),
        ] {
            assert!(
                r.resume(&t, who, partition, (&scope, resource), limit, now)
                    .is_err()
            );
        }
        let mut restarted = runtime();
        restarted.key = Zeroizing::new([8; 32]);
        assert!(
            restarted
                .resume(
                    &t,
                    actor,
                    "1",
                    (&Scope::AllAuthorized, Resource::Sessions),
                    1,
                    2
                )
                .is_err()
        );
        let mut bad = t.into_bytes();
        bad[30] = if bad[30] == b'a' { b'b' } else { b'a' };
        assert!(
            r.resume(
                std::str::from_utf8(&bad).unwrap(),
                actor,
                "1",
                (&Scope::AllAuthorized, Resource::Sessions),
                1,
                2
            )
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
        assert!(
            r.resume(
                &token,
                actor,
                "1",
                (&Scope::AllAuthorized, Resource::Sessions),
                1,
                6
            )
            .is_err()
        );
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
