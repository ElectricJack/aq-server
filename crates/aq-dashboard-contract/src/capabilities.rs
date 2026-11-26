//! Ordered capability projections for dashboard-read-1. The service supplies verified availability.
use crate::{Capability, CapabilityVersion};

pub fn capability(name: &str, enabled: bool, reason: &str) -> Capability {
    Capability {
        operation_retention: (name == "dashboard.actions").then(Default::default),
        name: name.into(),
        version: CapabilityVersion::V1,
        enabled,
        reason: if enabled { None } else { Some(reason.into()) },
    }
}

fn append(caps: &mut Vec<Capability>, enabled: bool, descriptions: &[(&str, &str)]) {
    caps.extend(
        descriptions
            .iter()
            .map(|(name, reason)| capability(name, enabled, reason)),
    );
}

pub struct GraphCapabilities {
    pub read: bool,
    pub comments: bool,
    pub mutate: bool,
}
impl GraphCapabilities {
    pub fn append_to(&self, caps: &mut Vec<Capability>) {
        append(caps, self.read, GRAPH_READS);
        caps.push(capability(
            "tasks.comments.write",
            self.comments,
            "Writing task comments requires an owner account, Graph runtime and dashboard actions",
        ));
        append(caps, self.mutate, GRAPH_MUTATIONS);
        caps.push(capability(
            "knowledge.read",
            self.read,
            "Knowledge inspection requires an owner account and configured Graph runtime",
        ));
    }
}

const GRAPH_READS: &[(&str, &str)] = &[
    (
        "human.graph.read",
        "Human Graph inspection requires an owner account and configured Graph runtime",
    ),
    (
        "human.overview.tasks.read",
        "Recorded task summaries require an owner account and configured Graph runtime",
    ),
    (
        "human.work.tasks.read",
        "Work across projects requires an owner account and configured Graph runtime",
    ),
    (
        "tasks.read",
        "Task inspection requires an owner account and configured Graph runtime",
    ),
    (
        "tasks.comments.read",
        "Task comments require an owner account and configured Graph runtime",
    ),
];

const GRAPH_MUTATIONS: &[(&str, &str)] = &[
    (
        "tasks.create",
        "Task creation requires an owner account and configured Graph mutation identity",
    ),
    (
        "tasks.graph.create",
        "Task graph creation requires an owner account and configured Graph mutation identity",
    ),
    (
        "epic.retirement.register",
        "Epic retirement registration requires an owner account and configured Graph mutation identity",
    ),
    (
        "tasks.edit",
        "Task edits require an owner account and configured Graph mutation identity",
    ),
    (
        "tasks.story_points",
        "Editing task estimates requires an owner account and configured Graph mutation identity",
    ),
    (
        "tasks.reprioritize",
        "Task reprioritization requires an owner account and configured Graph mutation identity",
    ),
    (
        "tasks.route",
        "Task routes require an owner account and configured Graph mutation identity",
    ),
    (
        "tasks.dependencies.add",
        "Dependency changes require an owner account and configured Graph mutation identity",
    ),
    (
        "tasks.dependencies.remove",
        "Dependency changes require an owner account and configured Graph mutation identity",
    ),
    (
        "tasks.delete",
        "Task deletion requires an owner account and configured Graph mutation identity",
    ),
    (
        "tasks.activate",
        "Task activation requires an owner account and configured Graph mutation identity",
    ),
    (
        "tasks.activate.subtree",
        "Activating draft descendants requires an owner account and configured Graph mutation identity",
    ),
    (
        "tasks.complete",
        "Completing a task requires an owner account and configured Graph mutation identity",
    ),
    (
        "tasks.reopen",
        "Reopening a task requires an owner account and configured Graph mutation identity",
    ),
    (
        "tasks.cancel",
        "Task cancellation requires an owner account and configured Graph mutation identity",
    ),
    (
        "tasks.retry",
        "Task retry requires an owner account and configured Graph mutation identity",
    ),
    (
        "knowledge.author",
        "Knowledge authoring requires an owner account and configured Graph mutation identity",
    ),
    (
        "knowledge.correct",
        "Knowledge correction requires an owner account and configured Graph mutation identity",
    ),
    (
        "knowledge.verify",
        "Knowledge verification requires an owner account and configured Graph mutation identity",
    ),
    (
        "knowledge.retire",
        "Knowledge retirement requires an owner account and configured Graph mutation identity",
    ),
    (
        "knowledge.restore",
        "Knowledge restore requires an owner account and configured Graph mutation identity",
    ),
];
pub fn playbook_capabilities(namespace: bool, definitions: bool, catalog: bool) -> [Capability; 8] {
    [
        capability(
            "playbooks.inspect",
            namespace,
            "Playbook inspection requires the mounted runtime namespace owner",
        ),
        capability(
            "playbooks.definition-graph.read",
            namespace,
            "Compiled graphs require the mounted runtime namespace owner",
        ),
        capability(
            "playbooks.loaded-definitions.read",
            definitions,
            "Loaded definitions require the configured runtime namespace owner",
        ),
        capability(
            "playbooks.catalog.read",
            catalog,
            "Current catalog inspection requires the mounted component query and namespace owner",
        ),
        capability(
            "playbooks.runs.start",
            namespace,
            "Starting runs requires the mounted runtime namespace owner",
        ),
        capability(
            "playbooks.runs.cancel",
            namespace,
            "Cancelling runs requires the mounted runtime namespace owner",
        ),
        capability(
            "playbooks.activations.activate",
            namespace,
            "Activating definitions requires the mounted runtime namespace owner",
        ),
        capability(
            "playbooks.activations.rollback",
            namespace,
            "Rolling back definitions requires the mounted runtime namespace owner",
        ),
    ]
}

pub fn providers(owner: bool, can_control_availability: bool) -> [Capability; 7] {
    [
        capability(
            "providers.configured.read",
            owner,
            "Configured provider inspection requires an owner account",
        ),
        capability(
            "provider.quota",
            owner,
            "Provider account usage is unavailable",
        ),
        capability(
            "capacity.read",
            owner,
            "Capacity and provider availability require an owner account",
        ),
        capability("swarm.read", owner, "The swarm requires an owner account"),
        capability(
            "swarm.worker.control",
            owner,
            "Worker control requires an owner account",
        ),
        capability(
            "swarm.worker.enroll",
            owner,
            "Adding a machine requires an owner account",
        ),
        capability(
            "swarm.availability.control",
            can_control_availability,
            "Owner availability requires an owner account and a configured intelligence registry",
        ),
    ]
}

pub struct ResultSources {
    pub results: bool,
    pub media_read: bool,
    pub media_upload: bool,
}
impl ResultSources {
    pub fn capabilities(&self, owner: bool) -> [Capability; 5] {
        let readable = owner && self.results;
        [
            capability(
                "results.read",
                readable,
                "Result inspection requires an owner account",
            ),
            capability(
                "results.comment",
                readable,
                "Comments require an owner account and a mounted Results owner",
            ),
            capability(
                "results.feedback",
                readable,
                "Feedback requires an owner account and a mounted Results owner",
            ),
            capability(
                "results.media.read",
                self.media_read && owner,
                "Supplemental images require an owner account and mounted media storage",
            ),
            capability(
                "results.media.upload",
                self.media_upload && owner,
                if !owner {
                    "Supplemental image uploads require an owner account"
                } else if !self.media_read {
                    "Result media store is not configured"
                } else {
                    "Result media uploads are disabled"
                },
            ),
        ]
    }
}
