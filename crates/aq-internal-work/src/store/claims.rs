use super::*;

struct ClaimPolicy<'a> {
    way: Way,
    harnesses: &'a [&'a str],
    agents: bool,
}

impl Queue<'_> {
    /// [`Queue::claim`] for a login host, which takes an agent task only when `agents`
    /// (it runs no agent now), so that one of its slots always serves answer
    /// tasks.
    pub async fn claim_login(
        self,
        harnesses: &[&str],
        agents: bool,
        runner: &str,
    ) -> Result<Option<Claimed>> {
        self.claim_next(
            ClaimPolicy {
                way: Way::Login,
                harnesses,
                agents,
            },
            runner,
        )
        .await
    }

    /// Takes the next queued task whose current candidate is served `way` (for
    /// [`Way::Login`], by one of `harnesses`): blocking tasks first, then oldest.
    ///
    /// A Codex agent run holds the Codex login for up to 30 minutes, so an agent
    /// task whose current candidate is Codex is taken only while no blocking
    /// answer task ends on Codex: none is queued, or running on an earlier
    /// candidate it may fail over from, with Codex as its last candidate.
    pub async fn claim(
        self,
        way: Way,
        harnesses: &[&str],
        runner: &str,
    ) -> Result<Option<Claimed>> {
        self.claim_next(
            ClaimPolicy {
                way,
                harnesses,
                agents: true,
            },
            runner,
        )
        .await
    }

    async fn claim_next(self, policy: ClaimPolicy<'_>, runner: &str) -> Result<Option<Claimed>> {
        let Self { pool, now_ms } = self;
        let ClaimPolicy {
            way,
            harnesses,
            agents,
        } = policy;
        let row = sqlx::query(
            "WITH next AS (
             SELECT id FROM internal_tasks a
             WHERE status='queued'
               AND candidates->candidate_index->>'way'=$1
               AND ($1<>'login' OR candidates->candidate_index->>'harness'=ANY($2))
               AND ($5 OR workspace_kind<>'agent')
               AND NOT (workspace_kind='agent'
                   AND candidates->candidate_index->>'harness'='codex'
                   AND EXISTS(SELECT 1 FROM internal_tasks b
                       WHERE b.blocking AND b.workspace_kind<>'agent' AND b.id<>a.id
                         AND (b.status='queued' OR b.status='running'
                              AND b.candidate_index+1 < jsonb_array_length(b.candidates))
                         AND b.candidates->-1->>'way'='login'
                         AND b.candidates->-1->>'harness'='codex'))
             ORDER BY blocking DESC, created_at_ms, id
             LIMIT 1
             FOR UPDATE SKIP LOCKED)
         UPDATE internal_tasks t SET status='running', runner=$3, claimed_at_ms=$4
         FROM next WHERE t.id=next.id
         RETURNING t.*",
        )
        .bind(to_text(&way))
        .bind(harnesses)
        .bind(runner)
        .bind(now_ms)
        .bind(agents)
        .fetch_optional(pool)
        .await?;
        row.as_ref()
            .map(Row::decode)
            .transpose()?
            .map(claimed)
            .transpose()
    }

    /// Takes the next queued task whose current candidate is served by the
    /// login `harness` and may be passed over while that login is busy: a later
    /// candidate exists, the deadline has not passed, and the task is blocking
    /// or has less than `short_ms` left. Blocking tasks first, then oldest. The
    /// caller hands it to [`pass_candidate`].
    pub async fn claim_passable(
        self,
        harness: &str,
        runner: &str,
        short_ms: i64,
    ) -> Result<Option<Claimed>> {
        let Self { pool, now_ms } = self;
        let row = sqlx::query(
            "WITH next AS (
             SELECT id FROM internal_tasks
             WHERE status='queued'
               AND candidates->candidate_index->>'way'='login'
               AND candidates->candidate_index->>'harness'=$1
               AND candidate_index+1 < jsonb_array_length(candidates)
               AND deadline_at_ms > $3
               AND (blocking OR deadline_at_ms-$3 < $4)
             ORDER BY blocking DESC, created_at_ms, id
             LIMIT 1
             FOR UPDATE SKIP LOCKED)
         UPDATE internal_tasks t SET status='running', runner=$2, claimed_at_ms=$3
         FROM next WHERE t.id=next.id
         RETURNING t.*",
        )
        .bind(harness)
        .bind(runner)
        .bind(now_ms)
        .bind(short_ms)
        .fetch_optional(pool)
        .await?;
        row.as_ref()
            .map(Row::decode)
            .transpose()?
            .map(claimed)
            .transpose()
    }
}

/// The [`Claimed`] of a row a claim has just taken.
fn claimed(row: Row) -> Result<Claimed> {
    let candidate = row
        .candidates
        .get(row.candidate_index as usize)
        .cloned()
        .ok_or(Error::Corrupt)?;
    Ok(Claimed {
        id: row.id,
        request: row.request,
        candidate,
        candidate_index: row.candidate_index,
        deadline_at_ms: row.deadline_at_ms,
        claimed_at_ms: row.claimed_at_ms.ok_or(Error::Corrupt)?,
        candidate_count: row.candidates.len(),
    })
}
