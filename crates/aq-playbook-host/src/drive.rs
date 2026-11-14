//! The automatic drive and the engine open never stop for good on an error:
//! a failure is reported and retried after a bounded backoff, and only a stop
//! request ends them.
use std::{future::Future, time::Duration};
use tokio::sync::watch;

/// A delay that starts at `first`, doubles with each consecutive failure and
/// never exceeds `max` (at least `first`).
#[derive(Clone, Debug)]
pub struct Backoff {
    first: Duration,
    max: Duration,
    failures: u32,
}
impl Backoff {
    pub fn new(first: Duration, max: Duration) -> Self {
        Self {
            first,
            max: max.max(first),
            failures: 0,
        }
    }
    /// Records a failure and returns the delay before the next attempt.
    pub fn fail(&mut self) -> Duration {
        let delay = self
            .first
            .saturating_mul(1 << self.failures.min(20))
            .min(self.max);
        self.failures = self.failures.saturating_add(1);
        delay
    }
    pub fn reset(&mut self) {
        self.failures = 0;
    }
}

/// Why a startup step refused: the step and its cause, as text. Callers keep
/// secrets (connection strings, keys) out of both.
#[derive(Debug)]
pub struct Refused(pub String);
impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "playbook owner startup refused: {}", self.0)
    }
}
impl std::error::Error for Refused {}
/// Names the startup step a failure belongs to: `result.step("what")?`.
pub trait Step<T> {
    fn step(self, what: &str) -> Result<T, Refused>;
}
impl<T, E: std::fmt::Display> Step<T> for Result<T, E> {
    fn step(self, what: &str) -> Result<T, Refused> {
        self.map_err(|cause| Refused(format!("{what}: {cause}")))
    }
}

/// Whether an owner's answer to `Stop` proves the engine released its
/// namespace. A closed channel is not release evidence: only an exact
/// successful Stop response, remembered in `known` for this owner, permits a
/// later closed (`Stopped`) answer to count as the same release.
pub fn stop_acknowledged(
    known: &std::sync::atomic::AtomicBool,
    response: Result<serde_json::Value, crate::Error>,
) -> bool {
    let ordering = std::sync::atomic::Ordering::SeqCst;
    match response {
        Ok(_) => {
            known.store(true, ordering);
            true
        }
        Err(crate::Error::Stopped) => known.load(ordering),
        Err(_) => false,
    }
}

/// Waits `delay`; `false` when a stop was requested first.
async fn pause(stop: &mut watch::Receiver<bool>, delay: Duration) -> bool {
    tokio::select! {
        biased;
        _ = stop.wait_for(|stop| *stop) => false,
        _ = tokio::time::sleep(delay) => true,
    }
}

/// Runs `quantum` every `interval` until a stop is requested. A failed
/// quantum is passed to `failed` with the delay before the next attempt
/// (a [`Backoff`] from `interval` to `max_backoff`); a success resets it.
pub async fn run<E, Q, F>(
    mut stop: watch::Receiver<bool>,
    interval: Duration,
    max_backoff: Duration,
    mut quantum: impl FnMut() -> Q,
    mut failed: F,
) where
    Q: Future<Output = Result<(), E>>,
    F: FnMut(&E, Duration),
{
    let mut backoff = Backoff::new(interval, max_backoff);
    loop {
        let delay = match quantum().await {
            Ok(()) => {
                backoff.reset();
                interval
            }
            Err(error) => {
                let delay = backoff.fail();
                failed(&error, delay);
                delay
            }
        };
        if *stop.borrow() || !pause(&mut stop, delay).await {
            return;
        }
    }
}

/// Calls `attempt` until it succeeds, waiting a [`Backoff`] between failures
/// (each passed to `failed`). `None` when a stop was requested first.
pub async fn retry<T, E, A, F>(
    mut stop: watch::Receiver<bool>,
    mut backoff: Backoff,
    mut attempt: impl FnMut() -> A,
    mut failed: F,
) -> Option<T>
where
    A: Future<Output = Result<T, E>>,
    F: FnMut(&E, Duration),
{
    loop {
        if *stop.borrow() {
            return None;
        }
        match attempt().await {
            Ok(value) => return Some(value),
            Err(error) => {
                let delay = backoff.fail();
                failed(&error, delay);
                if !pause(&mut stop, delay).await {
                    return None;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    #[test]
    fn closed_queue_is_unknown_until_exact_stop_ack_for_this_owner() {
        let known = std::sync::atomic::AtomicBool::new(false);
        assert!(!stop_acknowledged(&known, Err(crate::Error::Stopped)));
        assert!(stop_acknowledged(&known, Ok(serde_json::Value::Null)));
        assert!(stop_acknowledged(&known, Err(crate::Error::Stopped)));
        assert!(!stop_acknowledged(&known, Err(crate::Error::Unknown)));
    }

    #[test]
    fn a_refused_step_names_the_step_and_its_cause() {
        let failed: Result<(), &str> = Err("relation missing");
        let refused = failed.step("reading the schema version").unwrap_err();
        assert_eq!(
            refused.to_string(),
            "playbook owner startup refused: reading the schema version: relation missing"
        );
    }

    #[test]
    fn backoff_doubles_from_first_and_stops_at_max() {
        let mut b = Backoff::new(Duration::from_millis(100), Duration::from_millis(500));
        let delays: Vec<_> = (0..5).map(|_| b.fail().as_millis()).collect();
        assert_eq!(delays, [100, 200, 400, 500, 500]);
        b.reset();
        assert_eq!(b.fail(), Duration::from_millis(100));
        let mut tiny = Backoff::new(Duration::from_millis(100), Duration::ZERO);
        assert_eq!(tiny.fail(), Duration::from_millis(100));
    }

    #[tokio::test(start_paused = true)]
    async fn a_failing_quantum_is_retried_with_backoff_and_the_drive_recovers() {
        let (tx, rx) = watch::channel(false);
        let calls = Arc::new(AtomicUsize::new(0));
        let delays = Arc::new(Mutex::new(vec![]));
        let (c, d) = (calls.clone(), delays.clone());
        let drive = tokio::spawn(run(
            rx,
            Duration::from_millis(10),
            Duration::from_millis(40),
            move || {
                let n = c.fetch_add(1, Ordering::SeqCst);
                // Three transient storage failures, then the drive goes on.
                async move {
                    if (1..=3).contains(&n) {
                        Err("storage")
                    } else {
                        Ok(())
                    }
                }
            },
            move |_: &&str, delay| d.lock().unwrap().push(delay.as_millis()),
        ));
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(*delays.lock().unwrap(), [10, 20, 40]);
        // Still driving after the failures: interval 10 ms over ~430 ms.
        assert!(calls.load(Ordering::SeqCst) > 20);
        tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(1), drive)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn retry_returns_the_first_success_and_stop_ends_it() {
        let (_tx, rx) = watch::channel(false);
        let mut n = 0;
        let value = retry(
            rx,
            Backoff::new(Duration::from_millis(5), Duration::from_millis(20)),
            || {
                n += 1;
                let n = n;
                async move { if n < 4 { Err(n) } else { Ok(n) } }
            },
            |_, _| {},
        )
        .await;
        assert_eq!(value, Some(4));
        let (tx, rx) = watch::channel(false);
        let pending = tokio::spawn(retry(
            rx,
            Backoff::new(Duration::from_millis(5), Duration::from_millis(20)),
            || async { Err::<(), _>(()) },
            |_, _| {},
        ));
        tokio::time::sleep(Duration::from_millis(50)).await;
        tx.send(true).unwrap();
        assert_eq!(pending.await.unwrap(), None);
    }
}
