//! The pure part of the H3 stall ladder: a deterministic [`ladder::step`]
//! decides one rung at a time from [`evidence::Evidence`]. aq-server's
//! `stall::engine` persists each intent before its effect and runs it.
pub mod evidence;
pub mod ladder;
pub mod policy;

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
