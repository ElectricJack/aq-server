//! The sources the server defines itself (spec 0013 section 7).
use crate::{SourceKind, SourceRecord};
use aq_internal_work::Purpose;
use aq_playbooks_compiler::{docs, draft};
use std::collections::BTreeSet;

/// The id of the source whose document says who takes each task.
pub const ROUTING: &str = "routing";
/// The binding its playbook is activated on.
pub const ROUTING_BINDING: &str = "aq.routing";
/// The fact provider that says what Settings offers.
pub const ROUTING_OPTIONS: &str = "routing_options";

/// The record of the `routing` source: it reads a task, asks a structured
/// judgment and writes the route (or says the document allows nobody), when
/// the server says a task needs a route. Its judgments run as routing
/// classification. Its scope is `project`, the scope of every grant of the
/// namespace; that it serves the tasks of every project on the router is the
/// namespace's `installation_routing`, not the playbook's scope.
pub fn routing() -> SourceRecord {
    let names =
        |items: &[&str]| -> BTreeSet<String> { items.iter().map(|s| s.to_string()).collect() };
    let commands = names(&[
        "work.read",
        "judgment.assess",
        "work.route",
        "work.unroutable",
    ]);
    let events = names(&["aq.work.route_needed"]);
    // What those commands need, the reconciliation of the judgment included.
    let capabilities = draft::retain_registry(&docs::catalog_registry(), &commands, &events)
        .map(|(_, capabilities)| capabilities)
        .unwrap_or_default();
    SourceRecord {
        id: ROUTING.into(),
        kind: SourceKind::Builtin,
        binding: ROUTING_BINDING.into(),
        scope: "project".into(),
        purpose: "Decide how strong a model each task needs and which agents may take it".into(),
        judgment_purpose: Purpose::RoutingClassification,
        commands,
        events,
        capabilities,
        facts: vec![ROUTING_OPTIONS.into()],
        revision: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_routing_record_may_do_what_routing_needs_and_no_more() {
        let record = routing();
        let capabilities: Vec<&str> = record.capabilities.iter().map(String::as_str).collect();
        assert_eq!(
            capabilities,
            [
                "judgment.assess",
                "operation.get",
                "work.read",
                "work.route",
                "work.unroutable"
            ]
        );
        assert_eq!(
            (record.scope.as_str(), record.binding.as_str()),
            ("project", "aq.routing")
        );
        // The catalog renders for it, so a workspace can be built.
        docs::catalog(&record.commands, &record.events).unwrap();
    }
}
