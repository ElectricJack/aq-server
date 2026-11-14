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
