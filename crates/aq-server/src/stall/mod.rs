//! H3 stall ladder. A pure deterministic [`ladder::step`] decides one rung at a
//! time from [`evidence::Evidence`]. [`engine::tick`] persists each intent
//! (stable action id, expected revision) before its effect, and records an effect
//! receipt after it. Effects are idempotent by action id, so a daemon restart
//! re-runs an unreceipted intent instead of repeating or losing it.
pub mod engine;
pub mod evidence;
pub mod ladder;
pub mod policy;
pub mod ports;
pub mod runtime;
pub mod store;

use uuid::Uuid;

const EPISODE_NAMESPACE: Uuid = Uuid::from_bytes(*b"aq-stall-episode");

/// One episode per profiled session (attempt).
pub fn episode_id(attempt: Uuid) -> Uuid {
    Uuid::new_v5(&EPISODE_NAMESPACE, attempt.as_bytes())
}

/// Stable action identity: the episode, the intent kind and its ordinal.
pub fn action_id(episode: Uuid, kind: &str, ordinal: u32) -> Uuid {
    Uuid::new_v5(&episode, format!("{kind}:{ordinal}").as_bytes())
}
