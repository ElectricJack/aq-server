//! PostgreSQL connection budget and pressure watch for the local services.
//!
//! One PostgreSQL instance serves AQ Server, every supervisor host, Graph and
//! the maintenance tools, and idle pooled sessions count against its
//! `max_connections`. This module holds the documented per-process maxima, the
//! headroom check against the instance's real ceiling, and a pure pressure
//! watch that tells a sustained shortage from a spike. The control and permit
//! pools keep their reserved capacity: the budget counts them, and the
//! sampler reads them through that reserved pool.
use serde::Serialize;
use sqlx::PgPool;
use std::sync::Mutex;
use std::time::Duration;

/// Idle sessions of a secondary pool are closed after this long, so a pool
/// that is used in bursts does not hold its maximum between them.
pub const SECONDARY_IDLE: Duration = Duration::from_secs(20);
/// Connections one supervisor host may hold: its service pool, the lease
/// connection of `run` and the dedicated advisory-lock connection.
pub const SUPERVISOR_SERVICE_POOL: u32 = 4;
pub const SUPERVISOR_CONTROLLER_POOL: u32 = 3;
pub const SUPERVISOR_EXTRA: u32 = 2;
pub const SUPERVISORS: u32 = 7;
/// Graph's observed peak (about nine) and the internal-work host (two runs and one).
pub const GRAPH: u32 = 9;
pub const INTERNAL_WORK: u32 = 3;
/// Left for psql, backups, migrations and the owner. Never planned for.
pub const MAINTENANCE_RESERVE: u32 = 8;
/// The sampler's own connection comes from the reserved control pool.
pub const WARN_PERCENT: u64 = 80;
pub const CRITICAL_PERCENT: u64 = 95;
/// How long critical use must last before it is escalated, and how often an
/// escalation repeats.
pub const SUSTAIN: Duration = Duration::from_secs(30);
pub const REPEAT: Duration = Duration::from_secs(300);
pub const SAMPLE_EVERY: Duration = Duration::from_secs(5);

/// One consumer of connections and the most it may hold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Consumer {
    pub name: String,
    pub max: u32,
}

/// Maxima of the AQ Server pools, from the configured launch limits. The
/// fixed secondary pools are listed in `docs/operations.md`.
pub fn server_consumers(limits: &crate::limits::LaunchLimits) -> Vec<Consumer> {
    let c = |name: &str, max: u32| Consumer {
        name: name.into(),
        max,
    };
    vec![
        c("server general", limits.connections as u32),
        c(
            "server control (reserved)",
            limits.control_connections as u32,
        ),
        c(
            "server renewal (reserved)",
            limits.renewal_connections as u32,
        ),
        c(
            "server presence (reserved)",
            limits.presence_connections as u32,
        ),
        c("server notification", 4),
        c("server observation", 4),
        c("server terminal", 8),
        c("server dashboard", 5),
        c("server graph read, messaging, conversation", 8),
        c("server playbook bridge", 2),
        c("server scheduling diagnostics", 1),
    ]
}

/// Every local consumer in the worst case.
pub fn plan(limits: &crate::limits::LaunchLimits) -> Vec<Consumer> {
    let mut all = server_consumers(limits);
    let per_host = SUPERVISOR_SERVICE_POOL.max(SUPERVISOR_CONTROLLER_POOL) + SUPERVISOR_EXTRA;
    all.push(Consumer {
        name: format!("{SUPERVISORS} supervisor hosts"),
        max: SUPERVISORS * per_host,
    });
    all.push(Consumer {
        name: "graph".into(),
        max: GRAPH,
    });
    all.push(Consumer {
        name: "internal work host".into(),
        max: INTERNAL_WORK,
    });
    all
}

/// The planned worst case against an instance's usable connections.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Headroom {
    pub planned: u32,
    pub usable: u32,
    pub fits: bool,
}

/// `ceiling` is `max_connections` less `superuser_reserved_connections`.
pub fn headroom(consumers: &[Consumer], ceiling: u32) -> Headroom {
    let planned = consumers.iter().map(|c| c.max).sum();
    let usable = ceiling.saturating_sub(MAINTENANCE_RESERVE);
    Headroom {
        planned,
        usable,
        fits: planned <= usable,
    }
}

/// How hard the instance is pressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Ok,
    Warning,
    Critical,
}

pub fn level(used: u64, ceiling: u64) -> Level {
    let ceiling = ceiling.max(1);
    if used * 100 >= ceiling * CRITICAL_PERCENT {
        Level::Critical
    } else if used * 100 >= ceiling * WARN_PERCENT {
        Level::Warning
    } else {
        Level::Ok
    }
}

/// One reading of `pg_stat_activity`. `by_source` is sorted largest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Sample {
    pub used: u64,
    pub ceiling: u64,
    pub by_source: Vec<(String, u64)>,
}

impl Sample {
    pub fn level(&self) -> Level {
        level(self.used, self.ceiling)
    }
    /// The biggest holders, for a log line or an alert.
    pub fn top(&self, n: usize) -> String {
        let mut text = String::new();
        for (name, count) in self.by_source.iter().take(n) {
            if !text.is_empty() {
                text.push_str(", ");
            }
            let name = if name.is_empty() { "(unnamed)" } else { name };
            text.push_str(&format!("{name}={count}"));
        }
        text
    }
}

/// Counts the instance's sessions by `application_name`.
pub async fn sample(pool: &PgPool) -> Result<Sample, sqlx::Error> {
    let ceiling: i64 = sqlx::query_scalar(
        "SELECT current_setting('max_connections')::bigint - current_setting('superuser_reserved_connections')::bigint",
    )
    .fetch_one(pool)
    .await?;
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT application_name, count(*) FROM pg_stat_activity WHERE backend_type='client backend' GROUP BY 1 ORDER BY 2 DESC, 1",
    )
    .fetch_all(pool)
    .await?;
    let by_source: Vec<(String, u64)> = rows.into_iter().map(|(n, c)| (n, c as u64)).collect();
    Ok(Sample {
        used: by_source.iter().map(|(_, c)| c).sum(),
        ceiling: ceiling.max(1) as u64,
        by_source,
    })
}

/// What a reading asks of the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Nothing,
    /// Crossed the warning mark.
    Warn,
    /// Critical for [`SUSTAIN`]; repeats every [`REPEAT`] while it lasts.
    Escalate,
    /// Back below the warning mark after a warning or escalation.
    Recovered,
}

/// Tells a spike from a sustained shortage. A failed sample (no connection
/// could be had) counts as critical: that is exhaustion seen from inside.
#[derive(Debug, Default)]
pub struct Watch {
    level: Option<Level>,
    critical_since: Option<Duration>,
    escalated_at: Option<Duration>,
}

impl Watch {
    /// `now` is any monotonic offset; `reading` is `None` when sampling failed.
    pub fn observe(&mut self, now: Duration, reading: Option<Level>) -> Action {
        let new = reading.unwrap_or(Level::Critical);
        let old = self.level.replace(new).unwrap_or(Level::Ok);
        if new == Level::Critical {
            let since = *self.critical_since.get_or_insert(now);
            if now.saturating_sub(since) >= SUSTAIN
                && self
                    .escalated_at
                    .is_none_or(|at| now.saturating_sub(at) >= REPEAT)
            {
                self.escalated_at = Some(now);
                return Action::Escalate;
            }
        } else {
            self.critical_since = None;
        }
        match (old, new) {
            (Level::Ok, Level::Warning | Level::Critical) => Action::Warn,
            (Level::Warning | Level::Critical, Level::Ok) => {
                self.escalated_at = None;
                Action::Recovered
            }
            _ => Action::Nothing,
        }
    }
}

/// The latest published status, for the loopback readiness surface. It needs no
/// database, so it answers when admission cannot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Status {
    pub level: Level,
    pub used: Option<u64>,
    pub ceiling: Option<u64>,
    pub top: String,
}

static LATEST: Mutex<Option<Status>> = Mutex::new(None);

pub fn publish(status: Status) {
    *LATEST.lock().unwrap_or_else(|e| e.into_inner()) = Some(status);
}

pub fn latest() -> Option<Status> {
    LATEST.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Samples forever on `pool` (the reserved control pool), logging the source of
/// pressure, publishing [`latest`] and escalating sustained exhaustion at error
/// level, which the process log, the host's log shipping and the owner's
/// `aq-server` health probe all see without a database. Returns when `stop` fires.
pub async fn watch(pool: PgPool, mut stop: tokio::sync::watch::Receiver<bool>) {
    let start = tokio::time::Instant::now();
    let mut watch = Watch::default();
    let mut checked = false;
    let mut tick = tokio::time::interval(SAMPLE_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = stop.wait_for(|s| *s) => return,
        }
        let reading = tokio::time::timeout(Duration::from_secs(3), sample(&pool))
            .await
            .ok()
            .and_then(Result::ok);
        if let (false, Some(s)) = (checked, &reading) {
            checked = true;
            let plan = plan(&crate::limits::current());
            let h = headroom(&plan, s.ceiling as u32);
            if h.fits {
                tracing::info!(
                    planned = h.planned,
                    usable = h.usable,
                    "PostgreSQL connection budget fits"
                );
            } else {
                tracing::warn!(
                    planned = h.planned,
                    usable = h.usable,
                    plan = ?plan.iter().map(|c| (c.name.as_str(), c.max)).collect::<Vec<_>>(),
                    "the worst-case local connection plan exceeds PostgreSQL's usable connections; raise max_connections or lower the pool limits"
                );
            }
        }
        let action = watch.observe(start.elapsed(), reading.as_ref().map(Sample::level));
        publish(match &reading {
            Some(s) => Status {
                level: s.level(),
                used: Some(s.used),
                ceiling: Some(s.ceiling),
                top: s.top(5),
            },
            None => Status {
                level: Level::Critical,
                used: None,
                ceiling: None,
                top: "no connection for the sampler".into(),
            },
        });
        let (used, ceiling, top) = match &reading {
            Some(s) => (s.used as i64, s.ceiling as i64, s.top(5)),
            None => (-1, -1, "the sampler itself could not connect".into()),
        };
        match action {
            Action::Nothing => {}
            Action::Warn => tracing::warn!(used, ceiling, %top, "PostgreSQL connections are high"),
            Action::Escalate => tracing::error!(
                used,
                ceiling,
                %top,
                "PostgreSQL connections stay exhausted: the supervisor or owner must act"
            ),
            Action::Recovered => tracing::info!(used, ceiling, "PostgreSQL connections recovered"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::LaunchLimits;

    fn s(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn default_plan_fits_the_documented_ceiling() {
        let plan = plan(&LaunchLimits::DEFAULT);
        let total: u32 = plan.iter().map(|c| c.max).sum();
        let h = headroom(&plan, 150);
        assert_eq!((h.planned, h.usable), (total, 142));
        assert!(h.fits, "{plan:?}");
        // The stock ceiling does not hold the worst case: the server says so at start.
        assert!(!headroom(&plan, 100).fits);
    }

    #[test]
    fn control_and_permit_pools_keep_their_reserved_capacity() {
        let plan = server_consumers(&LaunchLimits::DEFAULT);
        for name in ["server control (reserved)", "server renewal (reserved)"] {
            assert!(plan.iter().any(|c| c.name == name && c.max >= 1), "{name}");
        }
    }

    #[test]
    fn levels_follow_the_marks() {
        assert_eq!(level(79, 100), Level::Ok);
        assert_eq!(level(80, 100), Level::Warning);
        assert_eq!(level(95, 100), Level::Critical);
    }

    #[test]
    fn a_spike_warns_but_only_sustained_exhaustion_escalates() {
        let mut w = Watch::default();
        assert_eq!(w.observe(s(0), Some(Level::Ok)), Action::Nothing);
        assert_eq!(w.observe(s(5), Some(Level::Critical)), Action::Warn);
        assert_eq!(w.observe(s(10), Some(Level::Ok)), Action::Recovered);
        assert_eq!(w.observe(s(15), Some(Level::Critical)), Action::Warn);
        assert_eq!(w.observe(s(40), Some(Level::Critical)), Action::Nothing);
        assert_eq!(w.observe(s(45), Some(Level::Critical)), Action::Escalate);
        // Repeats only after REPEAT, and a failed sample counts as critical.
        assert_eq!(w.observe(s(100), None), Action::Nothing);
        assert_eq!(w.observe(s(350), None), Action::Escalate);
        assert_eq!(w.observe(s(355), Some(Level::Warning)), Action::Nothing);
        assert_eq!(w.observe(s(360), Some(Level::Ok)), Action::Recovered);
    }

    #[test]
    fn the_top_holders_name_the_source() {
        let sample = Sample {
            used: 9,
            ceiling: 10,
            by_source: vec![("aq-terminal".into(), 5), (String::new(), 4)],
        };
        assert_eq!(sample.level(), Level::Warning);
        assert_eq!(sample.top(2), "aq-terminal=5, (unnamed)=4");
    }

    #[test]
    fn the_latest_status_is_published() {
        publish(Status {
            level: Level::Warning,
            used: Some(1),
            ceiling: Some(2),
            top: "x=1".into(),
        });
        assert_eq!(latest().unwrap().level, Level::Warning);
    }
}
