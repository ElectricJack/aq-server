use super::*;

/// The earlier run a new run retries, and the supervisor attempts it carries.
#[derive(Clone, Copy, Debug)]
pub struct Lineage {
    pub retry_of: Uuid,
    pub supervisor_attempts: i32,
}

/// The purpose and affected item for which a cause was observed.
pub struct FailureCause<'a> {
    pub purpose: Purpose,
    pub affected: &'a Affected,
    pub cause: &'a str,
}

impl FailureCause<'_> {
    fn request(&self, detail: &str) -> Request {
        Request {
            purpose: self.purpose,
            instructions: format!(
                "Say in one sentence what the owner should do about this: {}",
                detail
            ),
            input: self.cause.into(),
            answer: AnswerShape::Json {
                schema: serde_json::json!({"type": "object"}),
            },
            blocking: false,
            affected: self.affected.clone(),
            deadline_ms: *crate::DEADLINE_MS.start(),
            max_output_bytes: 4096,
            escalate: true,
            profile_override: None,
            agent: None,
        }
    }
}

/// Caller-generated identity or a new run linked to its earlier attempt.
struct RunIdentity {
    id: Uuid,
    lineage: Option<Lineage>,
}

struct Admission<'a> {
    candidates: &'a [Candidate],
    failure: Option<(Reason, &'a str)>,
}

impl<'a> Admission<'a> {
    fn for_candidates(candidates: std::result::Result<&'a [Candidate], &'a str>) -> Self {
        let detail = match candidates {
            Ok([]) => "no candidate is configured for this purpose",
            Ok(candidates) => {
                return Self {
                    candidates,
                    failure: None,
                };
            }
            Err(detail) => detail,
        };
        Self {
            candidates: &[],
            failure: Some((Reason::NoCandidate, detail)),
        }
    }
}

/// Queues `request` for `candidates` in fallback order. With no candidate the
/// task is stored already `failed` with `no_candidate`.
pub async fn submit(
    pool: &PgPool,
    request: &Request,
    candidates: &[Candidate],
    now_ms: i64,
) -> Result<Uuid> {
    Queue::at(pool, now_ms)
        .submit_as(Uuid::new_v4(), request, Ok(candidates))
        .await
}

impl Queue<'_> {
    /// [`submit`] under the caller's `id`, or with no candidate (`Err` gives the
    /// detail) [`Queue::submit_failed`] with `no_candidate`. A task with that id already
    /// stored is left as it is, so a caller that records the id before it submits
    /// can submit again after a restart without making a second task.
    pub async fn submit_as(
        self,
        id: Uuid,
        request: &Request,
        candidates: std::result::Result<&[Candidate], &str>,
    ) -> Result<Uuid> {
        self.insert(
            RunIdentity { id, lineage: None },
            request,
            Admission::for_candidates(candidates),
        )
        .await
    }

    /// Stores `request` already failed with `reason` and `detail`.
    pub async fn submit_failed(self, request: &Request, failure: (Reason, &str)) -> Result<Uuid> {
        self.insert(
            RunIdentity {
                id: Uuid::new_v4(),
                lineage: None,
            },
            request,
            Admission {
                candidates: &[],
                failure: Some(failure),
            },
        )
        .await
    }

    /// [`submit`], or [`Queue::submit_failed`] with `no_candidate` and `failure` as the
    /// detail, for a retry: the link to the earlier run is part of the insert, so
    /// the escalation step never sees the retry unlinked.
    pub async fn submit_retry(
        self,
        request: &Request,
        candidates: std::result::Result<&[Candidate], &str>,
        lineage: Lineage,
    ) -> Result<Uuid> {
        self.insert(
            RunIdentity {
                id: Uuid::new_v4(),
                lineage: Some(lineage),
            },
            request,
            Admission::for_candidates(candidates),
        )
        .await
    }

    /// Stores the task `id`; one already stored under `id` is left as it is.
    async fn insert(
        self,
        run: RunIdentity,
        request: &Request,
        admission: Admission<'_>,
    ) -> Result<Uuid> {
        let Self { pool, now_ms } = self;
        let RunIdentity { id, lineage } = run;
        let Admission {
            candidates,
            failure,
        } = admission;
        request.validate()?;
        let deadline_at_ms = now_ms + request.deadline_ms as i64;
        let failed = failure.is_some();
        sqlx::query(
        "INSERT INTO internal_tasks(id,purpose,request,candidates,status,blocking,deadline_at_ms,
             created_at_ms,finished_at_ms,reason,detail,affected_kind,affected_id,project_id,escalate,
             retry_of,supervisor_attempts,workspace_kind)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18)
         ON CONFLICT(id) DO NOTHING",
    )
    .bind(id)
    .bind(to_text(&request.purpose))
    .bind(to_json(request))
    .bind(to_json(&candidates))
    .bind(if failed { "failed" } else { "queued" })
    .bind(request.blocking)
    .bind(deadline_at_ms)
    .bind(now_ms)
    .bind(failed.then_some(now_ms))
    .bind(failure.map(|(reason, _)| to_text(&reason)))
    .bind(failure.map(|(_, detail)| detail))
    .bind(&request.affected.kind)
    .bind(&request.affected.id)
    .bind(request.affected.project_id)
    .bind(request.escalate)
    .bind(lineage.map(|l| l.retry_of))
    .bind(lineage.map_or(0, |l| l.supervisor_attempts))
    .bind(if request.agent.is_some() { "agent" } else { "none" })
    .execute(pool)
    .await?;
        Ok(id)
    }

    /// Records a failure the server itself found (spec 0011 §4 W6: a task that
    /// cannot be routed), so that it follows the failure path of spec 0012 §8:
    /// a `failed` run with no candidate and `escalate`, which the escalation
    /// step sends to a supervisor and then the owner. Once per `cause` for the
    /// affected item and purpose, whatever became of the earlier one: `None`
    /// when that cause is already on record. `detail` says what happened in the
    /// owner's words; a retry of the run only asks a model to restate it.
    pub async fn record_failure(
        self,
        subject: FailureCause<'_>,
        failure: (Reason, &str),
    ) -> Result<Option<Uuid>> {
        let Self { pool, now_ms } = self;
        let request = subject.request(failure.1);
        let FailureCause {
            purpose,
            affected,
            cause,
        } = subject;
        request.validate()?;
        let mut tx = pool.begin().await?;
        // One writer per item and cause, so two servers record it once.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
            .bind(format!(
                "aq.internal-work.failure/{}/{}/{cause}",
                affected.kind, affected.id
            ))
            .execute(&mut *tx)
            .await?;
        let known: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM internal_tasks WHERE affected_kind=$1 AND affected_id=$2
             AND purpose=$3 AND retry_of IS NULL AND jsonb_array_length(candidates)=0
             AND request->>'input'=$4)",
        )
        .bind(&affected.kind)
        .bind(&affected.id)
        .bind(to_text(&purpose))
        .bind(cause)
        .fetch_one(&mut *tx)
        .await?;
        if known {
            return Ok(None);
        }
        let id = Uuid::new_v4();
        sqlx::query(
        "INSERT INTO internal_tasks(id,purpose,request,candidates,status,blocking,deadline_at_ms,
             created_at_ms,finished_at_ms,reason,detail,affected_kind,affected_id,project_id,escalate)
         VALUES($1,$2,$3,'[]','failed',FALSE,$4,$4,$4,$5,$6,$7,$8,$9,TRUE)",
    )
    .bind(id)
    .bind(to_text(&purpose))
    .bind(to_json(&request))
    .bind(now_ms)
    .bind(to_text(&failure.0))
    .bind(failure.1)
    .bind(&affected.kind)
    .bind(&affected.id)
    .bind(affected.project_id)
    .execute(&mut *tx)
    .await?;
        tx.commit().await?;
        Ok(Some(id))
    }
}

/// Resolves every failure [`Queue::record_failure`] recorded for the affected item
/// under `purpose` that is not resolved yet: its cause is gone (the task got
/// a route). Returns how many.
pub async fn resolve_failures(pool: &PgPool, purpose: Purpose, affected: &Affected) -> Result<u64> {
    let resolved = sqlx::query(
        "UPDATE internal_tasks SET escalation='resolved' WHERE affected_kind=$1 AND affected_id=$2
             AND purpose=$3 AND status='failed' AND escalate AND retry_of IS NULL
             AND jsonb_array_length(candidates)=0 AND escalation<>'resolved'",
    )
    .bind(&affected.kind)
    .bind(&affected.id)
    .bind(to_text(&purpose))
    .execute(pool)
    .await?;
    Ok(resolved.rows_affected())
}
