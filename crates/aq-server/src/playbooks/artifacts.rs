//! D9 owner artifact API: install verified immutable bytes, validate (structure
//! + exact pinned-catalog compatibility), activate and roll back a binding.
//!
//! Every mutation carries a caller operation id. The ledger records each
//! operation once; a replay with the same id and the same request returns the
//! recorded result, a replay with a different request is a conflict. The
//! engine pins each run's artifact at admission, so activation and rollback
//! change only which artifact new runs admit.
use super::domain::{Error, Result};
use aq_playbook_host::model::{Artifact, Registry, canonical_bytes};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{future::Future, sync::Mutex};
use uuid::Uuid;

const MAX_ID: usize = 128;
pub const MAX_ARTIFACT_BYTES: usize = 1_048_576;

/// The engine side of the API.
pub trait Catalog: Send + Sync {
    fn registry(&self) -> Registry;
    fn install(&self, artifact: Artifact) -> impl Future<Output = Result<()>> + Send;
    fn activate(&self, binding: &str, digest: &str) -> impl Future<Output = Result<()>> + Send;
    /// The binding's active artifact, `None` if the binding was never activated.
    fn current(&self, binding: &str) -> impl Future<Output = Result<Option<String>>> + Send;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Install,
    Activate,
    Rollback,
}
impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Install => "install",
            Kind::Activate => "activate",
            Kind::Rollback => "rollback",
        }
    }
    fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "install" => Kind::Install,
            "activate" => Kind::Activate,
            "rollback" => Kind::Rollback,
            _ => return Err(Error::Unknown),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub op_id: String,
    pub kind: Kind,
    pub binding: String,
    pub artifact: String,
    /// Activate/rollback: the artifact that was active before this operation.
    pub previous: Option<String>,
}
impl Record {
    fn same_request(&self, other: &Record) -> bool {
        self.kind == other.kind
            && self.binding == other.binding
            && (self.kind == Kind::Rollback || self.artifact == other.artifact)
    }
    fn view(&self, duplicate: bool) -> Value {
        json!({"op_id": self.op_id, "kind": self.kind, "binding": self.binding,
               "artifact": self.artifact, "previous": self.previous,
               "duplicate": duplicate})
    }
}

/// Operation ledger. `insert` is first-writer-wins per op id and returns the
/// stored record; `history` is one binding's activate/rollback records in
/// commit order.
pub trait Ledger: Send + Sync {
    fn get(&self, op_id: &str) -> impl Future<Output = Result<Option<Record>>> + Send;
    fn insert(&self, record: Record) -> impl Future<Output = Result<Record>> + Send;
    fn history(&self, binding: &str) -> impl Future<Output = Result<Vec<Record>>> + Send;
}

/// A ledger whose bindings can be locked. Activate and rollback run their
/// replay check, stack read, engine switch and ledger write through the held
/// lock, so concurrent operations on one binding each see the stack the
/// previous one committed.
pub trait LockingLedger: Ledger {
    type Held<'a>: Held
    where
        Self: 'a;
    fn lock(&self, binding: &str) -> impl Future<Output = Result<Self::Held<'_>>> + Send;
}
/// The ledger seen while holding one binding's lock.
pub trait Held: Ledger {
    /// Make this hold's writes durable and release the lock. Dropping the
    /// hold instead (cancellation, error) discards its writes and releases.
    fn release(self) -> impl Future<Output = Result<()>> + Send;
}

#[derive(Default)]
pub struct MemoryLedger(Mutex<Vec<Record>>, tokio::sync::Mutex<()>);
impl Ledger for MemoryLedger {
    async fn get(&self, op_id: &str) -> Result<Option<Record>> {
        Ok(self
            .0
            .lock()
            .map_err(|_| Error::Unknown)?
            .iter()
            .find(|r| r.op_id == op_id)
            .cloned())
    }
    async fn insert(&self, record: Record) -> Result<Record> {
        let mut records = self.0.lock().map_err(|_| Error::Unknown)?;
        if let Some(old) = records.iter().find(|r| r.op_id == record.op_id) {
            return Ok(old.clone());
        }
        records.push(record.clone());
        Ok(record)
    }
    async fn history(&self, binding: &str) -> Result<Vec<Record>> {
        Ok(self
            .0
            .lock()
            .map_err(|_| Error::Unknown)?
            .iter()
            .filter(|r| r.binding == binding && r.kind != Kind::Install)
            .cloned()
            .collect())
    }
}
/// One lock for every binding: coarser than needed, still correct. Writes
/// through the hold are immediate.
pub struct MemoryHeld<'a> {
    ledger: &'a MemoryLedger,
    _guard: tokio::sync::MutexGuard<'a, ()>,
}
impl LockingLedger for MemoryLedger {
    type Held<'a> = MemoryHeld<'a>;
    async fn lock(&self, _binding: &str) -> Result<MemoryHeld<'_>> {
        Ok(MemoryHeld {
            ledger: self,
            _guard: self.1.lock().await,
        })
    }
}
impl Ledger for MemoryHeld<'_> {
    async fn get(&self, op_id: &str) -> Result<Option<Record>> {
        self.ledger.get(op_id).await
    }
    async fn insert(&self, record: Record) -> Result<Record> {
        self.ledger.insert(record).await
    }
    async fn history(&self, binding: &str) -> Result<Vec<Record>> {
        self.ledger.history(binding).await
    }
}
impl Held for MemoryHeld<'_> {
    async fn release(self) -> Result<()> {
        Ok(())
    }
}

/// Durable ledger, one per playbook owner namespace.
pub struct PgLedger {
    pub pool: sqlx::PgPool,
    pub namespace_id: Uuid,
}
fn row(r: sqlx::postgres::PgRow) -> Result<Record> {
    use sqlx::Row;
    Ok(Record {
        op_id: r.try_get("op_id")?,
        kind: Kind::parse(r.try_get::<&str, _>("kind")?)?,
        binding: r.try_get("binding")?,
        artifact: r.try_get("artifact")?,
        previous: r.try_get("previous")?,
    })
}
const COLUMNS: &str = "op_id,kind,binding,artifact,previous";
async fn pg_get(c: &mut sqlx::PgConnection, ns: Uuid, op_id: &str) -> Result<Option<Record>> {
    sqlx::query(&format!(
        "SELECT {COLUMNS} FROM playbook_activations WHERE namespace_id=$1 AND op_id=$2"
    ))
    .bind(ns)
    .bind(op_id)
    .fetch_optional(&mut *c)
    .await?
    .map(row)
    .transpose()
}
async fn pg_insert(c: &mut sqlx::PgConnection, ns: Uuid, r: Record) -> Result<Record> {
    sqlx::query(
        "INSERT INTO playbook_activations(namespace_id,op_id,kind,binding,artifact,previous,created_at_ms)
         VALUES($1,$2,$3,$4,$5,$6,(extract(epoch from clock_timestamp())*1000)::BIGINT)
         ON CONFLICT(namespace_id,op_id) DO NOTHING",
    )
    .bind(ns)
    .bind(&r.op_id)
    .bind(r.kind.as_str())
    .bind(&r.binding)
    .bind(&r.artifact)
    .bind(&r.previous)
    .execute(&mut *c)
    .await?;
    pg_get(c, ns, &r.op_id).await?.ok_or(Error::Unknown)
}
async fn pg_history(c: &mut sqlx::PgConnection, ns: Uuid, binding: &str) -> Result<Vec<Record>> {
    sqlx::query(&format!(
        "SELECT {COLUMNS} FROM playbook_activations
         WHERE namespace_id=$1 AND binding=$2 AND kind<>'install' ORDER BY seq"
    ))
    .bind(ns)
    .bind(binding)
    .fetch_all(&mut *c)
    .await?
    .into_iter()
    .map(row)
    .collect()
}
impl Ledger for PgLedger {
    async fn get(&self, op_id: &str) -> Result<Option<Record>> {
        pg_get(&mut *self.pool.acquire().await?, self.namespace_id, op_id).await
    }
    async fn insert(&self, r: Record) -> Result<Record> {
        pg_insert(&mut *self.pool.acquire().await?, self.namespace_id, r).await
    }
    async fn history(&self, binding: &str) -> Result<Vec<Record>> {
        pg_history(&mut *self.pool.acquire().await?, self.namespace_id, binding).await
    }
}
/// One transaction holding a transaction-scoped advisory lock per
/// (namespace, binding). Every ledger read and write of the hold runs on that
/// transaction, so a waiter holds one connection and the holder needs no
/// other. Dropping the hold rolls back (the pool flushes it on release),
/// which also releases the lock.
pub struct PgHeld {
    tx: tokio::sync::Mutex<sqlx::Transaction<'static, sqlx::Postgres>>,
    namespace_id: Uuid,
}
impl LockingLedger for PgLedger {
    type Held<'a> = PgHeld;
    async fn lock(&self, binding: &str) -> Result<PgHeld> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
            .bind(format!(
                "aq.playbook.activation/{}/{binding}",
                self.namespace_id
            ))
            .execute(&mut *tx)
            .await?;
        Ok(PgHeld {
            tx: tokio::sync::Mutex::new(tx),
            namespace_id: self.namespace_id,
        })
    }
}
impl Ledger for PgHeld {
    async fn get(&self, op_id: &str) -> Result<Option<Record>> {
        pg_get(&mut **self.tx.lock().await, self.namespace_id, op_id).await
    }
    async fn insert(&self, r: Record) -> Result<Record> {
        pg_insert(&mut **self.tx.lock().await, self.namespace_id, r).await
    }
    async fn history(&self, binding: &str) -> Result<Vec<Record>> {
        pg_history(&mut **self.tx.lock().await, self.namespace_id, binding).await
    }
}
impl Held for PgHeld {
    async fn release(self) -> Result<()> {
        Ok(self.tx.into_inner().commit().await?)
    }
}

fn valid_id(s: &str) -> Result<()> {
    if s.is_empty() || s.len() > MAX_ID || s.chars().any(char::is_control) {
        return Err(Error::Invalid);
    }
    Ok(())
}

/// Decode verified immutable bytes and check every contract is exactly the
/// owner's pinned catalog contract.
pub fn validate(registry: &Registry, bytes: &[u8]) -> Result<Artifact> {
    if bytes.len() > MAX_ARTIFACT_BYTES {
        return Err(Error::Invalid);
    }
    let artifact = Artifact::from_canonical_bytes(bytes).map_err(|_| Error::Invalid)?;
    let r = &artifact.content.registry;
    let commands = r
        .commands
        .iter()
        .all(|(n, c)| registry.commands.get(n) == Some(c));
    let events = r
        .events
        .iter()
        .all(|(n, e)| registry.events.get(n) == Some(e));
    if !commands || !events {
        return Err(Error::Invalid);
    }
    valid_id(&artifact.content.playbook.id)?;
    Ok(artifact)
}
/// Canonical bytes of an artifact given as JSON (the HTTP body form).
pub fn bytes_of(artifact: &Value) -> Result<Vec<u8>> {
    canonical_bytes(artifact).map_err(|_| Error::Invalid)
}
pub fn summary(artifact: &Artifact) -> Value {
    let r = &artifact.content.registry;
    json!({"digest": artifact.digest, "binding": artifact.content.playbook.id,
           "commands": r.commands.keys().collect::<Vec<_>>(),
           "events": r.events.keys().collect::<Vec<_>>(),
           "dependencies": artifact.content.dependencies.keys().collect::<Vec<_>>()})
}

async fn replay<L: Ledger>(ledger: &L, wanted: &Record) -> Result<Option<Value>> {
    match ledger.get(&wanted.op_id).await? {
        Some(old) if old.same_request(wanted) => Ok(Some(old.view(true))),
        Some(_) => Err(Error::Conflict),
        None => Ok(None),
    }
}
async fn commit<L: Ledger>(ledger: &L, record: Record) -> Result<Value> {
    let stored = ledger.insert(record.clone()).await?;
    if !stored.same_request(&record) {
        return Err(Error::Conflict);
    }
    Ok(stored.view(stored != record))
}

pub async fn install<C: Catalog, L: Ledger>(
    catalog: &C,
    ledger: &L,
    op_id: &str,
    bytes: &[u8],
) -> Result<Value> {
    valid_id(op_id)?;
    let artifact = validate(&catalog.registry(), bytes)?;
    let record = Record {
        op_id: op_id.into(),
        kind: Kind::Install,
        binding: artifact.content.playbook.id.clone(),
        artifact: artifact.digest.clone(),
        previous: None,
    };
    if let Some(v) = replay(ledger, &record).await? {
        return Ok(v);
    }
    // Content-addressed: re-installing identical bytes is a no-op.
    catalog.install(artifact).await?;
    commit(ledger, record).await
}

/// Activation stack from the ledger: activate pushes, rollback pops. The
/// bottom is the artifact active before the first ledgered activation.
fn stack(history: &[Record]) -> Vec<String> {
    let mut s: Vec<String> = history
        .first()
        .and_then(|r| r.previous.clone())
        .into_iter()
        .collect();
    for r in history {
        match r.kind {
            Kind::Activate => s.push(r.artifact.clone()),
            Kind::Rollback => {
                s.pop();
            }
            Kind::Install => {}
        }
    }
    s
}

pub async fn activate<C: Catalog, L: LockingLedger>(
    catalog: &C,
    ledger: &L,
    op_id: &str,
    binding: &str,
    digest: &str,
) -> Result<Value> {
    valid_id(op_id)?;
    valid_id(binding)?;
    let wanted = Record {
        op_id: op_id.into(),
        kind: Kind::Activate,
        binding: binding.into(),
        artifact: digest.into(),
        previous: None,
    };
    // Replay, stack read, engine switch and ledger write run under the
    // binding's lock, so a concurrent activate/rollback (or a concurrent
    // retry of this op id) cannot read a stale stack.
    let held = ledger.lock(binding).await?;
    if let Some(v) = replay(&held, &wanted).await? {
        held.release().await?;
        return Ok(v);
    }
    // Previous comes from the ledger first: a crash after the engine commit
    // and before the ledger write replays to the same recorded previous.
    let previous = match stack(&held.history(binding).await?).pop() {
        Some(top) => Some(top),
        None => catalog.current(binding).await?,
    };
    catalog.activate(binding, digest).await?;
    let out = commit(&held, Record { previous, ..wanted }).await?;
    held.release().await?;
    Ok(out)
}

pub async fn rollback<C: Catalog, L: LockingLedger>(
    catalog: &C,
    ledger: &L,
    op_id: &str,
    binding: &str,
) -> Result<Value> {
    valid_id(op_id)?;
    valid_id(binding)?;
    let wanted = Record {
        op_id: op_id.into(),
        kind: Kind::Rollback,
        binding: binding.into(),
        artifact: String::new(),
        previous: None,
    };
    // Under the binding's lock: two concurrent rollbacks pop two entries,
    // and a concurrent retry of this op id replays instead of popping again.
    let held = ledger.lock(binding).await?;
    if let Some(v) = replay(&held, &wanted).await? {
        held.release().await?;
        return Ok(v);
    }
    let s = stack(&held.history(binding).await?);
    let [.., target, current] = s.as_slice() else {
        // Nothing recorded to roll back to: refuse explicitly.
        return Err(Error::Conflict);
    };
    catalog.activate(binding, target).await?;
    let record = Record {
        artifact: target.clone(),
        previous: Some(current.clone()),
        ..wanted
    };
    let out = commit(&held, record).await?;
    held.release().await?;
    Ok(out)
}

/// Make the D9 coding policy the binding default: install its four artifacts
/// and activate `aq.coding`, each under a derived operation id.
pub async fn install_coding_default<C: Catalog, L: LockingLedger>(
    catalog: &C,
    ledger: &L,
    op_id: &str,
    policy: &super::coding::CodingPolicy,
) -> Result<Value> {
    if op_id.len() > MAX_ID - 16 {
        return Err(Error::Invalid);
    }
    let artifacts = policy.build().map_err(|_| Error::Invalid)?;
    for (i, a) in artifacts.install_order().into_iter().enumerate() {
        let bytes = canonical_bytes(a).map_err(|_| Error::Invalid)?;
        install(catalog, ledger, &format!("{op_id}/install/{i}"), &bytes).await?;
    }
    let digest = &artifacts.root.digest;
    let activation = activate(
        catalog,
        ledger,
        &format!("{op_id}/activate"),
        super::coding::ROOT,
        digest,
    )
    .await?;
    Ok(json!({"binding": super::coding::ROOT, "artifact": digest,
              "activation": activation, "input_keys": policy.max_repairs + 1}))
}

/// Production catalog: the owner driver, activating under an active grant.
pub struct DriverCatalog<'a> {
    pub driver: &'a super::driver::Driver,
    pub grant: Uuid,
    pub revision: i64,
}
impl Catalog for DriverCatalog<'_> {
    fn registry(&self) -> Registry {
        self.driver.service.profile.registry()
    }
    async fn install(&self, artifact: Artifact) -> Result<()> {
        self.driver.install(artifact).await
    }
    async fn activate(&self, binding: &str, digest: &str) -> Result<()> {
        self.driver
            .activate(binding.into(), digest.into(), self.grant, self.revision)
            .await
            .map(|_| ())
    }
    async fn current(&self, binding: &str) -> Result<Option<String>> {
        self.driver.binding_artifact(binding.into()).await
    }
}

#[cfg(test)]
mod tests;
