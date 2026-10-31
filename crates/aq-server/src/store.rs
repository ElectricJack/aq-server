//! PostgreSQL foundation store. All timestamps are supplied by the caller.

use crate::STATE_FORMAT_VERSION;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgRow;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{FromRow, PgPool, Row};
use std::time::Duration;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(
        "state format version {found} is newer than this server supports ({supported}); upgrade the server"
    )]
    IncompatibleState { found: u32, supported: u32 },
    #[error("a user with that email already exists")]
    EmailTaken,
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("corrupt data: {0}")]
    Corrupt(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Owner,
    Viewer,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Owner => "owner",
            Role::Viewer => "viewer",
        }
    }
}

impl std::str::FromStr for Role {
    type Err = StoreError;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "owner" => Ok(Role::Owner),
            "viewer" => Ok(Role::Viewer),
            other => Err(StoreError::Corrupt(format!("unknown role {other:?}"))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub id: Uuid,
    pub email: String,
    pub role: Role,
    pub issuer: Option<String>,
    pub subject: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NewNode {
    pub name: String,
    pub credential_hash: String,
    pub worker_version: String,
    pub protocol_version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub id: Uuid,
    pub name: String,
    pub worker_version: String,
    pub protocol_version: u32,
    pub enrolled_at: DateTime<Utc>,
    pub last_seen_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
}

struct UserRow {
    id: Uuid,
    email: String,
    role: String,
    issuer: Option<String>,
    subject: Option<String>,
}

impl<'r> FromRow<'r, PgRow> for UserRow {
    fn from_row(row: &'r PgRow) -> sqlx::Result<Self> {
        Ok(UserRow {
            id: row.try_get("id")?,
            email: row.try_get("email")?,
            role: row.try_get("role")?,
            issuer: row.try_get("issuer")?,
            subject: row.try_get("subject")?,
        })
    }
}

impl TryFrom<UserRow> for User {
    type Error = StoreError;
    fn try_from(r: UserRow) -> Result<Self> {
        Ok(User {
            id: r.id,
            email: r.email,
            role: r.role.parse()?,
            issuer: r.issuer,
            subject: r.subject,
        })
    }
}

struct NodeRow {
    id: Uuid,
    name: String,
    worker_version: String,
    protocol_version: i32,
    enrolled_at: DateTime<Utc>,
    last_seen_at: Option<DateTime<Utc>>,
    revoked_at: Option<DateTime<Utc>>,
}

impl<'r> FromRow<'r, PgRow> for NodeRow {
    fn from_row(row: &'r PgRow) -> sqlx::Result<Self> {
        Ok(NodeRow {
            id: row.try_get("id")?,
            name: row.try_get("name")?,
            worker_version: row.try_get("worker_version")?,
            protocol_version: row.try_get("protocol_version")?,
            enrolled_at: row.try_get("enrolled_at")?,
            last_seen_at: row.try_get("last_seen_at")?,
            revoked_at: row.try_get("revoked_at")?,
        })
    }
}

impl From<NodeRow> for Node {
    fn from(r: NodeRow) -> Self {
        Node {
            id: r.id,
            name: r.name,
            worker_version: r.worker_version,
            protocol_version: r.protocol_version.max(0) as u32,
            enrolled_at: r.enrolled_at,
            last_seen_at: r.last_seen_at,
            revoked_at: r.revoked_at,
        }
    }
}

const USER_COLS: &str = "id, email, role, issuer, subject";
const NODE_COLS: &str =
    "id, name, worker_version, protocol_version, enrolled_at, last_seen_at, revoked_at";

fn map_user_insert_err(e: sqlx::Error) -> StoreError {
    if let sqlx::Error::Database(db) = &e
        && db.is_unique_violation()
        && db.constraint() == Some("users_email_lower")
    {
        return StoreError::EmailTaken;
    }
    StoreError::Database(e)
}

const MIGRATIONS: &[(i32, &str)] = &[
    (1, include_str!("../migrations/0001_foundation.sql")),
    (2, include_str!("../migrations/0002_execution.sql")),
    (3, include_str!("../migrations/0003_agent_access.sql")),
    (4, include_str!("../migrations/0004_source_retention.sql")),
    (5, include_str!("../migrations/0005_messaging.sql")),
    (6, include_str!("../migrations/0006_profile_catalog.sql")),
    (7, include_str!("../migrations/0007_graph_mutations.sql")),
    (8, include_str!("../migrations/0008_project_bindings.sql")),
    (
        9,
        include_str!("../migrations/0009_intelligence_selection.sql"),
    ),
    (
        10,
        include_str!("../migrations/0010_profiled_authority.sql"),
    ),
    (11, include_str!("../migrations/0011_notifications.sql")),
    (12, include_str!("../migrations/0012_terminal_relay.sql")),
    (
        13,
        include_str!("../migrations/0013_profiled_hot_authority.sql"),
    ),
    (
        14,
        include_str!("../migrations/0014_observation_ledger.sql"),
    ),
    (
        15,
        include_str!("../migrations/0015_never_granted_closure.sql"),
    ),
    (
        16,
        include_str!("../migrations/0016_observation_transport.sql"),
    ),
    (
        17,
        include_str!("../migrations/0017_policy_decision_principals.sql"),
    ),
    (18, include_str!("../migrations/0018_profiled_started.sql")),
    (
        19,
        include_str!("../migrations/0019_native_registration.sql"),
    ),
    (
        20,
        include_str!("../migrations/0020_inline_artifact_owner.sql"),
    ),
    (21, include_str!("../migrations/0021_profiled_stopped.sql")),
];

/// Applies missing migrations in one transaction under an advisory lock.
/// Refuses, changing nothing, when the database is from a newer server.
async fn migrate(pool: &PgPool) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(x'6167656e7471'::bigint)")
        .execute(&mut *tx)
        .await?;

    let has_meta: bool = sqlx::query_scalar("SELECT to_regclass('meta') IS NOT NULL")
        .fetch_one(&mut *tx)
        .await?;
    if has_meta {
        let value: Option<String> =
            sqlx::query_scalar("SELECT value FROM meta WHERE key = 'state_format_version'")
                .fetch_optional(&mut *tx)
                .await?;
        if let Some(value) = value {
            let found: u32 = value
                .parse()
                .map_err(|_| StoreError::Corrupt(format!("state_format_version {value:?}")))?;
            if found > STATE_FORMAT_VERSION {
                return Err(StoreError::IncompatibleState {
                    found,
                    supported: STATE_FORMAT_VERSION,
                });
            }
        }
    }

    sqlx::raw_sql(
        "CREATE TABLE IF NOT EXISTS schema_migrations (\
         version INTEGER PRIMARY KEY, \
         applied_at TIMESTAMPTZ NOT NULL DEFAULT now())",
    )
    .execute(&mut *tx)
    .await?;

    let applied: Vec<i32> = sqlx::query_scalar("SELECT version FROM schema_migrations")
        .fetch_all(&mut *tx)
        .await?;
    let newest_known = MIGRATIONS.iter().map(|m| m.0).max().unwrap_or(0);
    if let Some(&found) = applied.iter().filter(|v| **v > newest_known).max() {
        return Err(StoreError::IncompatibleState {
            found: found as u32,
            supported: STATE_FORMAT_VERSION,
        });
    }

    for (version, sql) in MIGRATIONS {
        if applied.contains(version) {
            continue;
        }
        sqlx::raw_sql(sql).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO schema_migrations (version) VALUES ($1)")
            .bind(version)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

#[derive(Clone)]
pub struct Store {
    pub(crate) pool: PgPool,
    control_pool: PgPool,
    pub(crate) notification_pool: PgPool,
    pub(crate) observation_pool: PgPool,
    terminal_pool: PgPool,
    terminal_data_pool: PgPool,
    pub(crate) admission: std::sync::Arc<crate::admission::Admission>,
    pub(crate) verified_profiled_facts:
        std::sync::Arc<std::sync::Mutex<crate::execution::authority::cache::VerifiedCache>>,
    #[cfg(feature = "recovery-test-support")]
    pub(crate) recovery_test: std::sync::Arc<crate::execution::testing::Controls>,
}

impl Store {
    /// Terminal authentication and data use independent connections; never the
    /// execution renewal/cancellation pool. Held polls release SQL before wait.
    pub(crate) fn terminal_read_view(&self) -> Self {
        Self {
            pool: self.terminal_data_pool.clone(),
            ..self.clone()
        }
    }
    #[cfg(feature = "test-support")]
    pub fn terminal_control_pool_for_testing(&self) -> PgPool {
        self.terminal_pool.clone()
    }
    #[cfg(feature = "test-support")]
    pub fn terminal_data_pool_for_testing(&self) -> PgPool {
        self.terminal_data_pool.clone()
    }
    pub(crate) fn terminal_view(&self) -> Self {
        Self {
            pool: self.terminal_pool.clone(),
            ..self.clone()
        }
    }

    pub(crate) fn observation_view(&self) -> Self {
        Self {
            pool: self.observation_pool.clone(),
            ..self.clone()
        }
    }
    /// A view over the same database with reserved control connections. Private
    /// route wiring selects this; credentials cannot choose their own class.
    pub(crate) fn notification_view(&self) -> Self {
        Self {
            pool: self.notification_pool.clone(),
            ..self.clone()
        }
    }

    pub(crate) fn control_view(&self) -> Self {
        Self {
            pool: self.control_pool.clone(),
            ..self.clone()
        }
    }

    #[cfg(feature = "test-support")]
    pub fn profiled_cache_stats(&self) -> Option<(usize, usize, u64, u64)> {
        self.verified_profiled_facts
            .lock()
            .ok()
            .map(|cache| cache.stats())
    }

    /// `SELECT 1`: true when the database answers within `limit`.
    pub async fn ping(&self, limit: std::time::Duration) -> bool {
        matches!(
            tokio::time::timeout(limit, sqlx::query("SELECT 1").execute(&self.pool)).await,
            Ok(Ok(_))
        )
    }

    /// Closes the pool; later queries fail. Used by tests to simulate an outage.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn close(&self) {
        self.pool.close().await;
        self.control_pool.close().await;
        self.notification_pool.close().await;
        self.observation_pool.close().await;
        self.terminal_pool.close().await;
        self.terminal_data_pool.close().await;
    }

    pub async fn connect(database_url: &str) -> Result<Store> {
        Self::connect_with(database_url.parse::<PgConnectOptions>()?).await
    }

    pub async fn connect_with(options: PgConnectOptions) -> Result<Store> {
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .acquire_timeout(Duration::from_secs(5))
            .connect_with(options)
            .await?;
        Self::from_pool(pool).await
    }

    pub async fn from_pool(pool: PgPool) -> Result<Store> {
        migrate(&pool).await?;
        let control_pool = PgPoolOptions::new()
            .max_connections(4)
            .acquire_timeout(Duration::from_secs(2))
            .connect_lazy_with(pool.connect_options().as_ref().clone());
        let notification_pool = PgPoolOptions::new()
            .max_connections(4)
            .acquire_timeout(Duration::from_secs(2))
            .after_connect(|connection, _| {
                Box::pin(async move {
                    sqlx::query("SET statement_timeout='3s'")
                        .execute(&mut *connection)
                        .await?;
                    sqlx::query("SET lock_timeout='2s'")
                        .execute(&mut *connection)
                        .await?;
                    Ok(())
                })
            })
            .connect_lazy_with(pool.connect_options().as_ref().clone());
        let observation_pool = PgPoolOptions::new()
            .max_connections(4)
            .acquire_timeout(Duration::from_secs(2))
            .after_connect(|connection, _| {
                Box::pin(async move {
                    sqlx::query("SET statement_timeout='3s'")
                        .execute(&mut *connection)
                        .await?;
                    sqlx::query("SET lock_timeout='2s'")
                        .execute(&mut *connection)
                        .await?;
                    Ok(())
                })
            })
            .connect_lazy_with(pool.connect_options().as_ref().clone());
        let terminal_pool = PgPoolOptions::new()
            .max_connections(4)
            .acquire_timeout(Duration::from_secs(1))
            .after_connect(|connection, _| {
                Box::pin(async move {
                    sqlx::query("SET statement_timeout='750ms'")
                        .execute(&mut *connection)
                        .await?;
                    sqlx::query("SET lock_timeout='100ms'")
                        .execute(&mut *connection)
                        .await?;
                    sqlx::query("SET idle_in_transaction_session_timeout='2s'")
                        .execute(&mut *connection)
                        .await?;
                    Ok(())
                })
            })
            .connect_lazy_with(
                pool.connect_options()
                    .as_ref()
                    .clone()
                    .application_name("aq-terminal"),
            );
        let terminal_data_pool = PgPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(Duration::from_secs(1))
            .after_connect(|connection, _| {
                Box::pin(async move {
                    sqlx::query("SET statement_timeout='750ms'")
                        .execute(&mut *connection)
                        .await?;
                    sqlx::query("SET lock_timeout='100ms'")
                        .execute(&mut *connection)
                        .await?;
                    sqlx::query("SET idle_in_transaction_session_timeout='2s'")
                        .execute(&mut *connection)
                        .await?;
                    Ok(())
                })
            })
            .connect_lazy_with(
                pool.connect_options()
                    .as_ref()
                    .clone()
                    .application_name("aq-terminal-data"),
            );
        Ok(Store {
            terminal_pool,
            terminal_data_pool,
            observation_pool,
            notification_pool,
            pool,
            control_pool,
            admission: std::sync::Arc::new(crate::admission::Admission::default()),
            verified_profiled_facts: Default::default(),
            #[cfg(feature = "recovery-test-support")]
            recovery_test: std::sync::Arc::new(crate::execution::testing::Controls::default()),
        })
    }

    pub async fn find_user_by_binding(&self, issuer: &str, subject: &str) -> Result<Option<User>> {
        sqlx::query_as::<_, UserRow>(&format!(
            "SELECT {USER_COLS} FROM users WHERE issuer = $1 AND subject = $2"
        ))
        .bind(issuer)
        .bind(subject)
        .fetch_optional(&self.pool)
        .await?
        .map(User::try_from)
        .transpose()
    }

    /// Binds an invited (unbound) user, matched by email case-insensitively.
    pub async fn bind_invited_user(
        &self,
        email: &str,
        issuer: &str,
        subject: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<User>> {
        sqlx::query_as::<_, UserRow>(&format!(
            "UPDATE users SET issuer = $2, subject = $3, bound_at = $4 \
             WHERE lower(email) = lower($1) AND issuer IS NULL RETURNING {USER_COLS}"
        ))
        .bind(email)
        .bind(issuer)
        .bind(subject)
        .bind(now)
        .fetch_optional(&self.pool)
        .await?
        .map(User::try_from)
        .transpose()
    }

    /// Creates the first owner exactly once; later calls return `None`.
    pub async fn bootstrap_owner(
        &self,
        email: &str,
        issuer: &str,
        subject: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<User>> {
        let mut tx = self.pool.begin().await?;
        let claimed = sqlx::query(
            "INSERT INTO meta (key, value) VALUES ('owner_bootstrap_completed_at', $1) \
             ON CONFLICT DO NOTHING",
        )
        .bind(now.to_rfc3339())
        .execute(&mut *tx)
        .await?;
        if claimed.rows_affected() == 0 {
            return Ok(None);
        }
        let row = sqlx::query_as::<_, UserRow>(&format!(
            "INSERT INTO users (id, email, role, issuer, subject, created_at, bound_at) \
             VALUES ($1, $2, 'owner', $3, $4, $5, $5) RETURNING {USER_COLS}"
        ))
        .bind(Uuid::new_v4())
        .bind(email)
        .bind(issuer)
        .bind(subject)
        .bind(now)
        .fetch_one(&mut *tx)
        .await
        .map_err(map_user_insert_err)?;
        tx.commit().await?;
        Ok(Some(row.try_into()?))
    }

    pub async fn invite_user(&self, email: &str, role: Role, now: DateTime<Utc>) -> Result<User> {
        sqlx::query_as::<_, UserRow>(&format!(
            "INSERT INTO users (id, email, role, created_at) VALUES ($1, $2, $3, $4) \
             RETURNING {USER_COLS}"
        ))
        .bind(Uuid::new_v4())
        .bind(email)
        .bind(role.as_str())
        .bind(now)
        .fetch_one(&self.pool)
        .await
        .map_err(map_user_insert_err)?
        .try_into()
    }

    pub async fn list_users(&self) -> Result<Vec<User>> {
        sqlx::query_as::<_, UserRow>(&format!(
            "SELECT {USER_COLS} FROM users ORDER BY created_at, email"
        ))
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(User::try_from)
        .collect()
    }

    pub async fn create_enrollment(
        &self,
        code_hash: &str,
        created_by: Uuid,
        now: DateTime<Utc>,
        expires_at: DateTime<Utc>,
    ) -> Result<Uuid> {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO enrollment_codes (id, code_hash, created_by, created_at, expires_at) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id)
        .bind(code_hash)
        .bind(created_by)
        .bind(now)
        .bind(expires_at)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// Atomically consumes a valid code and registers the node.
    pub async fn redeem_enrollment(
        &self,
        code_hash: &str,
        node: NewNode,
        now: DateTime<Utc>,
    ) -> Result<Option<Node>> {
        let node_id = Uuid::new_v4();
        let mut tx = self.pool.begin().await?;
        let claimed = sqlx::query(
            "UPDATE enrollment_codes SET used_at = $1, node_id = $2 \
             WHERE code_hash = $3 AND used_at IS NULL AND expires_at > $1",
        )
        .bind(now)
        .bind(node_id)
        .bind(code_hash)
        .execute(&mut *tx)
        .await?;
        if claimed.rows_affected() != 1 {
            return Ok(None);
        }
        let row = sqlx::query_as::<_, NodeRow>(&format!(
            "INSERT INTO nodes (id, name, credential_hash, worker_version, protocol_version, \
             enrolled_at) VALUES ($1, $2, $3, $4, $5, $6) RETURNING {NODE_COLS}"
        ))
        .bind(node_id)
        .bind(&node.name)
        .bind(&node.credential_hash)
        .bind(&node.worker_version)
        .bind(node.protocol_version as i32)
        .bind(now)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some(row.into()))
    }

    /// Returns revoked nodes too so callers can tell revoked from unknown.
    pub async fn find_node_by_credential(&self, credential_hash: &str) -> Result<Option<Node>> {
        Ok(sqlx::query_as::<_, NodeRow>(&format!(
            "SELECT {NODE_COLS} FROM nodes WHERE credential_hash = $1"
        ))
        .bind(credential_hash)
        .fetch_optional(&self.pool)
        .await?
        .map(Node::from))
    }

    pub async fn record_heartbeat(
        &self,
        node_id: Uuid,
        worker_version: &str,
        protocol_version: u32,
        now: DateTime<Utc>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE nodes SET last_seen_at = $2, worker_version = $3, protocol_version = $4 \
             WHERE id = $1",
        )
        .bind(node_id)
        .bind(now)
        .bind(worker_version)
        .bind(protocol_version as i32)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Returns true if the node was newly revoked.
    pub async fn revoke_node(&self, node_id: Uuid, now: DateTime<Utc>) -> Result<bool> {
        let r =
            sqlx::query("UPDATE nodes SET revoked_at = $2 WHERE id = $1 AND revoked_at IS NULL")
                .bind(node_id)
                .bind(now)
                .execute(&self.pool)
                .await?;
        Ok(r.rows_affected() == 1)
    }

    pub async fn list_nodes(&self) -> Result<Vec<Node>> {
        Ok(sqlx::query_as::<_, NodeRow>(&format!(
            "SELECT {NODE_COLS} FROM nodes ORDER BY enrolled_at, name"
        ))
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(Node::from)
        .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::MIGRATIONS;
    use crate::{READABLE_STATE_FORMATS, STATE_FORMAT_VERSION};

    /// Every migration is a state-format change. The updater decides whether
    /// a rollback must restore the pre-update backup by comparing manifest
    /// state formats, while the server refuses a database holding any
    /// migration newer than it knows. A migration added without bumping
    /// `STATE_FORMAT_VERSION` would make the updater roll back without a
    /// restore onto data the old server refuses to open.
    #[test]
    fn state_format_is_the_latest_migration() {
        let numbers: Vec<i32> = MIGRATIONS.iter().map(|m| m.0).collect();
        let expected: Vec<i32> = (1..=numbers.len() as i32).collect();
        assert_eq!(
            numbers, expected,
            "migrations must be numbered 1, 2, 3, ... with no gaps"
        );
        let latest = *numbers.last().expect("at least one migration") as u32;
        assert_eq!(
            STATE_FORMAT_VERSION, latest,
            "STATE_FORMAT_VERSION must equal the newest migration number: every migration is a \
             state-format change. Bump STATE_FORMAT_VERSION (and READABLE_STATE_FORMATS, and the \
             Dockerfile's AQ_STATE_FORMAT default) together with the migration"
        );
        assert!(READABLE_STATE_FORMATS.contains(&STATE_FORMAT_VERSION));
        for (n, sql) in MIGRATIONS {
            assert!(
                sql.contains("state_format_version") && sql.contains(&format!("'{n}'")),
                "migration {n} must set meta.state_format_version to '{n}'"
            );
        }
    }
}
