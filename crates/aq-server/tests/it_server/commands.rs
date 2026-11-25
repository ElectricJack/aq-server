//! Spec 0006 phase 1: the command registry is the contract.
//! - `protocol/commands/v1/commands.json` is generated from the registry and
//!   must not drift (`AQ_REGENERATE_COMMANDS=1` rewrites it).
//! - Deny by default: every route aq-server mounts is a registered command
//!   route or on the reviewed list in `protocol/commands/v1/routes.json`.
use aq_command_registry::routes::{self, Class};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// The dashboard action union this build serves (the published one without
/// the feature, checked by `dashboard_actions_contract -- --check`).
fn actions() -> serde_json::Value {
    serde_json::from_slice(&aq_server::dashboard_api::actions::contract::schema_bytes()).unwrap()
}

/// The published contract is the feature-off build's; with
/// `project_environments` the generated one adds `task.needs.set` and
/// `needs` (checked by [`the_project_environments_contract_adds_needs`]).
#[cfg(not(feature = "project_environments"))]
// commands.json documents the default build.
#[cfg(not(feature = "supervisor_full_access"))]
#[test]
fn commands_json_is_generated_from_the_registry() {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../protocol/commands/v1/commands.json");
    let mut generated = serde_json::to_string_pretty(&aq_server::commands::document()).unwrap();
    generated.push('\n');
    if std::env::var_os("AQ_REGENERATE_COMMANDS").is_some() {
        std::fs::write(&path, &generated).unwrap();
    }
    let published = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        published == generated,
        "protocol/commands/v1/commands.json drifted from the registry; regenerate it with \
         AQ_REGENERATE_COMMANDS=1 cargo nextest run -p aq-server --test it_server -E 'test(/^commands::/)'"
    );
}

#[test]
fn every_action_kind_is_a_command_with_a_declared_grant() {
    let registry = aq_server::commands::registry();
    let schema = actions();
    for kind in schema["$defs"]["ActionRequest"]["oneOf"]
        .as_array()
        .unwrap()
    {
        let kind = kind["properties"]["kind"]["const"].as_str().unwrap();
        let c = registry
            .get(kind)
            .unwrap_or_else(|| panic!("{kind} is not a command"));
        assert!(c.grant.is_some(), "{kind}");
        assert!(c.owner.is_some(), "{kind}: the owner reaches every action");
    }
    for c in registry.commands() {
        assert!(
            c.owner.is_some() || c.supervisor.is_some() || c.agent.is_some(),
            "{}",
            c.name
        );
    }
}

#[test]
fn every_command_has_its_own_summary() {
    for c in aq_server::commands::registry().commands() {
        assert!(
            c.summary != aq_command_registry::PLACEHOLDER_SUMMARY && !c.summary.trim().is_empty(),
            "{} has no summary; add it to crates/aq-command-registry/src/summaries.rs",
            c.name
        );
    }
}

/// Every action kind has a subject rule (aq-command-registry subjects.rs),
/// and the rule's params are what the kind's payload schema says: an id or
/// revision "from the payload" is a payload field, and an id param of its
/// own is not (payloads refuse unknown fields, so it is taken out).
#[test]
fn subject_rules_match_the_action_schemas() {
    use aq_command_registry::SubjectId;
    let schema = actions();
    for kind in schema["$defs"]["ActionRequest"]["oneOf"]
        .as_array()
        .unwrap()
    {
        let name = kind["properties"]["kind"]["const"].as_str().unwrap();
        let fields = kind["properties"]["payload"]["properties"]
            .as_object()
            .unwrap();
        let rule = aq_command_registry::subject_rule(name)
            .unwrap_or_else(|| panic!("{name} has no subject rule"));
        match rule.id {
            SubjectId::Payload(p) => assert!(fields.contains_key(p), "{name}: {p}"),
            SubjectId::Param(p) => assert!(!fields.contains_key(p), "{name}: {p}"),
            SubjectId::Fixed(_) => {}
        }
        if let Some((param, _)) = rule.revision {
            assert!(fields.contains_key(param), "{name}: {param}");
        }
    }
}

/// The agent route path the registry gives `name`.
fn registry_agent_path(name: &str) -> String {
    let command = aq_server::commands::registry().get(name);
    let route = command.and_then(|c| c.agent);
    route
        .unwrap_or_else(|| panic!("{name} has no agent route"))
        .path
        .to_string()
}

fn route_constants() -> BTreeMap<(&'static str, &'static str), String> {
    // Paths given by a constant rather than a literal.
    [
        (
            ("questions.rs", "ROUTES[0].0"),
            aq_server::agent_access::questions::ROUTES[0].0.to_string(),
        ),
        (
            ("questions.rs", "ROUTES[1].0"),
            aq_server::agent_access::questions::ROUTES[1].0.to_string(),
        ),
        (
            ("routes.rs", "ROUTES[0].0"),
            aq_server::agent_access::waits::routes::ROUTES[0]
                .0
                .to_string(),
        ),
        (
            ("routes.rs", "ROUTES[1].0"),
            aq_server::agent_access::waits::routes::ROUTES[1]
                .0
                .to_string(),
        ),
        (
            ("routes.rs", "ROUTES[2].0"),
            aq_server::agent_access::waits::routes::ROUTES[2]
                .0
                .to_string(),
        ),
        (
            ("comments.rs", "add_path"),
            aq_task_comments::AGENT_ROUTES[0].0.to_string(),
        ),
        (
            ("comments.rs", "list_path"),
            aq_task_comments::AGENT_ROUTES[1].0.to_string(),
        ),
        (
            ("inbox.rs", "LIST_ROUTE"),
            aq_server::agent_access::questions::inbox::LIST_ROUTE.to_string(),
        ),
        (
            ("inbox.rs", "DETAIL_ROUTE"),
            aq_server::agent_access::questions::inbox::DETAIL_ROUTE.to_string(),
        ),
        (
            ("api.rs", "PATH"),
            "/v1/worker/profiled/material/refresh".to_string(),
        ),
        // `results.rs` mounts LIST, SHOW and PATCH: the agent routes of the
        // `result.review.*` commands, so a path that drifts from the
        // registry is not mounted under the name the registry gives.
        (
            ("results.rs", "LIST"),
            registry_agent_path("result.review.list"),
        ),
        (
            ("results.rs", "SHOW"),
            registry_agent_path("result.review.show"),
        ),
        (
            ("results.rs", "PATCH"),
            registry_agent_path("result.review.patch"),
        ),
    ]
    .into_iter()
    .collect()
}

/// `.route(path, ...)` in the lib sources, outside `#[cfg(test)] mod` tails.
fn mounted() -> BTreeMap<String, BTreeSet<String>> {
    let consts = route_constants();
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut stack = vec![
        root.join("src"),
        root.join("../aq-manual-sessions/src"),
        root.join("../aq-supervisor-control/src"),
        root.join("../aq-swarm-settings/src"),
        root.join("../aq-role-settings/src"),
        root.join("../aq-internal-work/src"),
        root.join("../aq-playbook-authoring/src"),
    ];
    // Spec 0017: the node routes of `project_environments` (see `reviewed_node_route`).
    #[cfg(feature = "project_environments")]
    stack.push(root.join("../aq-environments/src"));
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let name = path.file_name().unwrap().to_str().unwrap().to_string();
            // Static dashboard asset routes are generated from the bundle.
            if !name.ends_with(".rs") || name == "dashboard_assets.rs" {
                continue;
            }
            // This module is mounted only by the delivery_playbook build.
            // Its registered commands carry the same feature gate.
            if !cfg!(feature = "delivery_playbook")
                && path.ends_with("profiled_execution/human_results/publication.rs")
            {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            for route in parse_source_routes(&path, &text, &consts) {
                out.entry(route).or_default().insert(name.clone());
            }
        }
    }
    out
}

fn parse_source_routes(
    path: &Path,
    text: &str,
    consts: &BTreeMap<(&str, &str), String>,
) -> Vec<String> {
    let name = path.file_name().unwrap().to_str().unwrap();
    let mut out = Vec::new();
    let text = text.split("\n#[cfg(test)]\nmod ").next().unwrap();
    let mut rest = text;
    while let Some(i) = rest.find(".route(") {
        let head = &rest[..i];
        let before = head.chars().next_back();
        rest = &rest[i + 7..];
        // A router chain (`.route(` after whitespace or a closing
        // paren) or a router variable (`routes.route(`).
        let chained = before.is_some_and(|c| c.is_whitespace() || c == ')');
        if !chained && !head.ends_with("routes") && !head.ends_with("router") {
            continue;
        }
        let arg = rest[..rest.find(',').unwrap()].trim();
        let route = if let Some(lit) = arg.strip_prefix('"') {
            lit.trim_end_matches('"').to_string()
        } else {
            consts
                .get(&(name, arg))
                .unwrap_or_else(|| panic!("{}: unresolved route path `{arg}`", path.display()))
                .clone()
        };
        out.push(route);
    }
    // A nest mounts its prefix; the nested routes are scanned where declared.
    let mut rest = text;
    while let Some(i) = rest.find(".nest(") {
        rest = &rest[i + 6..];
        if let Some(lit) = rest.trim_start().strip_prefix('"') {
            let prefix = lit[..lit.find('"').unwrap()].to_string();
            out.push(prefix);
        }
    }
    out
}

#[test]
fn source_scan_keeps_router_routes_constants_and_nests_outside_test_modules() {
    let source = r#"
Router::new()
    .route("/chained", get(handler))
    .nest("/nested", nested);
routes.route(PATH, get(handler));
router.route("/variable", get(handler));
registry.route("/not-a-router", get(handler));
#[cfg(test)]
mod tests {
    router.route("/test-only", get(handler));
}
"#;
    let consts = BTreeMap::from([(("fixture.rs", "PATH"), "/constant".to_owned())]);
    assert_eq!(
        parse_source_routes(Path::new("fixture.rs"), source, &consts),
        ["/chained", "/constant", "/variable", "/nested"]
    );
}

/// Node routes reviewed here rather than in the published `routes.json`,
/// which documents the default build: spec 0017's builder and environment
/// routes under the node credential (`worker_api::authenticate_context`;
/// claim and complete only for the `template-build` worker group), mounted
/// only with `project_environments`. They join `routes.json` when the
/// feature is removed.
fn reviewed_node_route(path: &str) -> bool {
    cfg!(feature = "project_environments")
        && matches!(
            path,
            "/v1/worker/environments"
                | "/v1/worker/environments/ready"
                | "/v1/worker/environments/claim"
                | "/v1/worker/environments/complete"
        )
}

#[test]
fn every_mounted_route_is_a_command_or_reviewed() {
    let registry = aq_server::commands::registry();
    let mounted = mounted();
    assert!(mounted.len() > 150, "the scan found the routes");
    let unclassified: Vec<_> = mounted
        .iter()
        .filter(|(path, _)| {
            routes::classify(registry, path).is_none() && !reviewed_node_route(path)
        })
        .collect();
    assert!(
        unclassified.is_empty(),
        "routes outside the command registry must be reviewed and listed in \
         protocol/commands/v1/routes.json: {unclassified:?}"
    );
    for reviewed in routes::reviewed() {
        assert!(
            mounted.contains_key(&reviewed.path),
            "{} is listed but no longer mounted",
            reviewed.path
        );
        if reviewed.path.starts_with("/v1/worker/")
            || reviewed.path.starts_with("/v2/worker/")
            || reviewed.path.starts_with("/v1/execution/")
        {
            assert_eq!(reviewed.class, Class::Node, "{}", reviewed.path);
        }
    }
    // Agent-facing commands never route into the node namespaces.
    for c in registry.commands() {
        if let Some(r) = &c.agent {
            assert!(
                r.path.starts_with("/v1/agent/") || r.path.starts_with("/api/v1/commands/"),
                "{}: {}",
                c.name,
                r.path
            );
        }
    }
}

#[test]
fn the_command_surface_has_its_own_admission_class() {
    use aq_server::admission::{Class, classify};
    assert_eq!(classify("/api/v1/commands"), Class::AgentCommand);
    assert_eq!(
        classify("/api/v1/commands/task.create"),
        Class::AgentCommand
    );
    assert_eq!(classify("/v1/agent/tasks"), Class::General);
}

/// Spec 0006 §9 step 2: every `/v1/agent/*` route the server mounts is the
/// agent route of a registry command, so `/api/v1/commands/{name}` reaches the
/// same handler (and so the same domain function, receipts and idempotency).
/// The one exception is `/v1/agent/context`, superseded by `whoami` and
/// reviewed as `agent_legacy`. The §9 table is checked row by row.
#[test]
fn section_9_maps_every_agent_route_to_its_registry_command() {
    let registry = aq_server::commands::registry();
    let agent_route = |name: &str| {
        let c = registry
            .get(name)
            .unwrap_or_else(|| panic!("{name} is not registered"));
        c.agent
            .map(|r| (r.method, r.path))
            .unwrap_or_else(|| panic!("{name} has no agent route"))
    };
    let table: &[(&str, &[(&str, &str)])] = &[
        (
            "comments",
            &[
                ("task.comment.add", "/v1/agent/comments"),
                ("comment.list", "/v1/agent/comments/list"),
            ],
        ),
        (
            "questions",
            &[
                ("question.ask", "/v1/agent/questions"),
                ("question.show", "/v1/agent/questions/show"),
            ],
        ),
        (
            "waits",
            &[
                ("wait.register", "/v1/agent/waits/register"),
                ("wait.inspect", "/v1/agent/waits/inspect"),
                ("wait.ack", "/v1/agent/waits/ack"),
            ],
        ),
        (
            "messages",
            &[
                ("message.send", "/v1/agent/messages/send"),
                ("message.inbox", "/v1/agent/messages/inbox"),
                ("message.ack", "/v1/agent/messages/ack"),
                ("message.body", "/v1/agent/messages/body"),
            ],
        ),
        (
            "graph",
            &[
                ("knowledge.search", "/v1/agent/graph/read"),
                ("knowledge.show", "/v1/agent/graph/read"),
                ("graph.read", "/v1/agent/graph/read"),
                ("graph.command", "/v1/agent/graph/commands"),
            ],
        ),
        ("context", &[("whoami", "/api/v1/commands/whoami")]),
    ];
    for (row, commands) in table {
        for (name, path) in *commands {
            assert_eq!(agent_route(name).1, *path, "§9 {row}: {name}");
        }
    }
    let served: BTreeSet<&str> = registry
        .commands()
        .iter()
        .filter_map(|c| c.agent.map(|r| r.path))
        .collect();
    let agent: Vec<String> = mounted()
        .into_keys()
        .filter(|p| p.starts_with("/v1/agent/"))
        .collect();
    assert!(
        agent.len() >= 19,
        "the scan found the agent routes: {agent:?}"
    );
    for path in &agent {
        if path == "/v1/agent/context" {
            let reviewed = routes::reviewed()
                .into_iter()
                .find(|r| &r.path == path)
                .unwrap();
            assert_eq!(reviewed.class, Class::AgentLegacy, "{path}");
            continue;
        }
        assert!(
            served.contains(path.as_str()),
            "{path} has no registry command (spec 0006 §9 step 2)"
        );
    }
}

/// Spec 0017: with `project_environments` the owner's `task.needs.set` is a
/// command on the action route with the `task.edit` grant, its payload and
/// the task create payloads take `needs`, and a request validates.
#[cfg(feature = "project_environments")]
#[test]
fn the_project_environments_contract_adds_needs() {
    let c = aq_server::commands::registry()
        .get("task.needs.set")
        .unwrap();
    assert_eq!(c.grant, Some(aq_agent_grants::op::TASK_EDIT));
    assert!(c.owner.is_some() && c.supervisor.is_none() && c.agent.is_none());
    let schema = actions();
    let kinds = schema["$defs"]["ActionRequest"]["oneOf"]
        .as_array()
        .unwrap();
    let payload = |kind: &str| {
        let found = kinds
            .iter()
            .find(|k| k["properties"]["kind"]["const"] == kind);
        found.unwrap()["properties"]["payload"].clone()
    };
    assert!(payload("task.needs.set")["properties"]["needs"].is_object());
    assert!(payload("task.create")["properties"]["needs"].is_object());
    let graph = payload("task.graph.create").to_string();
    assert!(graph.contains("\"needs\""), "{graph}");
    let set = payload("task.needs.set");
    let valid = serde_json::json!({"operation_id":"00000000-0000-4000-8000-000000000206",
        "project_id":"00000000-0000-4000-8000-000000000010","expected_version":"4","needs":["node","playwright"]});
    assert!(aq_agent_api::check(&set, &set, &valid), "{set}");
    let unknown = serde_json::json!({"operation_id":"00000000-0000-4000-8000-000000000206",
        "project_id":"00000000-0000-4000-8000-000000000010","expected_version":"4","role":"x"});
    assert!(!aq_agent_api::check(&set, &set, &unknown));
    let document = aq_server::commands::document().to_string();
    assert!(document.contains("task.needs.set"));
}
