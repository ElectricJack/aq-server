use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use serde::Serialize;
use sqlx::PgPool;
use tokio::sync::mpsc;
use uuid::Uuid;

const CAPACITY: usize = 256;
const MINUTE: i64 = 60_000;
pub(crate) const WEEK: i64 = 7 * 24 * 60 * MINUTE;

#[derive(Clone)]
pub(crate) struct Capture {
    sender: mpsc::Sender<Observation>,
    dropped: Arc<AtomicU64>,
}

#[derive(Serialize)]
#[serde(untagged)]
pub(crate) enum Observation {
    Decision {
        operation_id: Uuid,
        task_id: Option<Uuid>,
        reason: &'static str,
        observed_at_ms: i64,
    },
    Claim {
        attempt_id: Uuid,
        task_id: Uuid,
        generation: u64,
        eligible_at_ms: i64,
        claimed_at_ms: i64,
    },
}

impl Capture {
    pub(crate) fn channel(capacity: usize) -> (Self, mpsc::Receiver<Observation>) {
        let (sender, receiver) = mpsc::channel(capacity);
        let capture = Self {
            sender,
            dropped: Arc::new(AtomicU64::new(0)),
        };
        (capture, receiver)
    }

    fn offer(&self, observation: Observation) {
        if self.sender.try_send(observation).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn decision(
        &self,
        operation_id: Uuid,
        task_id: Option<Uuid>,
        reason: &'static str,
        observed_at_ms: i64,
    ) {
        self.offer(Observation::Decision {
            operation_id,
            task_id,
            reason,
            observed_at_ms,
        });
    }

    pub(crate) fn claim(
        &self,
        attempt_id: Uuid,
        task_id: Uuid,
        generation: u64,
        eligible_at_ms: i64,
        claimed_at_ms: i64,
    ) {
        if claimed_at_ms >= eligible_at_ms {
            self.offer(Observation::Claim {
                attempt_id,
                task_id,
                generation,
                eligible_at_ms,
                claimed_at_ms,
            });
        }
    }
}

pub(crate) fn start(pool: &PgPool) -> Option<Capture> {
    if std::env::var_os("AQ_PERFORMANCE_SCHEDULING_DIAGNOSTICS").is_some_and(|v| v == "0") {
        return None;
    }
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(1))
        .idle_timeout(aq_machine_control::connections::SECONDARY_IDLE)
        .connect_lazy_with(pool.connect_options().as_ref().clone());
    let (capture, receiver) = Capture::channel(CAPACITY);
    tokio::spawn(flush(pool, receiver, capture.dropped.clone()));
    Some(capture)
}

async fn flush(pool: PgPool, mut receiver: mpsc::Receiver<Observation>, dropped: Arc<AtomicU64>) {
    let mut batch = Vec::with_capacity(CAPACITY);
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        while batch.len() < CAPACITY {
            match receiver.try_recv() {
                Ok(observation) => batch.push(observation),
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) if batch.is_empty() => return,
                Err(mpsc::error::TryRecvError::Disconnected) => break,
            }
        }
        let losses = dropped.swap(0, Ordering::Relaxed);
        let now = chrono::Utc::now().timestamp_millis();
        match write(&pool, &batch, losses, now).await {
            Ok(()) => {}
            Err(error) => {
                dropped.fetch_add(losses + batch.len() as u64, Ordering::Relaxed);
                tracing::debug!(%error, "scheduling diagnostic batch dropped");
            }
        }
        batch.clear();
    }
}

async fn write(pool: &PgPool, batch: &[Observation], losses: u64, now: i64) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL statement_timeout='750ms';")
        .execute(&mut *tx)
        .await?;
    let first = (now - WEEK).div_euclid(MINUTE) * MINUTE;
    let minute = now.div_euclid(MINUTE) * MINUTE;
    sqlx::query(include_str!("scheduling_diagnostics/write.sql"))
        .bind(sqlx::types::Json(batch))
        .bind(first)
        .bind(minute)
        .bind(i64::try_from(losses)?)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_or_closed_channel_drops_without_blocking() {
        let (capture, mut receiver) = Capture::channel(1);
        let operation = Uuid::new_v4();
        for _ in 0..2 {
            capture.decision(operation, None, "zero_available_slots", 1);
        }
        assert_eq!(capture.dropped.load(Ordering::Relaxed), 1);
        assert!(matches!(
            receiver.try_recv().unwrap(),
            Observation::Decision { operation_id, .. } if operation_id == operation
        ));
        drop(receiver);
        capture.decision(operation, None, "unknown", 2);
        assert_eq!(capture.dropped.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a_rolled_back_clock_cannot_make_a_claim_interval() {
        let (capture, mut receiver) = Capture::channel(1);
        capture.claim(Uuid::new_v4(), Uuid::new_v4(), 1, 2, 1);
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }
}
