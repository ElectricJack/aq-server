use super::*;

/// The claim loop: claims while fewer than [`MAX_RUNS`] runs are in
/// progress and runs each claimed task in its own task; looks again when a
/// run ends or after the poll interval.
///
/// At most one Codex run holds the Codex login at a time, decided when
/// claiming: while one does, only Claude tasks are claimed (Claude's token is
/// never refreshed, so its runs can share it). The Codex run keeps the login
/// until it has ended, its refresh copied back included, so the next Codex
/// run, even the same task's fallback, copies in the newest login.
///
/// At most one agent run is in progress at a time, so one of the
/// [`MAX_RUNS`] slots always serves answer tasks: while one runs, only answer
/// tasks are claimed. A Codex agent run holds the Codex login like any Codex
/// run, for its whole duration.
///
/// Once `keep_going` returns false, or an error, it claims nothing more and
/// returns that when the runs in progress have ended.
pub async fn serve(
    config: std::sync::Arc<InternalConfig>,
    pool: PgPool,
    runner: String,
    mut keep_going: impl FnMut() -> Result<bool>,
) -> Result<()> {
    let permits = Permits::new();
    let mut runs = tokio::task::JoinSet::new();
    loop {
        let claimed = permits
            .start(
                Host {
                    config: &config,
                    pool: &pool,
                    runner: &runner,
                },
                &mut runs,
            )
            .await?;
        let keep = keep_going();
        if !matches!(keep, Ok(true)) {
            while let Some(ended) = runs.join_next().await {
                report_end(ended);
            }
            return keep.map(drop);
        }
        if !claimed {
            tokio::select! {
                () = tokio::time::sleep(Duration::from_millis(config.poll_ms)) => {}
                Some(ended) = runs.join_next(), if !runs.is_empty() => report_end(ended),
            }
        }
        while let Some(ended) = runs.try_join_next() {
            report_end(ended);
        }
    }
}

struct Permits {
    codex: std::sync::Arc<tokio::sync::Semaphore>,
    agent: std::sync::Arc<tokio::sync::Semaphore>,
}

struct Host<'a> {
    config: &'a std::sync::Arc<InternalConfig>,
    pool: &'a PgPool,
    runner: &'a str,
}

impl Permits {
    fn new() -> Self {
        let slot = || std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        Self {
            codex: slot(),
            agent: slot(),
        }
    }

    async fn start(
        &self,
        host: Host<'_>,
        runs: &mut tokio::task::JoinSet<Result<()>>,
    ) -> Result<bool> {
        let Host {
            config,
            pool,
            runner,
        } = host;
        let codex = self.codex.clone().try_acquire_owned().ok();
        if codex.is_none() && config.codex.is_some() {
            pass_busy_codex(pool, runner).await;
        }
        if runs.len() >= MAX_RUNS {
            return Ok(false);
        }
        let agent = self.agent.clone().try_acquire_owned().ok();
        let free = (codex.is_some(), agent.is_some());
        let host = ClaimHost {
            config,
            pool,
            runner,
        };
        let job = match host.claim(now_ms()?, free).await {
            Ok(Some(job)) => job,
            Ok(None) => return Ok(false),
            Err(_) => {
                eprintln!("internal work: claim failed");
                return Ok(false);
            }
        };
        let login = codex.filter(|_| job.claimed.candidate.harness.as_deref() == Some("codex"));
        let agent = agent.filter(|_| job.claimed.request.agent.is_some());
        let (config, pool) = (config.clone(), pool.clone());
        runs.spawn(async move {
            let ended = finish(job, &config, &pool).await;
            drop((login, agent));
            ended
        });
        Ok(true)
    }
}

/// The detail of a Codex candidate passed over while the login is busy.
const LOGIN_BUSY: &str = "codex login busy";

/// While the Codex login is busy: passes over the Codex candidate of each
/// queued task that cannot wait for it, so that its next candidate runs. A
/// task cannot wait when it has a later candidate and is blocking or has
/// less than twice [`MIN_CANDIDATE_BUDGET_MS`] left. A task whose last
/// candidate is Codex waits: passing it over would only fail it.
async fn pass_busy_codex(pool: &PgPool, runner: &str) {
    loop {
        let Ok(now) = now_ms() else { return };
        let short = 2 * MIN_CANDIDATE_BUDGET_MS;
        let passable = match store::Queue::at(pool, now)
            .claim_passable("codex", runner, short)
            .await
        {
            Ok(Some(passable)) => passable,
            Ok(None) => return,
            Err(_) => {
                eprintln!("internal work: claim failed");
                return;
            }
        };
        let (id, index) = (passable.id, passable.candidate_index);
        if store::Queue::at(pool, now)
            .pass_candidate(store::CandidateAttempt::new(id, index), LOGIN_BUSY)
            .await
            .is_err()
        {
            eprintln!("internal work: passing over a busy login failed");
            return;
        }
    }
}

/// How long recording a run's result may take. Past it the host logs, gives
/// the record up (the sweep moves the task on) and frees the login and the
/// slot the run held.
const RECORD_TIMEOUT: Duration = Duration::from_secs(30);

async fn record(write: impl Future<Output = aq_internal_work::Result<()>>) -> Result<()> {
    match tokio::time::timeout(RECORD_TIMEOUT, write).await {
        Ok(written) => Ok(written?),
        Err(_) => {
            eprintln!("internal work: recording a run timed out");
            Ok(())
        }
    }
}

fn report_end(ended: std::result::Result<Result<()>, tokio::task::JoinError>) {
    if !matches!(ended, Ok(Ok(()))) {
        eprintln!("internal work: run failed");
    }
}

/// A login task this host claimed at `now_ms`, [`Instant`] `started`.
struct Job {
    claimed: Claimed,
    now_ms: i64,
    started: Instant,
}

/// Claims at most one queued login task for a harness this host has
/// configured, runs it, and records the answer or the candidate's failure.
/// Returns whether it claimed; it returns when the run has ended.
///
/// `now_ms` is the claim time. The run, its preparation included, is stopped
/// at [`Claimed::stop_at_ms`], counted from the claim exactly as the sweep
/// counts: the native's session gets SIGTERM and the failure is recorded at
/// once, while the run gets [`TERM_GRACE_MS`] to end before SIGKILL and then
/// reaps the native, copies a refreshed login back and removes its
/// directories.
pub async fn tick(
    config: &InternalConfig,
    pool: &PgPool,
    runner: &str,
    now_ms: i64,
) -> Result<bool> {
    let host = ClaimHost {
        config,
        pool,
        runner,
    };
    let Some(job) = host.claim(now_ms, (true, true)).await? else {
        return Ok(false);
    };
    finish(job, config, pool).await?;
    Ok(true)
}

struct ClaimHost<'a> {
    config: &'a InternalConfig,
    pool: &'a PgPool,
    runner: &'a str,
}

impl ClaimHost<'_> {
    /// Claims the next queued login task for a harness this host has
    /// configured; for Codex only when `codex` (its login is free), and agent
    /// work only when `agents` (no agent runs here now).
    async fn claim(self, now_ms: i64, (codex, agents): (bool, bool)) -> Result<Option<Job>> {
        let Self {
            config,
            pool,
            runner,
        } = self;
        let codex = config.codex.as_ref().filter(|_| codex);
        let configured: Vec<&str> = [("codex", codex), ("claude", config.claude.as_ref())]
            .into_iter()
            .filter_map(|(name, harness)| harness.map(|_| name))
            .collect();
        if configured.is_empty() {
            return Ok(None);
        }
        // At `now_ms`: the claim records it as `claimed_at_ms`.
        let started = Instant::now();
        let claimed = store::Queue::at(pool, now_ms)
            .claim_login(&configured, agents, runner)
            .await?;
        Ok(claimed.map(|claimed| Job {
            claimed,
            now_ms,
            started,
        }))
    }
}

/// Runs a claimed task and records the answer or the candidate's failure.
async fn finish(job: Job, config: &InternalConfig, pool: &PgPool) -> Result<()> {
    let claimed = &job.claimed;
    let stop_after = u64::try_from(claimed.stop_at_ms() - job.now_ms).unwrap_or(0);
    let stop_at = job.started + Duration::from_millis(stop_after);
    // Every process of the agent's own uid dies with an agent run.
    let agent = claimed.request.agent.is_some() && config.identity.drop_privileges;
    let session = Session::new(stop_at, agent.then_some(AGENT_UID));
    let transcript = TranscriptSlot::default();
    let run = run_candidate(config, claimed, &session, &transcript);
    tokio::pin!(run);
    let ran = tokio::select! {
        biased;
        result = &mut run => Some(result),
        () = tokio::time::sleep_until(stop_at.into()) => None,
    };
    let Some(result) = ran else {
        session.terminate();
        let harness = claimed.candidate.harness.as_deref().unwrap_or("native");
        let detail = claimed.unanswered(harness);
        let reason = Reason::DeadlineExceeded;
        let report = record(job.queue(pool).fail_candidate(job.key(), reason, &detail));
        // The run's outcome is not used; it still has to reap the native,
        // copy a refreshed login back and remove its directories.
        let end = async {
            let grace = Duration::from_millis(TERM_GRACE_MS);
            if tokio::time::timeout(grace, &mut run).await.is_err() {
                session.kill();
                let _ = run.await;
            }
        };
        // The report is polled first and does not wait for the run to end.
        let (reported, ()) = tokio::join!(report, end);
        keep_transcript(pool, claimed, &transcript).await;
        return reported;
    };
    keep_transcript(pool, claimed, &transcript).await;
    job.report(pool, result).await
}

impl Job {
    fn key(&self) -> store::CandidateAttempt {
        store::CandidateAttempt::new(self.claimed.id, self.claimed.candidate_index)
    }
    fn queue<'a>(&self, pool: &'a PgPool) -> store::Queue<'a> {
        let elapsed = i64::try_from(self.started.elapsed().as_millis()).unwrap_or(i64::MAX);
        store::Queue::at(pool, self.now_ms.saturating_add(elapsed))
    }
    async fn report(
        &self,
        pool: &PgPool,
        result: Result<(String, Option<Usage>), Failure>,
    ) -> Result<()> {
        let request = &self.claimed.request;
        let checked = result.and_then(|(raw, usage)| {
            validate::answer(&request.answer, &raw, request.max_output_bytes)
                .map(|answer| (answer, usage))
                .map_err(|detail| (Reason::InvalidOutput, detail))
        });
        let queue = self.queue(pool);
        match checked {
            Ok((answer, usage)) => {
                record(queue.complete(self.key(), &answer, usage.as_ref())).await
            }
            Err((reason, detail)) => {
                record(queue.fail_candidate(self.key(), reason, &detail)).await
            }
        }
    }
}

/// Stores the transcript an agent run left, once the run has ended (also
/// after a timeout, when its native was killed).
async fn keep_transcript(pool: &PgPool, claimed: &Claimed, transcript: &TranscriptSlot) {
    let events = transcript
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    let Some(events) = events else { return };
    let (id, index) = (claimed.id, claimed.candidate_index);
    if record(store::save_transcript(pool, id, index, &events))
        .await
        .is_err()
    {
        eprintln!("internal work: a transcript was not recorded");
    }
}
