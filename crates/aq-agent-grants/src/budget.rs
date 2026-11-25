//! Per-attempt command budgets (the `AgentCommand` admission class, §8):
//! concurrent requests, requests per minute and mutations per minute. A call
//! over budget is refused before it is checked or forwarded, as `overloaded`
//! and retryable.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub concurrent: u32,
    pub per_minute: u32,
    pub mutations_per_minute: u32,
}
pub const DEFAULT: Limits = Limits {
    concurrent: 8,
    per_minute: 240,
    mutations_per_minute: 60,
};
#[derive(Debug, Default)]
struct Window {
    active: u32,
    minute: u64,
    requests: u32,
    mutations: u32,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exceeded {
    Concurrent,
    Rate,
    MutationRate,
}
/// Bounded: attempts idle for a minute are forgotten when the map is large.
#[derive(Debug)]
pub struct Budgets {
    limits: Limits,
    windows: Arc<Mutex<HashMap<Uuid, Window>>>,
}
const MAX_TRACKED: usize = 4096;
/// Held while a call runs; releases its concurrency slot on drop.
#[derive(Debug)]
pub struct Permit {
    attempt: Uuid,
    windows: Arc<Mutex<HashMap<Uuid, Window>>>,
}
impl Drop for Permit {
    fn drop(&mut self) {
        let mut w = self.windows.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(w) = w.get_mut(&self.attempt) {
            w.active = w.active.saturating_sub(1);
        }
    }
}
/// The process-wide budgets at [`DEFAULT`] limits.
pub fn shared() -> &'static Budgets {
    static SHARED: std::sync::OnceLock<Budgets> = std::sync::OnceLock::new();
    SHARED.get_or_init(|| Budgets::new(DEFAULT))
}
impl Budgets {
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            windows: Default::default(),
        }
    }
    pub fn acquire(&self, attempt: Uuid, mutation: bool, now_ms: u64) -> Result<Permit, Exceeded> {
        let minute = now_ms / 60_000;
        let mut map = self.windows.lock().unwrap_or_else(|p| p.into_inner());
        if map.len() >= MAX_TRACKED && !map.contains_key(&attempt) {
            map.retain(|_, w| w.active > 0 || w.minute == minute);
        }
        let w = map.entry(attempt).or_default();
        w.acquire(self.limits, mutation, minute)?;
        Ok(Permit {
            attempt,
            windows: self.windows.clone(),
        })
    }
}

impl Window {
    fn acquire(&mut self, limits: Limits, mutation: bool, minute: u64) -> Result<(), Exceeded> {
        if self.minute != minute {
            (self.minute, self.requests, self.mutations) = (minute, 0, 0);
        }
        if self.active >= limits.concurrent {
            return Err(Exceeded::Concurrent);
        }
        if self.requests >= limits.per_minute {
            return Err(Exceeded::Rate);
        }
        if mutation && self.mutations >= limits.mutations_per_minute {
            return Err(Exceeded::MutationRate);
        }
        self.active += 1;
        self.requests += 1;
        self.mutations += u32::from(mutation);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn budgets_are_per_attempt_and_per_minute() {
        let b = Budgets::new(Limits {
            concurrent: 2,
            per_minute: 3,
            mutations_per_minute: 1,
        });
        let (a, other) = (Uuid::from_u128(1), Uuid::from_u128(2));
        let p1 = b.acquire(a, true, 0).unwrap();
        assert_eq!(b.acquire(a, true, 1).unwrap_err(), Exceeded::MutationRate);
        let p2 = b.acquire(a, false, 2).unwrap();
        assert_eq!(b.acquire(a, false, 3).unwrap_err(), Exceeded::Concurrent);
        let _o = b.acquire(other, true, 3).unwrap();
        drop(p1);
        let p3 = b.acquire(a, false, 4).unwrap();
        drop((p2, p3));
        assert_eq!(
            b.acquire(a, false, 5).unwrap_err(),
            Exceeded::Rate,
            "three per minute"
        );
        let _next = b.acquire(a, true, 60_000).unwrap();
    }
}
