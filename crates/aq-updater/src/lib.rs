//! Host-side release updater: config, durable journal, history and lock,
//! release verification, the runtime boundary (with the `docker` CLI
//! implementation), the update engine, and the spool shared with the server.
//! The `aq-updater` binary in `main.rs` wires them together.

pub mod config;
pub mod engine;
pub mod journal;
pub mod runtime;
pub mod spool;
pub mod verify;

#[cfg(test)]
pub(crate) mod test_support;
