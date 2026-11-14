//! Bounded human read admission. One dashboard page legitimately issues 3 to 6
//! reads at once, so a burst waits briefly in FIFO order instead of failing
//! (QA ISSUE-001, WD-07). More than `running + queued` concurrent requests, or a
//! wait longer than [`QUEUE_WAIT`], is still refused immediately.
use std::time::Duration;
use tokio::sync::{Semaphore, SemaphorePermit};
pub const QUEUE_WAIT: Duration = Duration::from_secs(2);
pub struct ReadGate {
    admitted: Semaphore,
    running: Semaphore,
}
impl ReadGate {
    pub const fn new(running: usize, queued: usize) -> Self {
        Self {
            admitted: Semaphore::const_new(running + queued),
            running: Semaphore::const_new(running),
        }
    }
    /// Both permits are held until the request finishes.
    pub async fn enter(&self) -> Option<(SemaphorePermit<'_>, SemaphorePermit<'_>)> {
        let admitted = self.admitted.try_acquire().ok()?;
        let running = tokio::time::timeout(QUEUE_WAIT, self.running.acquire())
            .await
            .ok()?
            .ok()?;
        Some((admitted, running))
    }
}
