use super::*;

/// A write to an attempt is fenced by its task ID and candidate index.
#[derive(Clone, Copy)]
pub struct CandidateAttempt {
    pub task: Uuid,
    pub index: u32,
}

impl CandidateAttempt {
    pub fn new(task: Uuid, index: u32) -> Self {
        Self { task, index }
    }
}

impl Queue<'_> {
    /// Passes over candidate `candidate_index` without running it: the attempt
    /// is recorded with [`Reason::LoginBusy`] and `detail`, and the task moves
    /// on to the next candidate. It never fails a task with that reason: when no
    /// later candidate is left, or the deadline passed meanwhile, the task fails
    /// as a queued task past its deadline would. Does nothing when that
    /// candidate no longer holds the task.
    pub async fn pass_candidate(self, key: CandidateAttempt, detail: &str) -> Result<()> {
        let Self { pool, now_ms } = self;
        let CandidateAttempt {
            task: id,
            index: candidate_index,
        } = key;
        let mut tx = pool.begin().await?;
        let Some(row) = held(&mut tx, id, candidate_index).await? else {
            return Ok(());
        };
        let next = candidate_index as usize + 1;
        if next < row.candidates.len() && now_ms < row.deadline_at_ms {
            let attempt = to_json(&attempt(&row, Some(Reason::LoginBusy), detail, now_ms)?);
            sqlx::query(
                "UPDATE internal_tasks SET status='queued', candidate_index=$2, runner=NULL,
                 claimed_at_ms=NULL, attempts=attempts||jsonb_build_array($3::jsonb)
             WHERE id=$1",
            )
            .bind(id)
            .bind(next as i32)
            .bind(attempt)
            .execute(&mut *tx)
            .await?;
        } else {
            sqlx::query(
                "UPDATE internal_tasks SET status='failed', reason=$2, detail=$3, finished_at_ms=$4
             WHERE id=$1",
            )
            .bind(id)
            .bind(to_text(&Reason::DeadlineExceeded))
            .bind(QUEUED_PAST_DEADLINE)
            .bind(now_ms)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Records the answer of candidate `candidate_index`. Does nothing when that
    /// candidate no longer holds the task.
    pub async fn complete(
        self,
        key: CandidateAttempt,
        answer: &Value,
        usage: Option<&Usage>,
    ) -> Result<()> {
        let Self { pool, now_ms } = self;
        let CandidateAttempt {
            task: id,
            index: candidate_index,
        } = key;
        let mut tx = pool.begin().await?;
        let Some(row) = held(&mut tx, id, candidate_index).await? else {
            return Ok(());
        };
        let attempt = attempt(&row, None, "", now_ms)?;
        sqlx::query(
            "UPDATE internal_tasks SET status='answered', answer=$2, usage=$3, finished_at_ms=$4,
             attempts=attempts||jsonb_build_array($5::jsonb)
         WHERE id=$1",
        )
        .bind(id)
        .bind(answer)
        .bind(usage.map(to_json))
        .bind(now_ms)
        .bind(to_json(&attempt))
        .execute(&mut *tx)
        .await?;
        // A retry that answers settles the failure it retried (spec 0012 §8).
        escalation::resolve_chain(&mut tx, id).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Records the failure of candidate `candidate_index`. A provider error,
    /// unavailable provider, invalid output or per-candidate deadline moves the
    /// task to the next candidate while one exists and the task deadline has not
    /// passed; otherwise, and for any other reason, the task fails with `reason`.
    /// Does nothing when that candidate no longer holds the task.
    pub async fn fail_candidate(
        self,
        key: CandidateAttempt,
        reason: Reason,
        detail: &str,
    ) -> Result<()> {
        self.fail_held(key, reason, detail).await?;
        Ok(())
    }

    /// [`Queue::fail_candidate`], returning whether the candidate still held the task.
    async fn fail_held(self, key: CandidateAttempt, reason: Reason, detail: &str) -> Result<bool> {
        let Self { pool, now_ms } = self;
        let CandidateAttempt {
            task: id,
            index: candidate_index,
        } = key;
        let mut tx = pool.begin().await?;
        let Some(row) = held(&mut tx, id, candidate_index).await? else {
            return Ok(false);
        };
        let attempt = to_json(&attempt(&row, Some(reason), detail, now_ms)?);
        let retryable = matches!(
            reason,
            Reason::ProviderError
                | Reason::ProviderUnavailable
                | Reason::InvalidOutput
                | Reason::DeadlineExceeded
        );
        let next = candidate_index as usize + 1;
        let fallback_available = next < row.candidates.len() && now_ms < row.deadline_at_ms;
        if retryable && fallback_available {
            sqlx::query(
                "UPDATE internal_tasks SET status='queued', candidate_index=$2, runner=NULL,
                 claimed_at_ms=NULL, attempts=attempts||jsonb_build_array($3::jsonb)
             WHERE id=$1",
            )
            .bind(id)
            .bind(next as i32)
            .bind(attempt)
            .execute(&mut *tx)
            .await?;
        } else {
            sqlx::query(
            "UPDATE internal_tasks SET status='failed', reason=$2, detail=$3, finished_at_ms=$4,
                 attempts=attempts||jsonb_build_array($5::jsonb)
             WHERE id=$1",
        )
        .bind(id)
        .bind(to_text(&reason))
        .bind(detail)
        .bind(now_ms)
        .bind(attempt)
        .execute(&mut *tx)
        .await?;
        }
        tx.commit().await?;
        Ok(true)
    }
}

/// [`candidate_budget_ms`] of a row's current attempt, in SQL over its
/// columns, or [`agent_candidate_budget_ms`] for agent work; `$2` is
/// [`MIN_CANDIDATE_BUDGET_MS`] and `$4` [`AGENT_FALLBACK_RESERVE_MS`].
/// Integer division truncates toward zero in both. NULL without a
/// `claimed_at_ms`, which only a claim sets; the sweep then falls back to the
/// task deadline.
const BUDGET_SQL: &str = "GREATEST(0, LEAST(deadline_at_ms-claimed_at_ms, CASE
    WHEN workspace_kind='agent' THEN GREATEST($4, deadline_at_ms-claimed_at_ms
        - $4*GREATEST(jsonb_array_length(candidates)-candidate_index-1, 0))
    ELSE GREATEST($2, (deadline_at_ms-claimed_at_ms)
        / GREATEST(jsonb_array_length(candidates)-candidate_index, 1)) END))";

/// Ends tasks whose deadline or runner is gone and returns how many it
/// changed. A `running` task whose runner has not reported within its
/// candidate budget ([`candidate_budget_ms`]) plus [`SWEEP_GRACE_MS`] fails
/// that candidate as `deadline_exceeded` with "runner did not report": a
/// live runner stops and reports by the end of the budget, so only a runner
/// that died gets here. The task moves on to the next
/// candidate, never back to `queued` on the same one, so a late runner stays
/// fenced out. A `queued` task past its deadline fails `deadline_exceeded`.
pub async fn sweep(pool: &PgPool, now_ms: i64) -> Result<u64> {
    let stale: Vec<(Uuid, i32)> = sqlx::query_as(&format!(
        "SELECT id,candidate_index FROM internal_tasks
         WHERE status='running' AND COALESCE(claimed_at_ms+{BUDGET_SQL}, deadline_at_ms)+$3 < $1"
    ))
    .bind(now_ms)
    .bind(MIN_CANDIDATE_BUDGET_MS)
    .bind(SWEEP_GRACE_MS)
    .bind(AGENT_FALLBACK_RESERVE_MS)
    .fetch_all(pool)
    .await?;
    let mut changed = 0;
    for (id, index) in stale {
        let index = u32::try_from(index).map_err(|_| Error::Corrupt)?;
        let reason = Reason::DeadlineExceeded;
        if Queue::at(pool, now_ms)
            .fail_held(
                CandidateAttempt::new(id, index),
                reason,
                "runner did not report",
            )
            .await?
        {
            changed += 1;
        }
    }
    let expired = sqlx::query(
        "UPDATE internal_tasks SET status='failed', reason=$2, detail=$3, finished_at_ms=$1
         WHERE status='queued' AND deadline_at_ms <= $1",
    )
    .bind(now_ms)
    .bind(to_text(&Reason::DeadlineExceeded))
    .bind(QUEUED_PAST_DEADLINE)
    .execute(pool)
    .await?;
    Ok(changed + expired.rows_affected())
}

/// The locked row, when candidate `candidate_index` is still running it.
async fn held(tx: &mut sqlx::PgConnection, id: Uuid, candidate_index: u32) -> Result<Option<Row>> {
    sqlx::query(
        "SELECT * FROM internal_tasks WHERE id=$1 AND candidate_index=$2 AND status='running'
         FOR UPDATE",
    )
    .bind(id)
    .bind(candidate_index as i32)
    .fetch_optional(tx)
    .await?
    .as_ref()
    .map(Row::decode)
    .transpose()
}

fn attempt(row: &Row, reason: Option<Reason>, detail: &str, now_ms: i64) -> Result<Attempt> {
    let candidate = row
        .candidates
        .get(row.candidate_index as usize)
        .ok_or(Error::Corrupt)?;
    Ok(Attempt {
        provider: candidate.provider.clone(),
        model: candidate.model.clone(),
        way: candidate.way,
        reason,
        detail: detail.into(),
        elapsed_ms: elapsed(row.claimed_at_ms.unwrap_or(now_ms), now_ms),
    })
}
