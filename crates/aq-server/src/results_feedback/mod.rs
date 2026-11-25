//! Revision-pinned human feedback. Task state and result acceptance stay with
//! their existing owners; annotations never confer completion authority.
pub(crate) mod actions;
mod api;
pub mod contract;
mod store;
pub(crate) use api::with_feed;
pub use api::{detail, routes};
