//! Task version and project scope used to record a host routing result.
use crate::{host_values::HostResult, host9};
use serde_json::Value;
use uuid::Uuid;

pub struct RouteTarget<'a> {
    pub id: &'a str,
    pub task: Uuid,
    pub workspace: Uuid,
    pub project: Uuid,
    pub version: i64,
    pub input: &'a Value,
}
#[derive(Clone, Copy)]
pub struct RoutingFailure<'a> {
    pub at: (Uuid, Uuid),
    pub outcome: &'a str,
    pub version: i64,
    pub input: &'a Value,
}
impl RouteTarget<'_> {
    pub fn refusal(
        &self,
        name: &str,
        asked: i64,
        stored: Option<aq_routing::Source>,
    ) -> Option<HostResult> {
        let refused = |reason| {
            Some(HostResult {
                outcome: "refused".into(),
                output: host9::refusal(self.id, self.version, reason),
            })
        };
        if name == "work.unroutable" && self.input["reasons"].as_array().is_none_or(Vec::is_empty) {
            return refused("no_reasons");
        }
        if stored == Some(aq_routing::Source::Owner) {
            return refused("owner_route");
        }
        (asked != self.version).then(|| self.stale())
    }
    pub fn location(&self) -> (Uuid, Uuid, u64) {
        (self.workspace, self.task, self.version as u64)
    }
    pub fn stale(&self) -> HostResult {
        HostResult {
            outcome: "stale".into(),
            output: host9::task_output(self.id, self.version),
        }
    }
    pub fn failure<'a>(&'a self, outcome: &'a str, version: i64) -> RoutingFailure<'a> {
        RoutingFailure {
            at: (self.task, self.project),
            outcome,
            version,
            input: self.input,
        }
    }
}
