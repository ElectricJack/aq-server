//! Per-test PostgreSQL databases. Needs `DATABASE_URL` (see `scripts/test-db`).

use crate::store::Store;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Connection, PgConnection, PgPool};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use uuid::Uuid;

#[cfg(feature = "test-support")]
pub mod control_diagnostics;

#[cfg(feature = "test-support")]
tokio::task_local! { static STORE_PROFILE: u32; }

/// Build a fixture with a real registered migration prefix. The scope affects
/// construction only; the Store keeps its profile across router/spawn boundaries.
#[cfg(feature = "test-support")]
pub fn with_store_profile<F: std::future::Future>(
    version: u32,
    future: F,
) -> impl std::future::Future<Output = F::Output> {
    STORE_PROFILE.scope(version, Box::pin(future))
}

/// Scope a fixture to a real frozen32 migration prefix. It does not relabel a
/// database and does not alter normal Store startup or inherited spawned tasks.
#[cfg(feature = "test-support")]
pub fn with_state32_store<F: std::future::Future>(
    future: F,
) -> impl std::future::Future<Output = F::Output> {
    // Keep large async fixture state on the heap rather than embedding another
    // complete copy in this wrapper's async state machine.
    with_store_profile(32, future)
}
/// Expected actual fixture format; ordinary fixtures always use current Store.
pub fn fixture_store_state_version() -> u32 {
    #[cfg(feature = "test-support")]
    if let Ok(version) = STORE_PROFILE.try_with(|version| *version) {
        return version;
    }
    crate::STATE_FORMAT_VERSION
}

pub struct TestDb {
    admin_url: String,
    name: String,
    options: PgConnectOptions,
    /// No connection to this database has been handed out yet, so the first
    /// `store()` may replace it with a copy of a migrated template.
    pristine: AtomicBool,
    clone_gate: tokio::sync::Mutex<()>,
}

/// One migration transaction at a time per process, and per PostgreSQL server
/// across processes. A fresh full migration holds ~2.5k locks until commit, and
/// PostgreSQL's shared lock table is only `max_locks_per_transaction` (64) x
/// backends (~8k slots), so a few concurrent fresh test databases overflow it
/// (53200 out of shared memory). nextest runs every test in its own process, so
/// the in-process semaphore alone no longer bounds concurrency: the slot also
/// holds a session advisory lock in the `DATABASE_URL` database, which every
/// test process shares. Release builds never compile this.
static MIGRATION_SLOT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
/// Advisory lock key (in the shared admin database) for `migration_slot`.
const MIGRATION_LOCK: i64 = 0x6171_7465_7374_6d67; // "aqtestmg"
/// Advisory lock key serializing template builds (see `template`).
const TEMPLATE_LOCK: i64 = 0x6171_7465_7374_7470; // "aqtesttp"

pub(crate) struct MigrationSlot {
    _permit: tokio::sync::SemaphorePermit<'static>,
    // Dropping the session releases its advisory lock.
    _lock: Option<PgConnection>,
}

pub(crate) async fn migration_slot() -> MigrationSlot {
    let permit = MIGRATION_SLOT
        .acquire()
        .await
        .expect("test migration semaphore is never closed");
    let lock = match std::env::var("DATABASE_URL") {
        Ok(url) => advisory_lock(&url, MIGRATION_LOCK).await,
        Err(_) => None,
    };
    MigrationSlot {
        _permit: permit,
        _lock: lock,
    }
}

/// A session holding advisory lock `key` in the `url` database; `None` (and
/// in-process serialization only) when that database is unreachable.
async fn advisory_lock(url: &str, key: i64) -> Option<PgConnection> {
    let mut conn = PgConnection::connect(url).await.ok()?;
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(key)
        .execute(&mut conn)
        .await
        .ok()?;
    Some(conn)
}

/// Migrated template databases built by this process, by schema version.
static TEMPLATES: tokio::sync::Mutex<Option<HashMap<u32, String>>> =
    tokio::sync::Mutex::const_new(None);

/// A database migrated to `version`, cloned by `CREATE DATABASE ... TEMPLATE`.
/// A fresh migration takes ~2.5k locks in one transaction and PostgreSQL's
/// shared lock table holds ~8k at default settings, so concurrent fresh test
/// migrations overflow it (53200). Building each version once and copying it
/// avoids that and the per-test migration cost. Names hash the migration SQL,
/// so concurrent and later test processes on the same server reuse them.
async fn template(admin_url: &str, version: u32) -> Result<String, String> {
    let mut templates = TEMPLATES.lock().await;
    let templates = templates.get_or_insert_with(HashMap::new);
    if let Some(name) = templates.get(&version) {
        return Ok(name.clone());
    }
    use sha2::{Digest as _, Sha256};
    let mut digest = Sha256::new();
    for (number, sql) in crate::migration_catalog::MIGRATIONS {
        if *number as u32 > version {
            break;
        }
        digest.update(number.to_be_bytes());
        digest.update(sql.as_bytes());
    }
    let name = format!("aq_tpl_{}_{version}", &hex::encode(digest.finalize())[..16]);
    let fail = |step: &str, e: &dyn std::fmt::Display| format!("template {name}: {step}: {e}");
    let mut admin = PgConnection::connect(admin_url)
        .await
        .map_err(|e| fail("connect", &e))?;
    let lookup = "SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname=$1)";
    let mut exists: bool = sqlx::query_scalar(lookup)
        .bind(&name)
        .fetch_one(&mut admin)
        .await
        .map_err(|e| fail("lookup", &e))?;
    // Test processes starting together would otherwise each build the same
    // template: one builds it while the others wait, then reuse it.
    let mut _building = None;
    if !exists {
        _building = advisory_lock(admin_url, TEMPLATE_LOCK).await;
        exists = sqlx::query_scalar(lookup)
            .bind(&name)
            .fetch_one(&mut admin)
            .await
            .map_err(|e| fail("lookup", &e))?;
    }
    if !exists {
        // Build under a private name; only a completely migrated database is
        // ever renamed to the shared template name.
        let building = format!("aq_tplb_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE DATABASE {building}"))
            .execute(&mut admin)
            .await
            .map_err(|e| fail("create", &e))?;
        let options = PgConnectOptions::from_str(admin_url)
            .map_err(|e| fail("url", &e))?
            .database(&building);
        let built = async {
            let pool = PgPoolOptions::new()
                .max_connections(1)
                .connect_with(options)
                .await
                .map_err(|e| fail("connect build", &e))?;
            let migrated = crate::store::migrate_fixture_profile(&pool, version).await;
            pool.close().await;
            migrated.map_err(|e| fail("migrate", &e))?;
            sqlx::query(&format!(
                "ALTER DATABASE {building} WITH ALLOW_CONNECTIONS false"
            ))
            .execute(&mut admin)
            .await
            .map_err(|e| fail("seal", &e))?;
            sqlx::query(&format!("ALTER DATABASE {building} RENAME TO {name}"))
                .execute(&mut admin)
                .await
                .map_err(|e| fail("publish", &e))
        }
        .await;
        if let Err(error) = built {
            // Another process may have published the same template first.
            let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS {building} WITH (FORCE)"))
                .execute(&mut admin)
                .await;
            let exists: bool =
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname=$1)")
                    .bind(&name)
                    .fetch_one(&mut admin)
                    .await
                    .map_err(|e| fail("lookup", &e))?;
            if !exists {
                return Err(error);
            }
        }
    }
    let _ = admin.close().await;
    templates.insert(version, name.clone());
    Ok(name)
}

impl TestDb {
    pub async fn create() -> TestDb {
        let admin_url =
            std::env::var("DATABASE_URL").expect("set DATABASE_URL via scripts/test-db");
        let name = format!("aq_test_{}", Uuid::new_v4().simple());
        let mut conn = PgConnection::connect(&admin_url)
            .await
            .expect("connect to DATABASE_URL");
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&mut conn)
            .await
            .expect("create test database");
        let _ = conn.close().await;
        let options = PgConnectOptions::from_str(&admin_url)
            .expect("DATABASE_URL is a valid PostgreSQL URL")
            .database(&name);
        TestDb {
            admin_url,
            name,
            options,
            pristine: AtomicBool::new(true),
            clone_gate: tokio::sync::Mutex::new(()),
        }
    }

    /// Connect options (host, credentials, query parameters of `DATABASE_URL`)
    /// pointing at this test's database.
    pub fn options(&self) -> PgConnectOptions {
        self.pristine.store(false, Ordering::SeqCst);
        self.options.clone()
    }

    /// Before anything has connected, swap this empty database for a copy of
    /// the migrated `version` template. Store startup then only verifies it.
    /// Any failure leaves an empty database, which startup migrates in place.
    async fn clone_template(&self, version: u32) {
        let _gate = self.clone_gate.lock().await;
        if !self.pristine.swap(false, Ordering::SeqCst) {
            return;
        }
        let template = match template(&self.admin_url, version).await {
            Ok(template) => template,
            Err(error) => {
                eprintln!("TestDb: migrating in place: {error}");
                return;
            }
        };
        let mut conn = PgConnection::connect(&self.admin_url)
            .await
            .expect("connect to DATABASE_URL");
        let name = &self.name;
        sqlx::query(&format!("DROP DATABASE {name}"))
            .execute(&mut conn)
            .await
            .expect("drop unused test database");
        if let Err(error) = sqlx::query(&format!("CREATE DATABASE {name} TEMPLATE {template}"))
            .execute(&mut conn)
            .await
        {
            eprintln!("TestDb: migrating in place: clone {template}: {error}");
            sqlx::query(&format!("CREATE DATABASE {name}"))
                .execute(&mut conn)
                .await
                .expect("create test database");
        }
        let _ = conn.close().await;
    }

    /// Connects (and migrates) a fresh `Store` to this database.
    pub async fn store(&self) -> Store {
        #[cfg(feature = "test-support")]
        if let Ok(version) = STORE_PROFILE.try_with(|version| *version) {
            self.clone_template(version).await;
            return Store::connect_profile_for_testing(self.options.clone(), version)
                .await
                .expect("connect actual historical store");
        }
        self.clone_template(crate::STATE_FORMAT_VERSION).await;
        Store::connect_with(self.options.clone())
            .await
            .expect("connect store")
    }

    /// A Store using an actual registered historical migration prefix, without
    /// the implicit upgrade performed by `store()`.
    #[cfg(test)]
    pub async fn store_at_version(&self, supported: u32) -> Store {
        self.clone_template(supported).await;
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect_with(self.options.clone())
            .await
            .expect("connect historical fixture pool");
        Store::from_fixture_pool(pool, supported)
            .await
            .expect("connect historical fixture store")
    }

    /// A raw pool for tests that poke at tables directly.
    pub async fn pool(&self) -> PgPool {
        PgPoolOptions::new()
            .max_connections(2)
            .connect_with(self.options())
            .await
            .expect("connect pool")
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        let admin_url = self.admin_url.clone();
        let name = self.name.clone();
        let _ = std::thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            rt.block_on(async {
                if let Ok(mut conn) = PgConnection::connect(&admin_url).await {
                    let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
                        .execute(&mut conn)
                        .await;
                }
            });
        })
        .join();
    }
}

/// Execute an actual frozen SQL prefix through the production migration/guard
/// implementation. This fixture does not activate a runtime or bypass checks.
pub async fn migrate_profile(
    pool: &PgPool,
    supported: u32,
) -> Result<(), crate::store::StoreError> {
    crate::store::migrate_fixture_profile(pool, supported).await
}

/// Shared request admission instance for reserved-lane saturation fixtures.
/// Available only with the existing test-support module.
pub fn admission(store: &Store) -> std::sync::Arc<crate::admission::Admission> {
    store.admission.clone()
}

/// Whether the node's compute is busy (`compute::busy_locked`, as a native poll sees it).
pub async fn node_busy(
    state: &crate::AppState,
    node: &crate::worker_api::AuthenticatedNode,
) -> Result<bool, crate::error::AppError> {
    let internal = |_| crate::error::AppError::Internal;
    let mut tx = state.store.pool.begin().await.map_err(internal)?;
    crate::worker_api::lock_current_node(&mut tx, node).await?;
    crate::profiled_execution::compute::busy_locked(&mut tx, node.id()).await
}
