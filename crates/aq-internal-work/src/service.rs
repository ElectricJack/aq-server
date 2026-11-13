//! The internal work service (spec 0012 §3, §4, §8 step 1): turning a request
//! into a task, the in-server loop that runs API candidates, and waiting for a
//! task to finish. The server builds one [`Service`] from its pool, clock,
//! registry and `[internal_work]` config.
//!
//! Internal work is outside swarm capacity: nothing here reads the swarm
//! settings, the provider mask or the availability switch.
use crate::*;
use aq_intelligence_config::LoadedRegistry;
use aq_selection::AvailabilityObservation;
use sqlx::PgPool;
use std::{sync::Arc, time::Duration};

/// The current time in Unix milliseconds.
pub type Now = Arc<dyn Fn() -> i64 + Send + Sync>;

pub struct Service {
    pool: PgPool,
    now: Now,
    api: ApiBackend,
    registry: Option<Arc<LoadedRegistry>>,
    poll: Duration,
    enabled: bool,
    /// Who claims tasks: this server process.
    runner: String,
}

impl Service {
    /// No `[internal_work]` section means no API endpoints, the default poll
    /// interval, and the loop on.
    pub fn new(
        pool: PgPool,
        now: Now,
        config: Option<&InternalWorkConfig>,
        registry: Option<Arc<LoadedRegistry>>,
    ) -> Result<Self> {
        let config = config.cloned().unwrap_or_else(|| {
            serde_json::from_value(serde_json::json!({})).expect("every field has a default")
        });
        Ok(Self {
            pool,
            now,
            api: ApiBackend::new(config.api)?,
            registry,
            poll: Duration::from_millis(config.poll_ms),
            enabled: config.enabled,
            runner: format!("server/{}", Uuid::new_v4()),
        })
    }

    /// Settings (or the request's override) pick the profile; the profile
    /// resolves to candidates; the task is queued, or stored `no_candidate`.
    pub async fn submit(&self, request: &Request) -> Result<Uuid> {
        request.validate()?;
        let (pool, now_ms) = (&self.pool, (self.now)());
        let profile = match &request.profile_override {
            Some(id) => Some(id.clone()),
            None => settings::profile_for(pool, request.purpose).await?,
        };
        let candidates = match profile {
            Some(profile) => self.candidates(&profile, now_ms).await?,
            None => Err("no internal work profile configured".into()),
        };
        match candidates {
            Ok(candidates) => store::submit(pool, request, &candidates, now_ms).await,
            Err(detail) => {
                store::submit_failed(pool, request, Reason::NoCandidate, &detail, now_ms).await
            }
        }
    }

    /// The candidates of `profile`, or why there are none.
    async fn candidates(
        &self,
        profile: &str,
        now_ms: i64,
    ) -> Result<std::result::Result<Vec<Candidate>, String>> {
        let Some(registry) = self.registry.as_deref() else {
            return Ok(Err("intelligence registry is not configured".into()));
        };
        let (_, catalog) = settings::active_catalog(&mut *self.pool.acquire().await?).await?;
        let Some(catalog) = catalog else {
            return Ok(Err("no profile catalog is active".into()));
        };
        let observations: Vec<serde_json::Value> = sqlx::query_scalar(
            "SELECT observation FROM intelligence_observations ORDER BY scope_source_digest LIMIT 1024",
        )
        .fetch_all(&self.pool)
        .await?;
        let observations = observations
            .into_iter()
            .map(serde_json::from_value)
            .collect::<std::result::Result<Vec<AvailabilityObservation>, _>>()
            .map_err(|_| Error::Corrupt)?;
        let now = u64::try_from(now_ms).unwrap_or(0);
        Ok(resolve(&catalog, registry, &observations, profile, now)
            .map(|resolved| resolved.candidates)
            .map_err(|_| format!("intelligence profile {profile:?} has no usable candidate")))
    }

    /// Reads the task every poll interval until it has an answer or a failure.
    /// Past the deadline plus [`SWEEP_GRACE_MS`] it sweeps itself, so a
    /// stopped, disabled or failing loop cannot leave the caller waiting.
    pub async fn wait(&self, id: Uuid) -> Result<Outcome> {
        let mut swept = false;
        loop {
            let row = store::get(&self.pool, id)
                .await?
                .ok_or_else(|| Error::NotFound("Internal task not found".into()))?;
            if matches!(row.status.as_str(), "answered" | "failed") {
                return row.outcome().ok_or(Error::Corrupt);
            }
            // Re-read straight after a sweep; sleep before sweeping again.
            swept = !swept && (self.now)() > row.deadline_at_ms + SWEEP_GRACE_MS;
            if swept {
                store::sweep(&self.pool, (self.now)()).await?;
            } else {
                tokio::time::sleep(self.poll).await;
            }
        }
    }

    pub async fn run(&self, request: &Request) -> Result<Outcome> {
        let id = self.submit(request).await?;
        self.wait(id).await
    }

    /// Sweeps, then runs one queued API task end to end: the provider call,
    /// the answer check, then the answer or the candidate's failure. False
    /// when no API task was queued, or with `enabled = false`, which only
    /// stops claiming.
    pub async fn tick(&self) -> Result<bool> {
        let (pool, now) = (&self.pool, &self.now);
        store::sweep(pool, now()).await?;
        if !self.enabled {
            return Ok(false);
        }
        let Some(claimed) = store::claim(pool, Way::Api, &[], &self.runner, now()).await? else {
            return Ok(false);
        };
        // ApiBackend stops at the task deadline; the candidate budget is shorter.
        let budget = (claimed.deadline_at_ms - now()).clamp(0, CANDIDATE_TIMEOUT_MS);
        let sent =
            tokio::time::timeout(Duration::from_millis(budget as u64), self.api.run(&claimed));
        let result = sent.await.unwrap_or_else(|_| {
            let detail = "provider did not answer within the candidate budget";
            Err((Reason::DeadlineExceeded, detail.into()))
        });
        let request = &claimed.request;
        let checked = result.and_then(|(raw, usage)| {
            validate::answer(&request.answer, &raw, request.max_output_bytes)
                .map(|answer| (answer, usage))
                .map_err(|detail| (Reason::InvalidOutput, detail))
        });
        let (id, index) = (claimed.id, claimed.candidate_index);
        match checked {
            Ok((answer, usage)) => {
                store::complete(pool, id, index, &answer, usage.as_ref(), now()).await
            }
            Err((reason, detail)) => {
                store::fail_candidate(pool, id, index, reason, &detail, now()).await
            }
        }?;
        Ok(true)
    }

    /// The in-server loop: ticks until no API task is queued, then sleeps the
    /// poll interval, until `stop` turns true or its sender is gone. It runs
    /// regardless of swarm availability, and with `enabled = false` it still
    /// sweeps but claims nothing.
    pub async fn serve(&self, mut stop: tokio::sync::watch::Receiver<bool>) {
        while !*stop.borrow() {
            loop {
                match self.tick().await {
                    Ok(true) => continue,
                    Ok(false) => break,
                    Err(error) => {
                        tracing::warn!(?error, "internal work tick failed");
                        break;
                    }
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(self.poll) => {}
                changed = stop.changed() => if changed.is_err() { return },
            }
        }
    }
}
