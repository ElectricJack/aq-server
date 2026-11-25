//! D11 routing, watcher and report pilots, sealed from the pinned Host8
//! (catalog 2.1.0) contracts with the D9 builder. They run on the production
//! catalog 2.x (Host7/Host8) adapters; nothing here talks to a host.
//!
//! - [`routing`] (`aq.pilot.routing`): read the attempt's permitted execution
//!   options (`context.read view=execution_options`). Complete facts pin the
//!   observed selection; missing facts ask one `judgment.request` (async; the
//!   constraint names the retained `aq.judgment.completed` judgment). Either way the work's current revision
//!   is read and `work.change variant=execution_constraints` is applied only
//!   at that revision: `routed`, or `stale` on a concurrent change. Actual
//!   placement stays AQ-owned.
//! - [`watcher`] (`aq.pilot.watcher`): the trusted host evaluator has started
//!   (captured prior episode revisions) and completed a health scan; the run
//!   requires a complete page-0 `context.read view=health_observations`
//!   before `findings.reconcile`. A missing page or incomplete read returns
//!   `incomplete` and never reconciles; a stale baseline is `conflict`. The
//!   reminder cadence is the run's `reminder_ms` parameter.
//! - [`report`] (`aq.pilot.report`): freeze the project-visible context
//!   window (`context.read view=project`), `work.ensure` exactly one
//!   `supervisor_authoring` item, wait for it until the deadline, then send
//!   exactly one `message.send`: the authored result, or a deterministic
//!   fallback. A late author completion is retained by the host and engine;
//!   the run has already sent its one report.
use super::*;

pub const ROUTING: &str = "aq.pilot.routing";
pub const WATCHER: &str = "aq.pilot.watcher";
pub const REPORT: &str = "aq.pilot.report";
/// The deterministic report body used when no authored result arrives by
/// the deadline.
pub const REPORT_FALLBACK: &str = "Supervisor report: the authored summary did not arrive by the report deadline. \
     The frozen project context window is retained in AQ.";
const JUDGMENT_WAIT_MS: u64 = 600_000;

fn is_true(e: Expr) -> Expr {
    Expr::Eq {
        left: Box::new(e),
        right: Box::new(lit(json!(true))),
    }
}
fn read(view: &str, subject: Expr, page: Expr) -> Expr {
    fields(&[
        ("view", lit(json!(view))),
        ("subject", subject),
        ("page", page),
        ("snapshot", lit(json!(""))),
    ])
}

mod report;
mod routing;
mod watcher;
pub use report::report;
pub use routing::routing;
pub use watcher::watcher;
