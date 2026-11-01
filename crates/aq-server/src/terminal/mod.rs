//! AQ21 staged terminal runtime. Transport/storage integration requires the
//! common profiled AuthorityView; absent configuration never enables input.
pub mod config;
pub mod persistence;
pub mod protection;
pub mod runtime;
pub mod service;

pub mod api;

pub mod peer;

pub mod tail;
