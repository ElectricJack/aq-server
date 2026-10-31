//! PostgreSQL persistence for a single-owner engine namespace.
//!
//! Every runner boundary is one compare-and-swap transaction. The snapshot
//! contains receipts, waits, children, events and operation intents together.
//! Immutable canonical artifacts are retained through explicit dependency/audit
//! references and safe disposal boundaries. Session advisory locks prevent two connections
//! from owning the same namespace, even if they supply the same owner name.
//! This does not fence external hosts: retire a prior runner before replacement.
mod indexes;
mod profiling;
mod projection;
mod protection;
use profiling::ObservedTransaction;
pub use profiling::{OperationProfile, StageProfile, StorageProfile};
mod retention;
use aq_playbooks_model::{Artifact, canonical_bytes};
use aq_playbooks_runner::{
    State,
    storage::{Storage, StorageError, VersionedState},
};
use postgres::{Client, NoTls};
pub use protection::ProtectionKeyring;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    sync::{Mutex, MutexGuard},
};
use zeroize::Zeroizing;

const MIGRATIONS: &[(i32, &str)] = &[
    (1, include_str!("../migrations/0001_initial.sql")),
    (
        2,
        include_str!("../migrations/0002_indexes_and_protection.sql"),
    ),
];
const MIGRATION_LOCK: i64 = 0x4151504c41594d47;

struct Lease {
    owner: String,
    incarnation: i64,
}
struct Connection {
    client: Client,
    leases: BTreeMap<String, Lease>,
}

/// Holds one database session for its lifetime. Dropping it releases all locks;
/// reopening increments each acquired namespace's durable runtime incarnation.
pub struct PostgresStorage {
    connection: Mutex<Connection>,
    keyring: Option<ProtectionKeyring>,
    profiling: profiling::Sink,
}
struct SnapshotEnvelope {
    plaintext: Option<Value>,
    key_id: Option<String>,
    nonce: Option<Vec<u8>>,
    ciphertext: Option<Vec<u8>>,
}

fn backend(error: impl std::fmt::Display) -> StorageError {
    StorageError::Backend(error.to_string())
}
fn lock_key(namespace: &str) -> i64 {
    let digest = Sha256::digest(format!("aq-playbooks/namespace/{namespace}"));
    i64::from_be_bytes(digest[..8].try_into().expect("eight digest bytes"))
}
fn check_name(name: &str) -> Result<(), StorageError> {
    if name.is_empty() || name.len() > 512 || name.contains('\0') {
        return Err(backend(
            "namespace and owner must contain 1..512 bytes and no NUL",
        ));
    }
    Ok(())
}

impl PostgresStorage {
    /// Connects and transactionally applies the crate-owned schema migrations.
    /// Credentials are never included in adapter error messages. This convenience
    /// constructor uses NoTls; use `from_client` with a host-configured TLS client
    /// for remote production databases requiring transport encryption.
    pub fn connect(config: &str) -> Result<Self, StorageError> {
        Self::from_client(Client::connect(config, NoTls).map_err(backend)?)
    }

    pub fn connect_protected(
        config: &str,
        keyring: ProtectionKeyring,
    ) -> Result<Self, StorageError> {
        Self::from_client_protected(Client::connect(config, NoTls).map_err(backend)?, keyring)
    }
    pub fn from_client(client: Client) -> Result<Self, StorageError> {
        Self::initialize(client, None)
    }
    pub fn from_client_protected(
        client: Client,
        keyring: ProtectionKeyring,
    ) -> Result<Self, StorageError> {
        Self::initialize(client, Some(keyring))
    }
    fn initialize(
        mut client: Client,
        keyring: Option<ProtectionKeyring>,
    ) -> Result<Self, StorageError> {
        let mut tx = client.transaction().map_err(backend)?;
        tx.query_one("SELECT pg_advisory_xact_lock($1)", &[&MIGRATION_LOCK])
            .map_err(backend)?;
        tx.batch_execute("CREATE SCHEMA IF NOT EXISTS aq_playbooks; CREATE TABLE IF NOT EXISTS aq_playbooks.schema_migrations (version integer PRIMARY KEY, checksum bytea NOT NULL, applied_at timestamptz NOT NULL DEFAULT clock_timestamp());").map_err(backend)?;
        let versions = tx
            .query(
                "SELECT version,checksum FROM aq_playbooks.schema_migrations ORDER BY version",
                &[],
            )
            .map_err(backend)?;
        for (offset, row) in versions.iter().enumerate() {
            let Some((version, sql)) = MIGRATIONS.get(offset) else {
                return Err(backend("unsupported aq-playbooks schema migration"));
            };
            if row.get::<_, i32>(0) != *version
                || row.get::<_, Vec<u8>>(1) != Sha256::digest(sql.as_bytes()).to_vec()
            {
                return Err(backend(
                    "unsupported or altered aq-playbooks schema migration",
                ));
            }
        }
        for (version, sql) in MIGRATIONS.iter().skip(versions.len()) {
            tx.batch_execute(sql).map_err(backend)?;
            let checksum = Sha256::digest(sql.as_bytes()).to_vec();
            tx.execute(
                "INSERT INTO aq_playbooks.schema_migrations(version,checksum) VALUES($1,$2)",
                &[version, &checksum],
            )
            .map_err(backend)?;
        }
        tx.commit().map_err(backend)?;
        Ok(Self {
            connection: Mutex::new(Connection {
                client,
                leases: BTreeMap::new(),
            }),
            keyring,
            profiling: Default::default(),
        })
    }
    fn encode(
        &self,
        namespace: &str,
        revision: u64,
        state: &State,
    ) -> Result<SnapshotEnvelope, StorageError> {
        let mut value = serde_json::to_value(state).map_err(backend)?;
        value["artifacts"] = serde_json::json!({});
        if let Some(keyring) = &self.keyring {
            let plaintext = Zeroizing::new(serde_json::to_vec(&value).map_err(backend)?);
            let sealed = keyring.seal(namespace, revision, &plaintext)?;
            Ok(SnapshotEnvelope {
                plaintext: None,
                key_id: Some(sealed.key_id),
                nonce: Some(sealed.nonce),
                ciphertext: Some(sealed.ciphertext),
            })
        } else {
            Ok(SnapshotEnvelope {
                plaintext: Some(value),
                key_id: None,
                nonce: None,
                ciphertext: None,
            })
        }
    }
    fn decode(
        &self,
        namespace: &str,
        revision: u64,
        row: &postgres::Row,
    ) -> Result<State, StorageError> {
        let plaintext: Option<Value> = row.get("snapshot");
        if let Some(key_id) = row.get::<_, Option<String>>("protection_key_id") {
            if plaintext.is_some() {
                return Err(backend("invalid protected snapshot envelope"));
            }
            let keyring = self
                .keyring
                .as_ref()
                .ok_or_else(|| backend("protected namespace requires configured keys"))?;
            let nonce: Vec<u8> = row.get("protection_nonce");
            let ciphertext: Vec<u8> = row.get("encrypted_snapshot");
            let bytes = keyring.open(namespace, revision, &key_id, &nonce, &ciphertext)?;
            serde_json::from_slice(&bytes)
                .map_err(|_| backend("invalid authenticated snapshot format"))
        } else {
            serde_json::from_value(plaintext.ok_or_else(|| backend("missing snapshot payload"))?)
                .map_err(backend)
        }
    }
    fn read_artifacts(
        &self,
        tx: &mut ObservedTransaction<'_>,
        namespace: &str,
        protected: bool,
    ) -> Result<BTreeMap<String, Artifact>, StorageError> {
        let mut artifacts = BTreeMap::new();
        for artifact_row in tx.query("SELECT n.digest,a.canonical_bytes,e.key_id,e.nonce,e.ciphertext FROM aq_playbooks.namespace_artifacts n LEFT JOIN aq_playbooks.artifacts a ON a.digest=n.digest LEFT JOIN aq_playbooks.encrypted_artifacts e ON e.namespace=n.namespace AND e.digest=n.digest WHERE n.namespace=$1 ORDER BY n.digest",&[&namespace]).map_err(backend)?{
            let digest:String=artifact_row.get(0);
            let bytes=if protected{let key_id:Option<String>=artifact_row.get(2);let key_id=key_id.ok_or_else(||backend("missing protected artifact envelope"))?;let nonce:Vec<u8>=artifact_row.get(3);let ciphertext:Vec<u8>=artifact_row.get(4);self.keyring.as_ref().ok_or_else(||backend("protected namespace requires configured keys"))?.open_artifact(namespace,&digest,&key_id,&nonce,&ciphertext)?}else{Zeroizing::new(artifact_row.get::<_,Option<Vec<u8>>>(1).ok_or_else(||backend("missing plaintext artifact payload"))?)};
            let started=tx.clock();let artifact=Artifact::from_canonical_bytes(&bytes).map_err(backend)?;tx.verified(started);if artifact.digest!=digest{return Err(backend("artifact address integrity mismatch"));}artifacts.insert(digest,artifact);
        }
        Ok(artifacts)
    }
    /// Disabled by default. Value-free fixed-stage aggregate diagnostics only.
    pub fn set_profiling(&self, enabled: bool) {
        if let Ok(mut p) = self.profiling.lock() {
            *p = enabled.then(StorageProfile::default);
        }
    }
    pub fn profiling_snapshot(&self) -> Option<StorageProfile> {
        self.profiling.lock().ok().and_then(|p| p.clone())
    }
    pub fn reencrypt_namespace(&self, namespace: &str, owner: &str) -> Result<u64, StorageError> {
        if self.keyring.is_none() {
            return Err(backend("key rotation requires configured protection"));
        }
        let current = self.load(namespace)?;
        self.commit_state(namespace, owner, current.revision, &current.state, true)
    }

    fn connection(&self) -> Result<MutexGuard<'_, Connection>, StorageError> {
        self.connection
            .lock()
            .map_err(|_| backend("storage connection mutex poisoned"))
    }

    /// Durable incarnation is useful in diagnostics; it does not confer host
    /// authority or claim external-system fencing.
    pub fn incarnation(&self, namespace: &str) -> Result<Option<u64>, StorageError> {
        let mut conn = self.connection()?;
        conn.client
            .query_opt(
                "SELECT incarnation FROM aq_playbooks.namespaces WHERE namespace=$1",
                &[&namespace],
            )
            .map_err(backend)
            .map(|row| row.map(|r| r.get::<_, i64>(0) as u64))
    }
}

impl Storage for PostgresStorage {
    fn protection_available(&self) -> bool {
        self.keyring.is_some()
    }
    fn query(
        &self,
        namespace: &str,
        query: &aq_playbooks_runner::IndexQuery,
    ) -> Result<aq_playbooks_runner::IndexPage, StorageError> {
        check_name(namespace)?;
        let mut conn = self.connection()?;
        if conn
            .client
            .query_opt(
                "SELECT indexes_materialized FROM aq_playbooks.namespaces WHERE namespace=$1",
                &[&namespace],
            )
            .map_err(backend)?
            .is_some_and(|row| !row.get::<_, bool>(0))
        {
            return Err(backend(
                "namespace indexes require exclusive owner initialization",
            ));
        }
        indexes::query(&mut conn.client, namespace, query)
    }
    fn inspect(
        &self,
        namespace: &str,
        query: &aq_playbooks_runner::InspectionQuery,
    ) -> Result<aq_playbooks_runner::InspectionPage, StorageError> {
        check_name(namespace)?;
        query.validate()?;
        let mut conn = self.connection()?;
        let mut tx = conn.client.transaction().map_err(backend)?;
        let Some(row) = tx
            .query_opt(
                "SELECT revision,indexes_materialized FROM aq_playbooks.namespaces WHERE namespace=$1 FOR SHARE",
                &[&namespace],
            )
            .map_err(backend)?
        else {
            return Ok(aq_playbooks_runner::InspectionPage {
                items: vec![],
                next: None,
                revision: 0,
            });
        };
        if !row.get::<_, bool>(1) {
            return Err(backend(
                "namespace indexes require exclusive owner initialization",
            ));
        }
        let revision = row.get::<_, i64>(0) as u64;
        let limit = (query.limit + 1) as i64;
        let rows=tx.query("SELECT item_key,public_value FROM aq_playbooks.inspection_rows WHERE namespace=$1 AND kind=$2 AND ($3::text IS NULL OR run_id=$3) AND ($4::text IS NULL OR item_key>$4) ORDER BY item_key LIMIT $5",&[&namespace,&query.kind.key(),&query.run,&query.after,&limit]).map_err(backend)?;
        let more = rows.len() > query.limit;
        let selected: Vec<_> = rows.into_iter().take(query.limit).collect();
        let next = if more {
            selected.last().map(|r| r.get::<_, String>(0))
        } else {
            None
        };
        let items = selected.into_iter().map(|r| r.get(1)).collect();
        tx.commit().map_err(backend)?;
        Ok(aq_playbooks_runner::InspectionPage {
            items,
            next,
            revision,
        })
    }
    fn acquire(&self, namespace: &str, owner: &str) -> Result<(), StorageError> {
        check_name(namespace)?;
        check_name(owner)?;
        let mut conn = self.connection()?;
        if let Some(lease) = conn.leases.get(namespace) {
            return if lease.owner == owner {
                Ok(())
            } else {
                Err(StorageError::Ownership)
            };
        }
        let key = lock_key(namespace);
        let locked: bool = conn
            .client
            .query_one("SELECT pg_try_advisory_lock($1)", &[&key])
            .map_err(backend)?
            .get(0);
        if !locked {
            return Err(StorageError::Ownership);
        }
        let result = (|| {
            let mut tx = conn.client.transaction().map_err(backend)?;
            let empty = self.encode(namespace, 0, &State::default())?;
            tx.execute("INSERT INTO aq_playbooks.namespaces(namespace,snapshot,protection_key_id,protection_nonce,encrypted_snapshot,indexes_materialized) VALUES($1,$2,$3,$4,$5,true) ON CONFLICT DO NOTHING",&[&namespace,&empty.plaintext,&empty.key_id,&empty.nonce,&empty.ciphertext]).map_err(backend)?;
            let existing=tx.query_one("SELECT revision,snapshot,protection_key_id,protection_nonce,encrypted_snapshot FROM aq_playbooks.namespaces WHERE namespace=$1",&[&namespace]).map_err(backend)?;
            self.decode(
                namespace,
                existing.get::<_, i64>("revision") as u64,
                &existing,
            )?;
            let incarnation: i64 = tx.query_one("UPDATE aq_playbooks.namespaces SET owner=$2, owner_backend_pid=pg_backend_pid(), incarnation=incarnation+1, acquired_at=clock_timestamp() WHERE namespace=$1 RETURNING incarnation", &[&namespace,&owner]).map_err(backend)?.get(0);
            tx.commit().map_err(backend)?;
            Ok(incarnation)
        })();
        match result {
            Ok(incarnation) => {
                conn.leases.insert(
                    namespace.into(),
                    Lease {
                        owner: owner.into(),
                        incarnation,
                    },
                );
                Ok(())
            }
            Err(error) => {
                let _ = conn
                    .client
                    .query_one("SELECT pg_advisory_unlock($1)", &[&key]);
                Err(error)
            }
        }
    }

    fn release(&self, namespace: &str, owner: &str) -> Result<(), StorageError> {
        let mut conn = self.connection()?;
        let lease = conn
            .leases
            .get(namespace)
            .filter(|l| l.owner == owner)
            .ok_or(StorageError::Ownership)?;
        let incarnation = lease.incarnation;
        let changed = conn.client.execute("UPDATE aq_playbooks.namespaces SET owner=NULL,owner_backend_pid=NULL WHERE namespace=$1 AND owner=$2 AND incarnation=$3 AND owner_backend_pid=pg_backend_pid()", &[&namespace,&owner,&incarnation]).map_err(backend)?;
        if changed != 1 {
            return Err(StorageError::Ownership);
        }
        let unlocked: bool = conn
            .client
            .query_one("SELECT pg_advisory_unlock($1)", &[&lock_key(namespace)])
            .map_err(backend)?
            .get(0);
        conn.leases.remove(namespace);
        if !unlocked {
            return Err(StorageError::Ownership);
        }
        Ok(())
    }

    fn load(&self, namespace: &str) -> Result<VersionedState, StorageError> {
        check_name(namespace)?;
        let mut conn = self.connection()?;
        let mut tx = ObservedTransaction::new(
            conn.client.transaction().map_err(backend)?,
            self.profiling.clone(),
            "load",
        );
        // The row lock gives the snapshot and its artifact references one view,
        // without holding an unbounded repeatable-read transaction.
        let Some(row) = tx.query_opt("SELECT revision,snapshot,protection_key_id,protection_nonce,encrypted_snapshot FROM aq_playbooks.namespaces WHERE namespace=$1 FOR SHARE", &[&namespace]).map_err(backend)? else {
            return Ok(VersionedState::default());
        };
        let revision = row.get::<_, i64>("revision") as u64;
        let protected = row.get::<_, Option<String>>("protection_key_id").is_some();
        tx.phase("snapshot_decode");
        let mut state = self.decode(namespace, revision, &row)?;
        tx.phase("artifact_read");
        state.artifacts = self.read_artifacts(&mut tx, namespace, protected)?;
        tx.commit().map_err(backend)?;
        Ok(VersionedState { revision, state })
    }

    fn compare_and_swap(
        &self,
        namespace: &str,
        owner: &str,
        expected_revision: u64,
        state: &State,
    ) -> Result<u64, StorageError> {
        self.commit_state(namespace, owner, expected_revision, state, false)
    }
}
impl PostgresStorage {
    fn commit_state(
        &self,
        namespace: &str,
        owner: &str,
        expected_revision: u64,
        state: &State,
        force_artifact_refresh: bool,
    ) -> Result<u64, StorageError> {
        let expected =
            i64::try_from(expected_revision).map_err(|_| backend("revision exhausted"))?;
        let next = expected
            .checked_add(1)
            .ok_or_else(|| backend("revision exhausted"))?;
        let mut conn = self.connection()?;
        let lease = conn
            .leases
            .get(namespace)
            .filter(|l| l.owner == owner)
            .ok_or(StorageError::Ownership)?;
        let incarnation = lease.incarnation;
        let mut tx = ObservedTransaction::new(
            conn.client.transaction().map_err(backend)?,
            self.profiling.clone(),
            "cas",
        );
        let advisory_key = lock_key(namespace) as u64;
        let high = (advisory_key >> 32) as i64;
        let low = (advisory_key & 0xffff_ffff) as i64;
        let owns_lock: bool = tx.query_one("SELECT EXISTS (SELECT 1 FROM pg_locks WHERE locktype='advisory' AND pid=pg_backend_pid() AND classid::bigint=$1 AND objid::bigint=$2 AND objsubid=1 AND granted)", &[&high,&low]).map_err(backend)?.get(0);
        if !owns_lock {
            return Err(StorageError::Ownership);
        }
        let row = tx.query_opt("SELECT revision,owner,incarnation,owner_backend_pid=pg_backend_pid(),snapshot,protection_key_id,protection_nonce,encrypted_snapshot,indexes_materialized FROM aq_playbooks.namespaces WHERE namespace=$1 FOR UPDATE", &[&namespace]).map_err(backend)?.ok_or(StorageError::Ownership)?;
        if row.get::<_, Option<String>>(1).as_deref() != Some(owner)
            || row.get::<_, i64>(2) != incarnation
            || row.get::<_, Option<bool>>(3) != Some(true)
        {
            return Err(StorageError::Ownership);
        }
        if row.get::<_, i64>(0) != expected {
            return Err(StorageError::Conflict);
        }
        let refresh_artifacts = force_artifact_refresh
            || self.keyring.as_ref().is_some_and(|keys| {
                row.get::<_, Option<String>>("protection_key_id").as_deref()
                    != Some(keys.active_key_id())
            });
        tx.phase("snapshot_decode");
        let mut previous = self.decode(namespace, expected_revision, &row)?;
        tx.phase("artifact_read");
        previous.artifacts = self.read_artifacts(
            &mut tx,
            namespace,
            row.get::<_, Option<String>>("protection_key_id").is_some(),
        )?;
        tx.phase("artifact_locks");
        aq_playbooks_runner::retention::verify_artifact_removals(&previous, state)?;
        let addresses: std::collections::BTreeSet<_> = previous
            .artifacts
            .keys()
            .chain(state.artifacts.keys())
            .cloned()
            .collect();
        let new_addresses: Vec<_> = state
            .artifacts
            .keys()
            .filter(|id| !previous.artifacts.contains_key(*id))
            .cloned()
            .collect();
        if !new_addresses.is_empty() {
            tx.execute("INSERT INTO aq_playbooks.artifact_catalog(digest) SELECT digest FROM unnest($1::text[]) AS pending(digest) ORDER BY digest ON CONFLICT DO NOTHING",&[&new_addresses]).map_err(backend)?;
        }
        if !addresses.is_empty() {
            // All previous references retain their catalog FK. New addresses are
            // created above. Keep union locks, in the same global order as GC.
            let addresses: Vec<_> = addresses.into_iter().collect();
            let locked=tx.query("SELECT digest FROM aq_playbooks.artifact_catalog WHERE digest=ANY($1) ORDER BY digest FOR UPDATE",&[&addresses]).map_err(backend)?;
            if locked.len() != addresses.len() {
                return Err(backend("artifact catalog reference disappeared"));
            }
        }
        tx.phase("retention");
        retention::verify_and_record(&mut tx, namespace, &previous, state)?;
        for (digest, artifact) in &state.artifacts {
            tx.phase("artifact_compare");
            // Previous bytes were freshly read, authenticated and fully verified
            // under this namespace's row/revision/owner locks. Equality checks the
            // entire typed artifact, never just its supplied address. No cache.
            if !refresh_artifacts && previous.artifacts.get(digest) == Some(artifact) {
                tx.skipped();
                continue;
            }
            tx.phase("artifact_validation");
            let started = tx.clock();
            artifact.verify().map_err(backend)?;
            tx.verified(started);
            if &artifact.digest != digest {
                return Err(backend("artifact key does not match digest"));
            }
            let started = tx.clock();
            let bytes = canonical_bytes(artifact).map_err(backend)?;
            tx.canonicalized(started);
            tx.phase("artifact_write");
            if let Some(keyring) = &self.keyring {
                if let Some(existing)=tx.query_opt("SELECT key_id,nonce,ciphertext FROM aq_playbooks.encrypted_artifacts WHERE namespace=$1 AND digest=$2",&[&namespace,&digest]).map_err(backend)?{let key_id:String=existing.get(0);let nonce:Vec<u8>=existing.get(1);let ciphertext:Vec<u8>=existing.get(2);if *keyring.open_artifact(namespace,digest,&key_id,&nonce,&ciphertext)?!=bytes{return Err(backend("immutable protected artifact address conflict"));}}
                let sealed = keyring.seal_artifact(namespace, digest, &bytes)?;
                tx.execute("INSERT INTO aq_playbooks.encrypted_artifacts(namespace,digest,key_id,nonce,ciphertext) VALUES($1,$2,$3,$4,$5) ON CONFLICT(namespace,digest) DO UPDATE SET key_id=EXCLUDED.key_id,nonce=EXCLUDED.nonce,ciphertext=EXCLUDED.ciphertext",&[&namespace,&digest,&sealed.key_id,&sealed.nonce,&sealed.ciphertext]).map_err(backend)?;
            } else {
                tx.execute("INSERT INTO aq_playbooks.artifacts(digest,canonical_bytes) VALUES($1,$2) ON CONFLICT DO NOTHING",&[&digest,&bytes]).map_err(backend)?;
                let stored: Vec<u8> = tx
                    .query_one(
                        "SELECT canonical_bytes FROM aq_playbooks.artifacts WHERE digest=$1",
                        &[&digest],
                    )
                    .map_err(backend)?
                    .get(0);
                if stored != bytes {
                    return Err(backend("immutable artifact address conflict"));
                }
            }
            tx.execute("INSERT INTO aq_playbooks.namespace_artifacts(namespace,digest) VALUES($1,$2) ON CONFLICT DO NOTHING", &[&namespace,&digest]).map_err(backend)?;
        }
        tx.phase("artifact_disposal");
        retention::dispose_artifacts(&mut tx, namespace, &previous, state)?;
        let empty = State::default();
        tx.phase("indexes");
        indexes::sync(
            &mut tx,
            namespace,
            if row.get::<_, bool>("indexes_materialized") {
                &previous
            } else {
                &empty
            },
            state,
        )?;
        tx.phase("snapshot_encode");
        let snapshot = self.encode(namespace, next as u64, state)?;
        tx.phase("snapshot_write");
        tx.execute("UPDATE aq_playbooks.namespaces SET snapshot=$2,protection_key_id=$3,protection_nonce=$4,encrypted_snapshot=$5,revision=$6,indexes_materialized=true,updated_at=clock_timestamp() WHERE namespace=$1",&[&namespace,&snapshot.plaintext,&snapshot.key_id,&snapshot.nonce,&snapshot.ciphertext,&next]).map_err(backend)?;
        if self.keyring.is_some() {
            let addresses: Vec<_> = state.artifacts.keys().cloned().collect();
            tx.execute("DELETE FROM aq_playbooks.artifacts a WHERE digest=ANY($1) AND NOT EXISTS(SELECT 1 FROM aq_playbooks.namespace_artifacts n JOIN aq_playbooks.namespaces s ON s.namespace=n.namespace WHERE n.digest=a.digest AND (s.protection_key_id IS NULL OR NOT EXISTS(SELECT 1 FROM aq_playbooks.encrypted_artifacts e WHERE e.namespace=n.namespace AND e.digest=n.digest)))",&[&addresses]).map_err(backend)?;
        }
        tx.commit().map_err(backend)?;
        Ok(next as u64)
    }
}
