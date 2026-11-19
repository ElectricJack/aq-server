//! H3 stall ladder. A pure deterministic [`ladder::step`] decides one rung at a
//! time from [`evidence::Evidence`]. [`engine::tick`] persists each intent
//! (stable action id, expected revision) before its effect, and records an effect
//! receipt after it. Effects are idempotent by action id, so a daemon restart
//! re-runs an unreceipted intent instead of repeating or losing it.
pub mod engine;
pub mod ports;
pub mod runtime;
pub mod store;
pub use aq_stall_ladder::{action_id, episode_id, evidence, ladder, policy};
