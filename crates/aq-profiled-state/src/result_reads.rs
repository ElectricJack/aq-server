//! The SQL and query validation behind the owner and project result reads
//! (`human_results`): bounded keyset discovery of recorded result reports.
//! Reads only; each function runs inside the caller's read transaction.
use aq_admission::{
    LaunchBinding, SessionIdentity,
    execution_spec::ProfiledExecutionSpec,
    result_record::{self, Receipt},
    transport::ResultReport,
};
use aq_session_protocol::{LaunchManifest, ProfiledOffer, Validate};
use sqlx::{PgConnection, Row, postgres::PgRow};
use uuid::Uuid;

/// A page holds 1 to 25 reports (default 10).
pub const DEFAULT_LIMIT: u32 = 10;

/// Whether a result read's filters are well formed: a limit of 1 to 25, no nil
/// project or cursor, and an exact revision that is 64 lowercase hex digits.
pub fn query_is_valid(
    limit: u32,
    project: Option<Uuid>,
    after: Option<Uuid>,
    exact: Option<(Uuid, &str)>,
) -> bool {
    (1..=25).contains(&limit)
        && !project.is_some_and(|v| v.is_nil())
        && !after.is_some_and(|v| v.is_nil())
        && !exact.is_some_and(|(id, revision)| {
            id.is_nil()
                || revision.len() != 64
                || !revision
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
}

/// Bounds the transaction's lock and statement waits.
pub async fn bound_waits(conn: &mut PgConnection) -> sqlx::Result<()> {
    sqlx::query("SET LOCAL lock_timeout='1s'")
        .execute(&mut *conn)
        .await?;
    sqlx::query("SET LOCAL statement_timeout='2s'")
        .execute(conn)
        .await?;
    Ok(())
}

/// Whether `actor` is an owner, holding the user's SHARE lock.
pub async fn is_owner(conn: &mut PgConnection, actor: Uuid) -> sqlx::Result<bool> {
    let role: Option<String> = sqlx::query_scalar("SELECT role FROM users WHERE id=$1 FOR SHARE")
        .bind(actor)
        .fetch_optional(conn)
        .await?;
    Ok(role.as_deref() == Some("owner"))
}

pub struct ReportPage {
    pub with_v2: bool,
    pub project: Option<Uuid>,
    pub after: Option<Uuid>,
    pub limit: i64,
}

/// Report ids after `after` in UUID order (not chronological), at most
/// `limit` of them, in `project` when given. Reports of a v2-accepted attempt
/// are included only with `with_v2`.
pub async fn page_ids(conn: &mut PgConnection, page: ReportPage) -> sqlx::Result<Vec<Uuid>> {
    let ReportPage {
        with_v2,
        project,
        after,
        limit,
    } = page;
    sqlx::query_scalar("SELECT r.report_id FROM profiled_result_reports r JOIN profiled_launches l USING(attempt_id) WHERE ($4 OR NOT EXISTS(SELECT 1 FROM profiled_v2_accepts a WHERE a.attempt_id=r.attempt_id)) AND ($1::uuid IS NULL OR (l.execution_spec#>>'{lifecycle,project_id}')=$1::text) AND ($2::uuid IS NULL OR r.report_id>$2) ORDER BY r.report_id LIMIT $3")
        .bind(project).bind(after).bind(limit).bind(with_v2)
        .fetch_all(conn).await
}

/// Whether the report belongs to `project`'s attempt.
pub async fn in_project(
    conn: &mut PgConnection,
    report: Uuid,
    project: Uuid,
) -> sqlx::Result<bool> {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM profiled_result_reports r JOIN profiled_launches l USING(attempt_id) WHERE r.report_id=$1 AND (l.execution_spec#>>'{lifecycle,project_id}')=$2::text)")
        .bind(report).bind(project).fetch_one(conn).await
}

/// Opens the savepoint a listed report is read in (when `isolate`), so one
/// refused validation can be undone without losing the page.
pub async fn savepoint(conn: &mut PgConnection, isolate: bool) -> sqlx::Result<()> {
    if isolate {
        sqlx::query("SAVEPOINT result_item").execute(conn).await?;
    }
    Ok(())
}
/// Keeps the savepoint's reads when `loaded`, and otherwise rolls them back
/// (when `isolate`).
pub async fn settle(conn: &mut PgConnection, isolate: bool, loaded: bool) -> sqlx::Result<()> {
    if isolate {
        let end = if loaded { "RELEASE" } else { "ROLLBACK TO" };
        sqlx::query(&format!("{end} SAVEPOINT result_item"))
            .execute(conn)
            .await?;
    }
    Ok(())
}

fn invalid(what: &str) -> sqlx::Error {
    sqlx::Error::Decode(format!("recorded result is inconsistent: {what}").into())
}
fn bad<E>(what: &'static str) -> impl FnOnce(E) -> sqlx::Error {
    move |_| invalid(what)
}
/// The attempt's frozen launch binding and execution spec, which must be the
/// `report_binding` of the report, agree with its frozen offer and manifest
/// and digests, and belong to `project` and `task`. Frozen offer metadata only,
/// never current authority. An inconsistent record is a decode error.
pub async fn read_launch(
    conn: &mut PgConnection,
    report_binding: &LaunchBinding,
    (project, task): (Uuid, Uuid),
) -> sqlx::Result<(LaunchBinding, ProfiledExecutionSpec)> {
    let attempt = report_binding.attempt_id;
    let launch = sqlx::query("SELECT launch_binding,launch_manifest,manifest_digest,execution_spec,execution_spec_digest FROM profiled_launches WHERE attempt_id=$1 FOR SHARE")
        .bind(attempt).fetch_one(&mut *conn).await?;
    let json = |column: &str| launch.get::<serde_json::Value, _>(column);
    let binding: LaunchBinding =
        serde_json::from_value(json("launch_binding")).map_err(bad("binding"))?;
    let manifest: LaunchManifest =
        serde_json::from_value(json("launch_manifest")).map_err(bad("manifest"))?;
    let spec_bytes = canonical(&json("execution_spec")).map_err(bad("spec"))?;
    let spec =
        aq_admission::execution_spec::decode_profiled_spec(&spec_bytes).map_err(bad("spec"))?;
    let offer: ProfiledOffer = serde_json::from_value(
        sqlx::query_scalar::<_, serde_json::Value>(
            "SELECT state->'offer' FROM execution_attempts WHERE attempt_id=$1",
        )
        .bind(attempt)
        .fetch_one(conn)
        .await?,
    )
    .map_err(bad("offer"))?;
    offer.validate().map_err(bad("offer"))?;
    spec.validate_against(&offer).map_err(bad("spec"))?;
    let agrees = binding == *report_binding
        && offer.binding == binding
        && offer.manifest == manifest
        && launch.get::<String, _>("manifest_digest") == binding.launch_manifest_digest.as_str()
        && launch.get::<String, _>("execution_spec_digest")
            == binding.execution_spec_digest.as_str()
        && spec.lifecycle.project_id == project
        && spec.task.task_id == task;
    if !agrees {
        return Err(invalid("binding, offer, digests or scope"));
    }
    Ok((binding, spec))
}

/// The canonical bytes of a record: object keys sorted, as recorded.
pub fn canonical(value: &impl serde::Serialize) -> serde_json::Result<Vec<u8>> {
    let mut value = serde_json::to_value(value)?;
    value.sort_all_objects();
    serde_json::to_vec(&value)
}
/// One stored result report, decoded and checked against its own row.
pub struct Stored {
    pub row: PgRow,
    pub report: ResultReport,
    pub receipt: Receipt,
    pub node_id: Uuid,
}
/// The report `report_id` as stored: its bytes must be the canonical encoding
/// of the report, which must agree with the row's digest, attempt, node and
/// operation, and the receipt must be the report's canonical receipt. `None`
/// when there is no such report; an inconsistent record is a decode error.
pub async fn read_report(conn: &mut PgConnection, report_id: Uuid) -> sqlx::Result<Option<Stored>> {
    let Some(row) = sqlx::query("SELECT * FROM profiled_result_reports WHERE report_id=$1")
        .bind(report_id)
        .fetch_optional(conn)
        .await?
    else {
        return Ok(None);
    };
    Stored::decode(row, report_id).map(Some)
}
impl Stored {
    fn decode(row: PgRow, report_id: Uuid) -> sqlx::Result<Self> {
        let bytes: Vec<u8> = row.get("report_bytes");
        let report = result_record::decode_report(&bytes).map_err(bad("report"))?;
        let node_id = validate_report_row(&row, &report, report_id, &bytes)?;
        let receipt_bytes: Vec<u8> = row.get("receipt_bytes");
        let receipt =
            result_record::decode_receipt(&receipt_bytes, &report).map_err(bad("receipt"))?;
        if canonical(&receipt).map_err(bad("receipt"))? != receipt_bytes {
            return Err(invalid("receipt"));
        }
        Ok(Self {
            row,
            report,
            receipt,
            node_id,
        })
    }
}
fn validate_report_row(
    row: &PgRow,
    report: &ResultReport,
    report_id: Uuid,
    bytes: &[u8],
) -> sqlx::Result<Uuid> {
    let digest = result_record::report_digest(report).map_err(bad("report"))?;
    let SessionIdentity::Remote {
        node_id, boot_id, ..
    } = report.binding.session
    else {
        return Err(invalid("session"));
    };
    let bytes_match = canonical(report).map_err(bad("report"))? == bytes
        && digest.as_str() == row.get::<String, _>("report_digest");
    if !bytes_match {
        return Err(invalid("report row"));
    }
    let identity_matches = report.identity.node_id == node_id && report.identity.boot_id == boot_id;
    if !(report_matches_row(report, row, report_id, node_id) && identity_matches) {
        return Err(invalid("report row"));
    }
    Ok(node_id)
}

fn report_matches_row(report: &ResultReport, row: &PgRow, report_id: Uuid, node_id: Uuid) -> bool {
    report.report_id == report_id
        && report.binding.attempt_id == row.get::<Uuid, _>("attempt_id")
        && node_id == row.get::<Uuid, _>("node_id")
        && report.operation_id == row.get::<Uuid, _>("operation_id")
}
/// The attempt's current head report, holding its SHARE lock.
pub async fn head_report(conn: &mut PgConnection, attempt: Uuid) -> sqlx::Result<Option<Uuid>> {
    sqlx::query_scalar("SELECT report_id FROM profiled_result_heads WHERE attempt_id=$1 FOR SHARE")
        .bind(attempt)
        .fetch_optional(conn)
        .await
}

/// What the policy actor could record for B4 after a final rework.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct ReplacementFacts {
    /// The attempt's hot state now records `retention = retained`.
    pub source_retained: bool,
    /// The attempt's hot state now records `effects = quiescent`.
    pub effects_quiescent: bool,
    /// This call wrote at least one of the two facts.
    pub changed: bool,
}

/// Serializes settlement, result facts and automatic retry on one report.
pub async fn lock_settlement(conn: &mut PgConnection, report: Uuid) -> sqlx::Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,56))")
        .bind(format!("aq-auto-settlement:{report}"))
        .execute(conn)
        .await?;
    Ok(())
}

pub async fn lock_hot_state(
    conn: &mut PgConnection,
    attempt: Uuid,
) -> sqlx::Result<crate::hot::HotState> {
    let state = sqlx::query_scalar(
        "SELECT state FROM profiled_authority_hot WHERE attempt_id=$1 FOR UPDATE",
    )
    .bind(attempt)
    .fetch_one(conn)
    .await?;
    serde_json::from_value(state).map_err(|e| sqlx::Error::Decode(e.into()))
}

pub async fn save_hot_state(
    conn: &mut PgConnection,
    attempt: Uuid,
    state: &crate::hot::HotState,
) -> sqlx::Result<()> {
    let state = serde_json::to_value(state).map_err(|e| sqlx::Error::Encode(e.into()))?;
    sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
        .bind(attempt)
        .bind(state)
        .execute(conn)
        .await?;
    Ok(())
}

pub async fn owner_settled(conn: &mut PgConnection, attempt: Uuid) -> sqlx::Result<bool> {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM profiled_settlement_evaluations WHERE attempt_id=$1 AND disposition IN ('accepted','needs_reconciliation','stale')) OR EXISTS(SELECT 1 FROM profiled_task_completions WHERE attempt_id=$1)")
        .bind(attempt).fetch_one(conn).await
}

/// Reworked attempts or verified platform interruptions awaiting settlement,
/// in report order after `after`. The cursor carries no authority.
pub async fn reworked_without_facts(
    pool: &sqlx::PgPool,
    after: Option<Uuid>,
    limit: i64,
) -> sqlx::Result<Vec<Uuid>> {
    sqlx::query_scalar(concat!(
        "SELECT r.report_id FROM profiled_result_heads rh \
         JOIN profiled_result_reports r ON (r.attempt_id,r.report_id)=(rh.attempt_id,rh.report_id) \
         JOIN execution_attempts a ON a.attempt_id=r.attempt_id \
         JOIN profiled_authority_hot h ON h.attempt_id=a.attempt_id \
         LEFT JOIN profiled_auto_settlements s ON s.report_id=r.report_id \
         WHERE a.kind='profiled_v1' AND a.holds_task \
         AND h.state#>>'{stop,kind}' IN ('stopped','not_started') \
         AND ((s.disposition='rework_required' AND
               (h.state#>>'{retention,kind}' IS DISTINCT FROM 'retained' OR h.state#>>'{effects,kind}' IS DISTINCT FROM 'quiescent')) \
              OR ((s.disposition IS NULL OR s.disposition='held') AND ",
         aq_machine_control::platform_interruption_sql!("r.report_bytes"),
        " AND h.state->>'revoked'='false' AND h.state->>'cancellation_id' IS NULL)) \
          AND ($1::UUID IS NULL OR r.report_id>$1) ORDER BY r.report_id LIMIT $2",
    ))
    .bind(after)
    .bind(limit)
    .fetch_all(pool)
    .await
}
