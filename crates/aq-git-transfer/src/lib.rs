//! Spec 0016 §5.2 (E0.3): an attempt's git grant and the `/v1/git` proxy.
//!
//! An offer carries a grant naming the repository, the base commit, the task
//! ref `refs/heads/aq/<task-id>` and its lease, with a token valid for that
//! attempt only. `/v1/git/<repository>/…` authenticates the token, checks it
//! names this repository and a live attempt, and streams git's smart-HTTP
//! request to aq-git's git socket (`aq-git serve --git-http-socket`). The
//! grant headers aq-git trusts are set from the attempt's stored grant; no
//! client header is copied into them.
#[cfg(feature = "delivery_playbook")]
mod config;
#[cfg(feature = "delivery_playbook")]
mod grant;
#[cfg(feature = "delivery_playbook")]
mod proxy;
#[cfg(feature = "delivery_playbook")]
pub use {config::*, grant::*, proxy::*};
#[cfg(all(test, feature = "delivery_playbook"))]
mod e2e_tests;
#[cfg(all(feature = "delivery_playbook", any(test, feature = "test-support")))]
pub mod fake;
