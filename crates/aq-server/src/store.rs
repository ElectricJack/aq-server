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
    (
        22,
        include_str!("../migrations/0022_profiled_source_exports.sql"),
    ),
    (23, include_str!("../migrations/0023_playbook_bridge.sql")),
    (24, include_str!("../migrations/0024_profiled_results.sql")),
    (
        25,
        include_str!("../migrations/0025_native_source_observations.sql"),
    ),
    (26, include_str!("../migrations/0026_review_owners.sql")),
    (
        27,
        include_str!("../migrations/0027_native_definition_identities.sql"),
    ),
    (
        28,
        include_str!("../migrations/0028_profiled_launch_closure.sql"),
    ),
    (
        29,
        include_str!("../migrations/0029_scoped_material_presence.sql"),
    ),
    (
        30,
        include_str!("../migrations/0030_accepted_preparation_v2.sql"),
    ),
    (
        31,
        include_str!("../migrations/0031_presence_binding_barrier.sql"),
    ),
    (32, include_str!("../migrations/0032_phase_execution.sql")),
    (33, include_str!("../migrations/0033_phase_terminal.sql")),
    (
        34,
        include_str!("../migrations/0034_human_graph_operations.sql"),
    ),
    (
        35,
        include_str!("../migrations/0035_supervisor_conversation.sql"),
    ),
    (
        36,
        include_str!("../migrations/0036_result_media_owner.sql"),
    ),
    (
        37,
        include_str!("../migrations/0037_central_reservations.sql"),
    ),
    (
        38,
        include_str!("../migrations/0038_profiled_result_origins.sql"),
    ),
    (39, include_str!("../migrations/0039_pilot_settlement.sql")),
    (40, include_str!("../migrations/0040_phase_never_granted.sql")),
];

/// Applies missing migrations in one transaction under an advisory lock.
/// Refuses, changing nothing, when the database is from a newer server.
async fn migrate(pool: &PgPool) -> Result<()> {
    migrate_known(pool, MIGRATIONS, STATE_FORMAT_VERSION).await
}
/// Same migration/refusal implementation with an explicitly older SQL profile.
/// Exported only through the disposable test-support boundary, never a route.
#[cfg(any(test, feature = "test-support"))]
pub(crate) async fn migrate_fixture_profile(pool: &PgPool, supported: u32) -> Result<()> {
    let last = MIGRATIONS
        .iter()
        .position(|(n, _)| *n as u32 == supported)
        .ok_or_else(|| StoreError::Corrupt("unknown fixture migration profile".into()))?;
    migrate_known(pool, &MIGRATIONS[..=last], supported).await
}
async fn migrate_known(pool: &PgPool, migrations: &[(i32, &str)], supported: u32) -> Result<()> {
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
            if found > supported {
                return Err(StoreError::IncompatibleState { found, supported });
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
    let newest_known = migrations.iter().map(|m| m.0).max().unwrap_or(0);
    if let Some(&found) = applied.iter().filter(|v| **v > newest_known).max() {
        return Err(StoreError::IncompatibleState {
            found: found as u32,
            supported,
        });
    }

    for (version, sql) in migrations {
        if applied.contains(version) {
            continue;
        }
        sqlx::raw_sql(sql).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO schema_migrations (version) VALUES ($1)")
            .bind(version)
            .execute(&mut *tx)
            .await?;
    }
    let format: Option<String> =
        sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
            .fetch_optional(&mut *tx)
            .await?;
    if format.as_deref() != Some(supported.to_string().as_str()) {
        return Err(StoreError::Corrupt(
            "state format metadata does not match applied migrations".into(),
        ));
    }
    if supported >= 30 {
        let complete:bool=sqlx::query_scalar("SELECT to_regclass('profiled_v2_proposals') IS NOT NULL AND to_regclass('profiled_v2_accepts') IS NOT NULL").fetch_one(&mut *tx).await?;
        if !complete {
            return Err(StoreError::Corrupt(
                "accepted preparation v2 storage missing from current schema".into(),
            ));
        }
    }
    if supported >= 31 {
        let complete: bool = sqlx::query_scalar("SELECT to_regprocedure('profiled_provisionable_binding_set(jsonb)') IS NOT NULL AND EXISTS(SELECT 1 FROM information_schema.columns WHERE table_schema='public' AND table_name='profiled_inventory_semantic_heads' AND column_name='provisionable_binding_set' AND data_type='jsonb')")
            .fetch_one(&mut *tx).await?;
        if !complete {
            return Err(StoreError::Corrupt(
                "scoped presence binding barrier missing from current schema".into(),
            ));
        }
    }
    if supported >= 32 {
        let complete: bool = sqlx::query_scalar("SELECT to_regclass('profiled_phase_execution_grants') IS NOT NULL AND to_regprocedure('profiled_phase_execution_guard()') IS NOT NULL AND EXISTS(SELECT 1 FROM pg_trigger WHERE tgname='profiled_phase_execution_immutable' AND tgrelid=to_regclass('profiled_phase_execution_grants') AND NOT tgisinternal AND tgenabled='O')")
            .fetch_one(&mut *tx).await?;
        if !complete {
            return Err(StoreError::Corrupt(
                "phase execution history storage missing from current schema".into(),
            ));
        }
    }
    if supported >= 33 {
        let complete:bool=sqlx::query_scalar(r#"SELECT
          to_regclass('profiled_phase_terminal_capacity') IS NOT NULL
          AND to_regprocedure('profiled_phase_terminal_capacity_guard()') IS NOT NULL
          AND to_regprocedure('profiled_phase_terminal_reserve()') IS NOT NULL
          AND to_regprocedure('profiled_phase_terminal_origin_guard()') IS NOT NULL
          AND to_regprocedure('profiled_phase_terminal_extra_bytes(text,text,text,bytea,bytea,jsonb)') IS NOT NULL
          AND to_regprocedure('profiled_phase_terminal_used_bytes(uuid)') IS NOT NULL
          AND to_regprocedure('profiled_phase_unqualified_result_guard()') IS NOT NULL
          AND NOT EXISTS(
            SELECT 1 FROM (VALUES
             ('profiled_phase_terminal_capacity','profiled_phase_terminal_capacity_guard'),
             ('profiled_phase_execution_grants','profiled_phase_terminal_reserve'),
             ('profiled_started_receipts','profiled_phase_terminal_origin_guard'),
             ('profiled_stopped_receipts','profiled_phase_terminal_origin_guard'),
             ('profiled_launch_closures','profiled_phase_terminal_origin_guard'),
             ('profiled_result_reports','profiled_phase_unqualified_result_guard'),
             ('profiled_verified_releases',CASE WHEN $1>=39 THEN 'profiled_phase_qualified_release_guard' ELSE 'profiled_phase_unqualified_release_guard' END)
            ) expected(rel,name) WHERE NOT EXISTS(SELECT 1 FROM pg_trigger t WHERE t.tgrelid=to_regclass(expected.rel) AND t.tgname=expected.name AND NOT t.tgisinternal AND t.tgenabled IN ('O','A') AND t.tgfoid=to_regprocedure(CASE WHEN expected.name='profiled_phase_unqualified_release_guard' THEN 'profiled_phase_unqualified_result_guard()' WHEN expected.name='profiled_phase_qualified_release_guard' THEN 'settlement_release_guard()' ELSE expected.name||'()' END)))
          AND NOT EXISTS(
            SELECT 1 FROM (VALUES ('profiled_started_receipts'),('profiled_stopped_receipts'),('profiled_launch_closures')) expected(rel)
            CROSS JOIN (VALUES ('initial_authority_kind','text'),('initial_authority_receipt_digest','text'),('phase_request_digest','text'),('phase_envelope_bytes','bytea'),('phase_ack_bytes','bytea'),('phase_terminal_qualification','jsonb')) col(name,kind)
            WHERE NOT EXISTS(SELECT 1 FROM information_schema.columns c WHERE c.table_schema='public' AND c.table_name=expected.rel AND c.column_name=col.name AND c.udt_name=col.kind))
          AND NOT EXISTS(
            SELECT 1 FROM (VALUES ('profiled_started_receipts'),('profiled_stopped_receipts'),('profiled_launch_closures')) expected(rel)
            CROSS JOIN (VALUES ('phase_envelope_digest'),('phase_ack_digest')) col(name)
            WHERE NOT EXISTS(SELECT 1 FROM pg_attribute a WHERE a.attrelid=to_regclass(expected.rel) AND a.attname=col.name AND a.attgenerated='s' AND NOT a.attisdropped))
          AND NOT EXISTS(SELECT 1 FROM (VALUES ('profiled_started_receipts'),('profiled_stopped_receipts'),('profiled_launch_closures')) expected(rel) WHERE NOT EXISTS(SELECT 1 FROM pg_constraint c WHERE c.conrelid=to_regclass(expected.rel) AND c.conname=expected.rel||'_phase_origin_check' AND c.contype='c' AND c.convalidated))
          AND NOT EXISTS(
            SELECT 1 FROM (VALUES ('legacy_origin_operation_id'),('phase_origin_operation_id')) col(name)
            WHERE NOT EXISTS(SELECT 1 FROM pg_attribute a WHERE a.attrelid=to_regclass('profiled_launch_closures') AND a.attname=col.name AND a.attgenerated='s' AND a.atttypid='uuid'::regtype AND NOT a.attisdropped))
          AND EXISTS(SELECT 1 FROM pg_constraint WHERE conrelid=to_regclass('profiled_launch_closures') AND conname='profiled_launch_closures_legacy_origin_fk' AND contype='f' AND convalidated AND confrelid=to_regclass('profiled_execution_receipts'))
          AND EXISTS(SELECT 1 FROM pg_constraint WHERE conrelid=to_regclass('profiled_launch_closures') AND conname='profiled_launch_closures_phase_origin_fk' AND contype='f' AND convalidated AND confrelid=to_regclass('profiled_phase_execution_grants'))
        "#).bind(supported as i32).fetch_one(&mut *tx).await?;
        if !complete {
            return Err(StoreError::Corrupt(
                "phase terminal origin/capacity storage missing or disabled in current schema"
                    .into(),
            ));
        }
    }
    if supported >= 34 {
        let complete: bool = sqlx::query_scalar("SELECT to_regclass('human_graph_operations') IS NOT NULL AND to_regclass('human_graph_operation_actor') IS NOT NULL AND to_regprocedure('guard_human_graph_operation_identity()') IS NOT NULL AND NOT EXISTS(SELECT 1 FROM (VALUES ('human_graph_operation_identity_guard',19),('human_graph_operation_delete_guard',11),('human_graph_operation_truncate_guard',34)) expected(name,kind) WHERE NOT EXISTS(SELECT 1 FROM pg_trigger t WHERE t.tgrelid=to_regclass('human_graph_operations') AND t.tgname=expected.name AND t.tgtype=expected.kind AND NOT t.tgisinternal AND t.tgenabled IN ('O','A') AND t.tgfoid=to_regprocedure('guard_human_graph_operation_identity()')))")
            .fetch_one(&mut *tx).await?;
        if !complete {
            return Err(StoreError::Corrupt(
                "human Graph intent journal storage missing from current schema".into(),
            ));
        }
    }
    if supported >= 35 {
        let complete: bool = sqlx::query_scalar("SELECT to_regclass('global_supervisor_conversation') IS NOT NULL AND to_regprocedure('guard_global_supervisor_conversation()') IS NOT NULL AND NOT EXISTS(SELECT 1 FROM (VALUES ('global_supervisor_conversation_identity_guard',19),('global_supervisor_conversation_delete_guard',11),('global_supervisor_conversation_truncate_guard',34)) expected(name,kind) WHERE NOT EXISTS(SELECT 1 FROM pg_trigger t WHERE t.tgrelid=to_regclass('global_supervisor_conversation') AND t.tgname=expected.name AND t.tgtype=expected.kind AND NOT t.tgisinternal AND t.tgenabled IN ('O','A') AND t.tgfoid=to_regprocedure('guard_global_supervisor_conversation()')))")
            .fetch_one(&mut *tx).await?;
        if !complete {
            return Err(StoreError::Corrupt(
                "global supervisor conversation storage missing or disabled".into(),
            ));
        }
        let singleton: bool = sqlx::query_scalar("SELECT count(*)=1 AND COALESCE(bool_and(singleton AND revision=1 AND conversation_id<>'00000000-0000-0000-0000-000000000000'::uuid),FALSE) FROM global_supervisor_conversation")
            .fetch_one(&mut *tx).await?;
        if !singleton {
            return Err(StoreError::Corrupt(
                "global supervisor conversation singleton missing or invalid".into(),
            ));
        }
    }
    if supported >= 36 {
        media36_storage_guard(&mut tx).await?;
    }
    if supported >= 37 {
        central37::central37_storage_guard(&mut tx).await?;
    }
    if supported == 38 {
        result38::validate(&mut tx).await?;
    }
    if supported >= 40 {
        settlement39::validate_phase_absence(&mut tx).await?;
    } else if supported >= 39 {
        settlement39::validate(&mut tx).await?;
    }
    if supported >= 40 {
        never_granted40::validate(&mut tx).await?;
    }
    tx.commit().await?;
    Ok(())
}

#[derive(Clone)]
pub struct Store {
    pub(crate) pool: PgPool,
    control_pool: PgPool,
    renewal_pool: PgPool,
    presence_pool: PgPool,
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

    /// Dedicated permit-maintenance connections; initial admission/bulk never use them.
    pub(crate) fn presence_view(&self) -> Self {
        Self {
            pool: self.presence_pool.clone(),
            ..self.clone()
        }
    }
    #[cfg(feature = "test-support")]
    pub fn presence_pool_for_testing(&self) -> PgPool {
        self.presence_pool.clone()
    }
    pub(crate) fn renewal_view(&self) -> Self {
        Self {
            pool: self.renewal_pool.clone(),
            ..self.clone()
        }
    }
    #[cfg(feature = "test-support")]
    pub fn renewal_pool_for_testing(&self) -> PgPool {
        self.renewal_pool.clone()
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
        self.renewal_pool.close().await;
        self.presence_pool.close().await;
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
        Self::initialize_pools(pool).await
    }

    /// Test-only actual frozen migration prefix; never a production fallback.
    #[cfg(feature = "test-support")]
    pub(crate) async fn connect_state32_for_testing(options: PgConnectOptions) -> Result<Store> {
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .acquire_timeout(Duration::from_secs(5))
            .connect_with(options)
            .await?;
        migrate_known(&pool, &MIGRATIONS[..32], 32).await?;
        Self::initialize_pools(pool).await
    }

    async fn initialize_pools(pool: PgPool) -> Result<Store> {
        let control_pool = PgPoolOptions::new()
            .max_connections(4)
            .acquire_timeout(Duration::from_secs(2))
            .connect_lazy_with(pool.connect_options().as_ref().clone());
        let renewal_pool = PgPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(Duration::from_millis(250))
            .after_connect(|connection, _| {
                Box::pin(async move {
                    sqlx::query("SET statement_timeout='750ms'")
                        .execute(&mut *connection)
                        .await?;
                    sqlx::query("SET lock_timeout='150ms'")
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
                    .application_name("aq-native-renewal"),
            );
        let presence_pool = PgPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(Duration::from_millis(250))
            .after_connect(|connection, _| {
                Box::pin(async move {
                    sqlx::query("SET statement_timeout='750ms'")
                        .execute(&mut *connection)
                        .await?;
                    sqlx::query("SET lock_timeout='150ms'")
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
                    .application_name("aq-material-presence"),
            );
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
            renewal_pool,
            presence_pool,
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
#[path = "store/human_graph34_tests.rs"]
mod human_graph34_tests;

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

#[cfg(test)]
#[path = "store/supervisor35_tests.rs"]
mod supervisor35_tests;

#[path = "store/media36.rs"]
mod media36;
use media36::media36_storage_guard;
#[cfg(test)]
#[path = "store/media36_tests.rs"]
mod media36_tests;

#[path = "store/central37.rs"]
mod central37;
#[cfg(test)]
#[path = "store/central37_tests.rs"]
mod central37_tests;

#[path = "store/result38.rs"]
mod result38;
#[cfg(test)]
#[path = "store/result38_tests.rs"]
mod result38_tests;

#[path = "store/settlement39.rs"]
mod settlement39;
#[cfg(test)]
#[path = "store/settlement39_tests.rs"]
mod settlement39_tests;

#[path = "store/never_granted40.rs"]
mod never_granted40;
