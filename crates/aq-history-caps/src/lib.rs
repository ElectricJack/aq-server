//! Spec 0008 phase A: live sealed-history usage, the poll gate and swarm holds.
//!
//! Migration 0103's SQL functions are the only definition of a live row. The
//! database triggers, these Rust mirror checks, the poll gate and the swarm read
//! all call them. Before 0103 (frozen-format fixtures) every row counts, as it
//! did before spec 0008.
use sqlx::PgConnection;
use uuid::Uuid;

const LIVE_SCHEMA: &str =
    "SELECT to_regprocedure('sealed_history_usage(text,uuid,bigint,uuid)') IS NOT NULL";
const LEGACY_STAGING: &str = "SELECT (COALESCE((SELECT sum((manifest->>'manifest_bytes')::BIGINT+(manifest->>'bundle_bytes')::BIGINT) FROM source_transfers WHERE NOT staging_released),0)+COALESCE((SELECT sum(reserved_bytes) FROM source_bases),0)+COALESCE((SELECT sum(charged_bytes) FROM profiled_source_exports),0))::BIGINT";

/// The usage kinds Rust checks mirror (the SQL function knows more).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    IntelligenceDecisions,
    SourceExports,
    NativePreparation,
    V2Proposals,
    LaunchRegistrations,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::IntelligenceDecisions => "intelligence_decisions",
            Kind::SourceExports => "source_exports",
            Kind::NativePreparation => "native_preparation",
            Kind::V2Proposals => "v2_proposals",
            Kind::LaunchRegistrations => "launch_registrations",
        }
    }
    /// The all-rows query used before migration 0103; `$1`, when present, is the scope.
    fn legacy(self) -> Option<&'static str> {
        match self {
            Kind::IntelligenceDecisions => Some(
                "SELECT count(*),COALESCE(sum(pg_column_size(decision)),0)::BIGINT,count(*),COALESCE(sum(pg_column_size(decision)),0)::BIGINT FROM intelligence_decisions",
            ),
            Kind::SourceExports => Some(
                "SELECT count(*),COALESCE(sum(octet_length(request_bytes)+COALESCE(octet_length(verified_bytes),4194304)),0)::BIGINT,count(*) FILTER(WHERE project_id=$1),COALESCE(sum(octet_length(request_bytes)+COALESCE(octet_length(verified_bytes),4194304)) FILTER(WHERE project_id=$1),0)::BIGINT FROM profiled_source_exports",
            ),
            Kind::V2Proposals => Some(
                "SELECT count(*),0::BIGINT,count(*) FILTER(WHERE node_id=$1),0::BIGINT FROM profiled_v2_proposals",
            ),
            Kind::LaunchRegistrations => Some(
                "SELECT count(*),COALESCE(sum(logical_bytes)::BIGINT,0),count(*) FILTER(WHERE node_id=$1),COALESCE((sum(logical_bytes) FILTER(WHERE node_id=$1))::BIGINT,0) FROM profiled_launch_registrations",
            ),
            Kind::NativePreparation => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub total_rows: i64,
    pub total_bytes: i64,
    pub scope_rows: i64,
    pub scope_bytes: i64,
}

impl From<(i64, i64, i64, i64)> for Usage {
    fn from((total_rows, total_bytes, scope_rows, scope_bytes): (i64, i64, i64, i64)) -> Self {
        Self {
            total_rows,
            total_bytes,
            scope_rows,
            scope_bytes,
        }
    }
}

fn ms(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// Whether migration 0103's live definition is present.
pub async fn live_schema(conn: &mut PgConnection) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, bool>(LIVE_SCHEMA)
        .fetch_one(conn)
        .await
}

/// Live usage, or `None` before migration 0103.
pub async fn live_usage(
    conn: &mut PgConnection,
    kind: Kind,
    scope: Option<Uuid>,
    now_ms: u64,
) -> Result<Option<Usage>, sqlx::Error> {
    if !live_schema(&mut *conn).await? {
        return Ok(None);
    }
    let row: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT total_rows,total_bytes,scope_rows,scope_bytes FROM sealed_history_usage($1,$2,$3)",
    )
    .bind(kind.name())
    .bind(scope)
    .bind(ms(now_ms))
    .fetch_one(&mut *conn)
    .await?;
    Ok(Some(row.into()))
}

/// Live usage, or every row before migration 0103.
pub async fn usage(
    conn: &mut PgConnection,
    kind: Kind,
    scope: Option<Uuid>,
    now_ms: u64,
) -> Result<Usage, sqlx::Error> {
    if let Some(u) = live_usage(&mut *conn, kind, scope, now_ms).await? {
        return Ok(u);
    }
    let sql = kind
        .legacy()
        .ok_or_else(|| sqlx::Error::Protocol(format!("{} has no pre-0103 query", kind.name())))?;
    let query = sqlx::query_as::<_, (i64, i64, i64, i64)>(sql);
    let query = if sql.contains("$1") {
        query.bind(scope)
    } else {
        query
    };
    Ok(query.fetch_one(&mut *conn).await?.into())
}

/// Staging bytes: unreleased transfers, source bases and (live) source exports.
pub async fn staging_used(conn: &mut PgConnection, now_ms: i64) -> Result<i64, sqlx::Error> {
    if live_schema(&mut *conn).await? {
        sqlx::query_scalar("SELECT sealed_staging_live_bytes($1)")
            .bind(now_ms)
            .fetch_one(&mut *conn)
            .await
    } else {
        sqlx::query_scalar(LEGACY_STAGING)
            .fetch_one(&mut *conn)
            .await
    }
}

/// The INSERT of a pending source export. From migration 0103 it writes
/// `created_at_ms` (`$6`), so the quota trigger sees the real time at insert and
/// the pending window is on; before, it ignores `$6`.
async fn export_insert_sql(conn: &mut PgConnection) -> Result<&'static str, sqlx::Error> {
    const BEFORE: &str = "INSERT INTO profiled_source_exports(operation_id,project_id,request_bytes,request_digest,reserved_bytes,charged_bytes,state) SELECT $1,$2,$3,$4,$5,$5,'pending' WHERE $6::bigint>0";
    const LIVE: &str = "INSERT INTO profiled_source_exports(operation_id,project_id,request_bytes,request_digest,reserved_bytes,charged_bytes,state,created_at_ms) SELECT $1,$2,$3,$4,$5,$5,'pending',$6::bigint";
    Ok(if live_schema(conn).await? {
        LIVE
    } else {
        BEFORE
    })
}

/// Reserves a pending source export, written with its creation time.
/// `ids` is (operation, project), `request` (bytes, digest), `reservation`
/// (reserved bytes, creation time in ms).
pub async fn insert_export(
    conn: &mut PgConnection,
    ids: (Uuid, Uuid),
    request: (&[u8], &str),
    reservation: (i64, i64),
) -> Result<(), sqlx::Error> {
    let sql = export_insert_sql(&mut *conn).await?;
    sqlx::query(sql)
        .bind(ids.0)
        .bind(ids.1)
        .bind(request.0)
        .bind(request.1)
        .bind(reservation.0)
        .bind(reservation.1)
        .execute(conn)
        .await?;
    Ok(())
}

/// The indexes migration 0103 adds on tables the settlement39 catalog attests
/// (`execution_attempts`, `profiled_authority_hot`, `source_transfers`), as the
/// catalog rows it expects from format 103 on. Any other index there still fails.
pub const ATTESTED_INDEXES: [(&str, &str); 4] = [
    (
        "sealed_attempt_held",
        "CREATE INDEX sealed_attempt_held ON public.execution_attempts USING btree (attempt_id) WHERE holds_task",
    ),
    (
        "sealed_attempt_unsettled",
        "CREATE INDEX sealed_attempt_unsettled ON public.execution_attempts USING btree (attempt_id) WHERE ((NOT holds_task) AND (kind = 'legacy_fixture'::text) AND (COALESCE((state ->> 'status'::text), 'unknown'::text) <> ALL (ARRAY['finished'::text, 'declined'::text, 'never_granted'::text])))",
    ),
    (
        "sealed_hot_unsettled",
        "CREATE INDEX sealed_hot_unsettled ON public.profiled_authority_hot USING btree (attempt_id) WHERE ((state ->> 'status'::text) <> ALL (ARRAY['finished'::text, 'declined'::text, 'never_granted'::text]))",
    ),
    (
        "sealed_transfer_unreleased",
        "CREATE INDEX sealed_transfer_unreleased ON public.source_transfers USING btree (transfer_id) WHERE (NOT staging_released)",
    ),
];

/// `ATTESTED_INDEXES` as catalog rows (`kind`, `name`, `body`).
pub fn attested_index_rows() -> Vec<serde_json::Value> {
    (ATTESTED_INDEXES.iter())
        .map(|(name, definition)| {
            serde_json::json!({"kind": "index", "name": name, "body": {
            "definition": definition, "valid": true, "ready": true, "live": true,
            "primary": false, "unique": false, "immediate": true, "nulls_not_distinct": false}})
        })
        .collect()
}

/// Whether the attested catalog equals the expected one. From format 103 the
/// expected rows include `ATTESTED_INDEXES`; both sides are compared in
/// (kind, name) order, so the database collation does not matter.
pub fn catalog_matches(
    expected: &mut serde_json::Value,
    actual: &mut serde_json::Value,
    supported: u32,
) -> bool {
    use serde_json::Value;
    let key = |r: &Value| (r["kind"].to_string(), r["name"].to_string());
    match (expected.as_array_mut(), actual.as_array_mut()) {
        (Some(expected), Some(actual)) => {
            if supported >= 103 {
                expected.extend(attested_index_rows());
            }
            expected.sort_by_key(key);
            actual.sort_by_key(key);
            expected == actual
        }
        _ => false,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Code {
    RateLimited,
    CapReached,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Node(Uuid),
    Project(Uuid),
    Global,
}

/// One limit that holds new offers back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hold {
    pub code: Code,
    /// A `sealed_history_limits()` kind, or `staging_exports`.
    pub kind: String,
    pub scope: Scope,
    pub bytes: bool,
    pub used: i64,
    pub limit: i64,
}

/// (max_staging_bytes, max_bundle_bytes) from the source retention config.
pub type Staging = (u64, u64);

fn capitalised(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

impl Hold {
    pub fn code_name(&self) -> &'static str {
        match self.code {
            Code::RateLimited => "history_rate_limited",
            Code::CapReached => "history_cap_reached",
        }
    }
    pub fn subject(&self) -> &'static str {
        match (self.kind.as_str(), self.bytes) {
            ("admission_artifacts", _) => "admission artifacts",
            ("intelligence_decisions", _) => "selection decisions",
            ("source_exports", _) => "source exports",
            ("staging_exports", _) => "staging bytes",
            ("native_preparation", true) => "preparation bytes",
            ("native_preparation", false) => "material leases",
            ("v2_proposals", _) => "preparation proposals",
            ("launch_registrations", _) => "launch registrations",
            ("phase_terminal", _) => "phase terminal reservations",
            ("presence_operations", _) => "inventory reports",
            _ => "sealed history",
        }
    }
    pub fn message(&self) -> String {
        let (used, limit) = (self.used, self.limit);
        let body = match (self.code, self.kind.as_str()) {
            (Code::RateLimited, "presence_rate") => format!(
                "inventory report rate limit reached: {used} reports in the last hour (limit {limit})"
            ),
            (Code::RateLimited, _) => {
                format!("offer rate limit reached: {used} offers in the last hour (limit {limit})")
            }
            (Code::CapReached, _) => format!(
                "live {} cap reached: {used} of {limit}{}",
                self.subject(),
                if self.bytes { " bytes" } else { "" }
            ),
        };
        match self.scope {
            Scope::Node(_) => capitalised(&body),
            Scope::Project(p) => format!("Project {p}: {body}"),
            Scope::Global => format!("Server: {body}"),
        }
    }
    pub fn remedy(&self) -> &'static str {
        match self.code {
            Code::RateLimited => {
                "Wait for the hour to pass; if this repeats, look for a task that fails in a loop"
            }
            Code::CapReached => "Wait for running agents to settle",
        }
    }
    pub fn log_line(&self) -> String {
        format!(
            "{} {} {:?}: {} of {}",
            self.code_name(),
            self.kind,
            self.scope,
            self.used,
            self.limit
        )
    }
}

type HoldRow = (String, String, String, Option<Uuid>, String, i64, i64);

async fn sql_holds(
    conn: &mut PgConnection,
    nodes: &[Uuid],
    projects: &[Uuid],
    now_ms: u64,
) -> Result<Vec<Hold>, sqlx::Error> {
    let rows: Vec<HoldRow> = sqlx::query_as(
        "SELECT hold_code,hold_kind,hold_scope,hold_scope_id,hold_unit,hold_used,hold_limit FROM sealed_history_holds($1,$2,$3)",
    )
    .bind(nodes)
    .bind(projects)
    .bind(ms(now_ms))
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(code, kind, scope, id, unit, used, limit)| Hold {
            code: if code == "history_rate_limited" {
                Code::RateLimited
            } else {
                Code::CapReached
            },
            scope: match (scope.as_str(), id) {
                ("node", Some(n)) => Scope::Node(n),
                ("project", Some(p)) => Scope::Project(p),
                _ => Scope::Global,
            },
            kind,
            bytes: unit == "bytes",
            used,
            limit,
        })
        .collect())
}

/// Holds for the listed nodes (server limits once) and projects, from one call
/// that computes each kind's live usage once. Empty before migration 0103.
pub async fn holds(
    conn: &mut PgConnection,
    nodes: &[Uuid],
    projects: &[Uuid],
    now_ms: u64,
    staging: Option<Staging>,
) -> Result<Vec<Hold>, sqlx::Error> {
    if !live_schema(&mut *conn).await? {
        return Ok(Vec::new());
    }
    let mut holds = sql_holds(&mut *conn, nodes, projects, now_ms).await?;
    if let (Some((max, bundle)), false) = (staging, nodes.is_empty()) {
        let used = staging_used(&mut *conn, ms(now_ms)).await?;
        let limit = ms(max);
        if used.saturating_add(ms(bundle)) > limit {
            holds.push(Hold {
                code: Code::CapReached,
                kind: "staging_exports".into(),
                scope: Scope::Global,
                bytes: true,
                used,
                limit,
            });
        }
    }
    Ok(holds)
}

/// Node and server limits for a poll of `node`. Empty before migration 0103.
pub async fn node_holds(
    conn: &mut PgConnection,
    node: Uuid,
    now_ms: u64,
    staging: Option<Staging>,
) -> Result<Vec<Hold>, sqlx::Error> {
    holds(conn, &[node], &[], now_ms, staging).await
}

/// What one poll learns from the holds. A node or server hold stops the poll; a
/// project hold only drops that project from this poll's candidates.
#[derive(Debug, Default)]
pub struct PollGate {
    pub stop: Option<Hold>,
    held: Vec<Uuid>,
}

impl PollGate {
    pub fn is_held(&self, project: Uuid) -> bool {
        self.held.contains(&project)
    }
}

pub async fn poll_gate(
    conn: &mut PgConnection,
    node: Uuid,
    projects: &[Uuid],
    now_ms: u64,
    staging: Option<Staging>,
) -> Result<PollGate, sqlx::Error> {
    let mut gate = PollGate::default();
    for hold in holds(conn, &[node], projects, now_ms, staging).await? {
        match hold.scope {
            Scope::Project(p) => gate.held.push(p),
            _ => gate.stop = gate.stop.or(Some(hold)),
        }
    }
    Ok(gate)
}

/// Project limits. Empty before migration 0103.
pub async fn project_holds(
    conn: &mut PgConnection,
    project: Uuid,
    now_ms: u64,
) -> Result<Vec<Hold>, sqlx::Error> {
    holds(conn, &[], &[project], now_ms, None).await
}

/// Every hold the swarm read shows: each listed node's and the server's holds
/// (once), then each listed project's holds, in one grouped call.
pub async fn swarm_holds(
    conn: &mut PgConnection,
    nodes: &[Uuid],
    projects: &[Uuid],
    now_ms: u64,
    staging: Option<Staging>,
) -> Result<Vec<Hold>, sqlx::Error> {
    holds(conn, nodes, projects, now_ms, staging).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hold(code: Code, kind: &str, scope: Scope, bytes: bool, used: i64, limit: i64) -> Hold {
        Hold {
            code,
            kind: kind.into(),
            scope,
            bytes,
            used,
            limit,
        }
    }

    #[test]
    fn node_offer_rate_message_and_remedy() {
        let h = hold(
            Code::RateLimited,
            "offers",
            Scope::Node(Uuid::nil()),
            false,
            30,
            30,
        );
        assert_eq!(
            h.message(),
            "Offer rate limit reached: 30 offers in the last hour (limit 30)"
        );
        assert_eq!(
            h.remedy(),
            "Wait for the hour to pass; if this repeats, look for a task that fails in a loop"
        );
        assert_eq!(h.code_name(), "history_rate_limited");
    }

    #[test]
    fn inventory_rate_message() {
        let h = hold(
            Code::RateLimited,
            "presence_rate",
            Scope::Node(Uuid::nil()),
            false,
            720,
            720,
        );
        assert_eq!(
            h.message(),
            "Inventory report rate limit reached: 720 reports in the last hour (limit 720)"
        );
    }

    #[test]
    fn project_cap_message() {
        let h = hold(
            Code::CapReached,
            "admission_artifacts",
            Scope::Project(Uuid::from_u128(1)),
            false,
            449,
            512,
        );
        assert_eq!(
            h.message(),
            "Project 00000000-0000-0000-0000-000000000001: live admission artifacts cap reached: 449 of 512"
        );
        assert_eq!(h.remedy(), "Wait for running agents to settle");
        assert_eq!(h.code_name(), "history_cap_reached");
    }

    #[test]
    fn server_bytes_message() {
        let h = hold(
            Code::CapReached,
            "native_preparation",
            Scope::Global,
            true,
            4_194_000,
            4_194_304,
        );
        assert_eq!(
            h.message(),
            "Server: live preparation bytes cap reached: 4194000 of 4194304 bytes"
        );
        let leases = hold(
            Code::CapReached,
            "native_preparation",
            Scope::Node(Uuid::nil()),
            false,
            256,
            256,
        );
        assert_eq!(
            leases.message(),
            "Live material leases cap reached: 256 of 256"
        );
    }

    #[test]
    fn kinds_have_names_and_only_preparation_lacks_a_legacy_query() {
        for kind in [
            Kind::IntelligenceDecisions,
            Kind::SourceExports,
            Kind::V2Proposals,
            Kind::LaunchRegistrations,
        ] {
            assert!(kind.legacy().is_some(), "{kind:?}");
        }
        assert!(Kind::NativePreparation.legacy().is_none());
        assert_eq!(Kind::SourceExports.name(), "source_exports");
    }
}
