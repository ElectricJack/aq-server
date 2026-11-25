//! Accounting uses only a verified worker stop and the report bound to it.
use sqlx::PgConnection;
use uuid::Uuid;

pub const REASON: &str = "worker_platform_interrupted";

/// SQL form of `aq_admission::transport::ResultReport::platform_interrupted`.
/// Pass the qualified report byte column at the query site.
#[macro_export]
macro_rules! platform_interruption_sql {
    ($report:literal) => {
        concat!(
            "(convert_from(",
            $report,
            ",'UTF8')::jsonb @> ",
            "'{\"outcome\":\"failed\",\"stop\":{\"reason\":\"guardian_failure\"}}'::jsonb)"
        )
    };
}

/// The exact matching result of this task carries a verified platform stop.
pub async fn report_interrupted(
    conn: &mut PgConnection,
    task: Uuid,
    report: Uuid,
) -> sqlx::Result<bool> {
    sqlx::query_scalar(concat!(
        "SELECT EXISTS(SELECT 1 FROM profiled_result_reports r ",
        "JOIN execution_attempts a USING(attempt_id) WHERE r.report_id=$1 AND a.task_id=$2 AND ",
        crate::platform_interruption_sql!("r.report_bytes"),
        ")"
    ))
    .bind(report)
    .bind(task)
    .fetch_one(conn)
    .await
}
