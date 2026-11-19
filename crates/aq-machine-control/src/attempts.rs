//! A task's attempt counts (moved out of the aq-server lib): what the retry and never-granted
//! bounds are decided from.
use crate::Error;
use sqlx::{PgConnection, Row};
use uuid::Uuid;

#[derive(Debug, Clone, Copy)]
pub struct AttemptCounts {
    pub started: u32,
    pub history: u32,
    /// Offers that expired without ever being granted, since the owner's latest override.
    pub never_granted: u32,
    /// Settings › Selection defaults `max_never_granted_offers`.
    pub max_never_granted: u32,
}
impl AttemptCounts {
    pub fn never_granted_bound_exhausted(&self) -> bool {
        self.never_granted >= self.max_never_granted
    }
    pub fn budget_revision(self) -> u64 {
        u64::from(self.history) + 1
    }
}
/// `$since` is the instant never-granted offers count from; `$fresh` further narrows the
/// started attempts.
macro_rules! counts {
    ($since:literal) => {
        counts!($since, "")
    };
    ($since:literal, $fresh:literal) => {
        concat!("SELECT count(*)::BIGINT AS history,count(*) FILTER(WHERE NOT (NOT a.holds_task AND profiled_never_granted_valid(a.attempt_id))", $fresh, ")::BIGINT AS started,count(c.attempt_id) FILTER(WHERE c.closed_at_ms>", $since, ")::BIGINT AS never_granted FROM execution_attempts a LEFT JOIN profiled_never_granted_closures c ON c.attempt_id=a.attempt_id WHERE a.task_id=$1")
    };
}
const COUNTS: &str = counts!("0");
/// From migration 151: an owner override starts the never-granted count again.
const COUNTS_151: &str = counts!(
    "COALESCE((SELECT max(o.recorded_at_ms) FROM task_limit_overrides o WHERE o.task_id=$1),0)"
);
/// From migration 156: an owner's fresh start (`crate::fresh`) leaves the attempts it released
/// out of the started count and starts the never-granted count again, as an override does.
const COUNTS_156: &str = counts!(
    "GREATEST(COALESCE((SELECT max(o.recorded_at_ms) FROM task_limit_overrides o WHERE o.task_id=$1),0),COALESCE((SELECT max(f.recorded_at_ms) FROM task_fresh_starts f WHERE f.task_id=$1),0))",
    " AND NOT EXISTS(SELECT 1 FROM task_fresh_starts f WHERE f.attempt_id=a.attempt_id)"
);
/// The counts of `task`, with the bound from Settings `max_never_granted_offers`.
pub async fn counts(
    conn: &mut PgConnection,
    task: Uuid,
    max_never_granted: u32,
) -> Result<AttemptCounts, Error> {
    let sql = match crate::holds::has_overrides(conn).await? {
        true if crate::fresh::has_fresh_starts(conn).await? => COUNTS_156,
        true => COUNTS_151,
        false => COUNTS,
    };
    let row = sqlx::query(sql).bind(task).fetch_one(conn).await?;
    let value = |name: &str| {
        u32::try_from(row.get::<i64, _>(name)).map_err(|_| Error::Invalid("attempt history bound"))
    };
    Ok(AttemptCounts {
        started: value("started")?,
        history: value("history")?,
        never_granted: value("never_granted")?,
        max_never_granted,
    })
}
