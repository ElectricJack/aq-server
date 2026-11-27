//! The sources the server defines itself (spec 0013 section 7).
use crate::{SourceKind, SourceRecord};
use aq_internal_work::Purpose;
use aq_playbooks_compiler::draft;
use std::collections::BTreeSet;

/// The id of the source whose document says who takes each task.
pub const ROUTING: &str = "routing";
/// The binding its playbook is activated on.
pub const ROUTING_BINDING: &str = "aq.routing";
/// The fact provider that says what Settings offers.
pub const ROUTING_OPTIONS: &str = "routing_options";

/// The id of the source whose document says what happens to a task's result
/// (spec 0016 §8).
#[cfg(feature = "delivery_playbook")]
pub const DELIVERY: &str = "delivery";
/// The binding its playbook is activated on (`aq-playbook-domain`'s
/// `DELIVERY_BINDING`; a server test keeps the two equal).
#[cfg(feature = "delivery_playbook")]
pub const DELIVERY_BINDING: &str = "aq.delivery";

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
    let mut record = SourceRecord {
        id: ROUTING.into(),
        kind: SourceKind::Builtin,
        binding: ROUTING_BINDING.into(),
        scope: "project".into(),
        purpose: "Decide how strong a model each task needs and which agents may take it".into(),
        judgment_purpose: Purpose::RoutingClassification,
        commands,
        events,
        capabilities: BTreeSet::new(),
        facts: vec![ROUTING_OPTIONS.into()],
        revision: 0,
    };
    // What those commands need, the reconciliation of the judgment included.
    let registry = crate::request::pinned_catalog(&record).expect("the routing catalog parses");
    record.capabilities = draft::retain_registry(&registry, &record.commands, &record.events)
        .map(|(_, capabilities)| capabilities)
        .unwrap_or_default();
    record
}

/// The record of the `delivery` source: on `aq.work.result_ready` it reads
/// the task and its result (`work.read` of catalog 2.8.0: the result facts of
/// 2.3.0, the task's acceptance criteria and the result's conclusion) and
/// settles it with the delivery commands of catalog 2.3.0: publish,
/// open a pull request, wait for checks, merge, complete, return or hold. It
/// may ask a structured judgment (`judgment.assess`), for example whether a
/// result that changed nothing meets the task; whether and when to ask is the
/// document's. Its judgments run as playbook calls, bounded by the budget the
/// playbook gives and internal work's caps.
/// Its scope is `project`, the only scope the catalog's contracts have; that
/// one document serves every project is the namespace's `installation_routing`
/// (see `routing()`). It is authored against catalog 2.8.0
/// ([`crate::request::pinned_catalog`]). The document is the only thing it
/// reads besides the task: Settings offers no fact provider to it, and the
/// numbers it uses (returns allowed, check wait) are the document's own words.
///
/// `environments` is whether the engine's namespace serves the environment
/// commands (catalog 2.5.0, spec 0017): only then does the record ask them.
/// A build with `project_environments` may still run a Host10 namespace,
/// whose grants refuse them.
#[cfg(feature = "delivery_playbook")]
pub fn delivery(environments: bool) -> SourceRecord {
    let names =
        |items: &[&str]| -> BTreeSet<String> { items.iter().map(|s| s.to_string()).collect() };
    #[allow(unused_mut)]
    let mut commands = names(&[
        "work.read",
        "judgment.assess",
        "work.complete",
        "work.return",
        "work.hold",
        "supervisor.notify",
        "main.sync",
        "branch.publish",
        "pull_request.open",
        "pull_request.await_checks",
        "pull_request.merge",
        "pull_request.close",
    ]);
    #[allow(unused_mut)]
    let mut events = names(&["aq.work.result_ready"]);
    // Spec 0017 §6.3-6.4: build and admit environments, and rebuild on a new
    // base (`aq.environment.completed` comes with the commands).
    #[cfg(all(feature = "delivery_playbook", feature = "project_environments"))]
    if environments {
        commands.extend(names(&["environment.build", "environment.admit"]));
        events.extend(names(&["aq.environment.base_changed"]));
    }
    let mut record = SourceRecord {
        id: DELIVERY.into(),
        kind: SourceKind::Builtin,
        binding: DELIVERY_BINDING.into(),
        scope: "project".into(),
        purpose: "Decide what happens to a task's result: deliver it, send it back or hold it"
            .into(),
        judgment_purpose: Purpose::PlaybookCall,
        commands,
        events,
        capabilities: BTreeSet::new(),
        facts: vec![],
        revision: 0,
    };
    #[cfg(not(feature = "project_environments"))]
    let _ = environments;
    let registry = crate::request::pinned_catalog(&record).expect("the delivery catalog parses");
    record.capabilities = draft::retain_registry(&registry, &record.commands, &record.events)
        .map(|(_, capabilities)| capabilities)
        .unwrap_or_default();
    record
}

#[cfg(test)]
mod tests {
    use super::*;
    use aq_playbooks_compiler::docs;

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

    /// Whatever the build: a Host10 namespace refuses the environment commands.
    #[cfg(feature = "delivery_playbook")]
    #[test]
    fn the_delivery_record_may_settle_a_result_and_no_more() {
        let record = delivery(false);
        assert_eq!(
            (
                record.id.as_str(),
                record.binding.as_str(),
                record.scope.as_str()
            ),
            ("delivery", "aq.delivery", "project")
        );
        assert_eq!(record.kind, SourceKind::Builtin);
        assert_eq!(
            record.events.iter().map(String::as_str).collect::<Vec<_>>(),
            ["aq.work.result_ready"]
        );
        let commands: Vec<&str> = record.commands.iter().map(String::as_str).collect();
        assert_eq!(
            commands,
            [
                "branch.publish",
                "judgment.assess",
                "main.sync",
                "pull_request.await_checks",
                "pull_request.close",
                "pull_request.merge",
                "pull_request.open",
                "supervisor.notify",
                "work.complete",
                "work.hold",
                "work.read",
                "work.return"
            ]
        );
        // Each command is covered by its capability, and nothing routes; the
        // judgment's reconciliation comes with it, billed as a playbook call.
        for command in &record.commands {
            assert!(record.capabilities.contains(command), "{command}");
        }
        assert!(record.capabilities.contains("operation.get"));
        assert!(!record.capabilities.contains("work.route"));
        assert_eq!(record.judgment_purpose, Purpose::PlaybookCall);
        assert!(record.facts.is_empty());
        // It is authored against catalog 2.8.0: 2.3.0 with the work.read whose
        // facts add the acceptance criteria and the result's conclusion.
        let catalog = crate::request::pinned_catalog(&record).unwrap();
        assert_eq!(catalog.commands["work.read"].version, "2.8.0");
        assert!(catalog.events.contains_key("aq.work.result_ready"));
        let v2_3: aq_playbooks_model::Registry =
            serde_json::from_str(docs::CATALOG_2_3_REGISTRY).unwrap();
        for (name, contract) in &v2_3.commands {
            assert_eq!(name == "work.read", &catalog.commands[name] != contract);
        }
        let workspace = crate::request::workspace_files(
            &record,
            crate::request::WorkspaceSource {
                markdown: "review",
                facts: &Default::default(),
                facts_version: "1",
            },
            &catalog,
            None,
        )
        .unwrap();
        for fact in [
            "review_approved",
            "review_changes_requested",
            "review_incomplete",
            "aq.work.result_ready",
            "`acceptance_criteria`",
            "`conclusion`",
            "judgment.assess",
        ] {
            assert!(workspace["CATALOG.md"].contains(fact), "{fact}");
        }
        // 2.2.0 does not know the event: it is not the catalog of this record.
        assert!(docs::catalog(&record.commands, &record.events).is_err());
    }

    /// The worked delivery example of catalog 2.3.0 seals as a playbook of
    /// this record, against catalog 2.8.0, with the record's scope, binding and
    /// abilities (`project` is the one scope the contracts allow).
    #[cfg(feature = "delivery_playbook")]
    #[test]
    fn the_delivery_example_seals_as_the_delivery_record() {
        const DIR: &str = "../../vendor/aq-playbooks-c744301/contracts/aq-host-2.3.0/examples/deliver-by-playbook";
        let read = |name: &str| {
            std::fs::read_to_string(format!("{}/{DIR}/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
        };
        let record = delivery(false);
        let catalog = crate::request::pinned_catalog(&record).unwrap();
        let ws = draft::Workspace {
            markdown: read("source.md"),
            record: crate::request::engine_record(&record, "settings-1"),
            catalog: crate::request::catalog_for(&record, &catalog).unwrap(),
            facts: serde_json::from_str(&read("facts.json")).unwrap(),
            draft: serde_json::from_str(&read("draft.json")).unwrap(),
        };
        let artifact = draft::seal(&ws).unwrap_or_else(|faults| panic!("{faults:?}"));
        let playbook = &artifact.content.playbook;
        assert_eq!(
            (playbook.id.as_str(), playbook.scope.as_str()),
            ("aq.delivery", "project")
        );
        assert!(
            playbook
                .required_capabilities
                .is_subset(&record.capabilities)
        );
    }

    /// Spec 0017: catalog 2.5.0 adds the environment commands and the base
    /// event, and keeps every 2.8.0 contract the record used.
    #[cfg(all(feature = "delivery_playbook", feature = "project_environments"))]
    #[test]
    fn the_delivery_record_builds_and_admits_environments_on_catalog_2_5_0() {
        let record = delivery(true);
        let commands: Vec<&str> = record.commands.iter().map(String::as_str).collect();
        assert_eq!(
            commands,
            [
                "branch.publish",
                "environment.admit",
                "environment.build",
                "judgment.assess",
                "main.sync",
                "pull_request.await_checks",
                "pull_request.close",
                "pull_request.merge",
                "pull_request.open",
                "supervisor.notify",
                "work.complete",
                "work.hold",
                "work.read",
                "work.return"
            ]
        );
        let events: Vec<&str> = record.events.iter().map(String::as_str).collect();
        assert_eq!(
            events,
            ["aq.environment.base_changed", "aq.work.result_ready"]
        );
        for command in &record.commands {
            assert!(record.capabilities.contains(command), "{command}");
        }
        assert!(!record.capabilities.contains("work.route"));
        let catalog = crate::request::pinned_catalog(&record).unwrap();
        let v2_8: aq_playbooks_model::Registry =
            serde_json::from_str(crate::request::CATALOG_2_8_REGISTRY).unwrap();
        let v2_5: aq_playbooks_model::Registry =
            serde_json::from_str(crate::request::CATALOG_2_5_REGISTRY).unwrap();
        for name in &record.commands {
            let expected = v2_8.commands.get(name).unwrap_or(&v2_5.commands[name]);
            assert_eq!(&catalog.commands[name], expected, "{name}");
        }
        let kept = crate::request::catalog_for(&record, &catalog).unwrap();
        assert!(kept.events.contains_key("aq.environment.completed"));
        assert!(kept.events.contains_key("aq.delivery.completed"));
        let workspace = crate::request::workspace_files(
            &record,
            crate::request::WorkspaceSource {
                markdown: "review",
                facts: &Default::default(),
                facts_version: "1",
            },
            &catalog,
            None,
        )
        .unwrap();
        let notes = &workspace["CATALOG.md"];
        for name in [
            "environment.build",
            "aq.environment.base_changed",
            "review_approved",
            "`conclusion`",
        ] {
            assert!(notes.contains(name), "{name}");
        }
    }

    /// Spec 0017 §5.4: the routing record is authored against catalog 2.5.0,
    /// whose `work.read` gives needs and environments and whose `work.route`
    /// takes the chosen environment; its other commands are 2.2.0's.
    #[cfg(all(feature = "delivery_playbook", feature = "project_environments"))]
    #[test]
    fn the_routing_record_chooses_environments_on_catalog_2_5_0() {
        let record = routing();
        let catalog = crate::request::pinned_catalog(&record).unwrap();
        let v2_2 = docs::catalog_registry();
        for name in &record.commands {
            let contract = &catalog.commands[name];
            match name.as_str() {
                "work.read" | "work.route" => assert_eq!(contract.version, "2.5.0"),
                _ => assert_eq!(contract, &v2_2.commands[name], "{name}"),
            }
        }
        let workspace = crate::request::workspace_files(
            &record,
            crate::request::WorkspaceSource {
                markdown: "route",
                facts: &Default::default(),
                facts_version: "1",
            },
            &catalog,
            None,
        )
        .unwrap();
        let notes = &workspace["CATALOG.md"];
        for name in ["default_environment", "environment", "judgment.assess"] {
            assert!(notes.contains(name), "{name}");
        }
        assert!(!notes.contains("environment.build"));
    }

    /// A Host10 routing namespace authors on catalog 2.3.0: its workspace
    /// describes 2.3.0's work.route, without the 2.5.0 environment.
    #[cfg(all(feature = "delivery_playbook", feature = "project_environments"))]
    #[test]
    fn the_routing_record_on_a_host10_namespace_gets_catalog_2_3_0_notes() {
        let record = routing();
        let catalog = crate::request::catalog_2_3().unwrap();
        let workspace = crate::request::workspace_files(
            &record,
            crate::request::WorkspaceSource {
                markdown: "route",
                facts: &Default::default(),
                facts_version: "1",
            },
            &catalog,
            None,
        )
        .unwrap();
        assert!(workspace["CATALOG.md"].contains("work.route"));
        assert!(!workspace["CATALOG.md"].contains("default_environment"));
    }

    /// The published routing example of spec 0017 §5.4 (vendored byte for
    /// byte) seals as a playbook of this record against its catalog, and its
    /// cases pass.
    #[cfg(all(feature = "delivery_playbook", feature = "project_environments"))]
    #[test]
    fn the_route_to_environment_example_seals_as_the_routing_record() {
        let root = format!(
            "{}/../../vendor/aq-route-to-environment-613db7c",
            env!("CARGO_MANIFEST_DIR")
        );
        let read = |name: &str| std::fs::read_to_string(format!("{root}/{name}")).unwrap();
        let pin: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(format!("{root}-source.json")).unwrap())
                .unwrap();
        for file in pin["files"].as_array().unwrap() {
            let bytes =
                std::fs::read(format!("{root}/{}", file["path"].as_str().unwrap())).unwrap();
            assert_eq!(file["bytes"], bytes.len());
            let sha = <sha2::Sha256 as sha2::Digest>::digest(&bytes);
            assert_eq!(file["sha256"].as_str().unwrap(), format!("{sha:x}"));
        }
        let mut names: Vec<String> = std::fs::read_dir(format!("{root}/cases"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        let cases: Vec<_> = names
            .iter()
            .map(|name| serde_json::from_str(&read(&format!("cases/{name}"))).unwrap())
            .collect();
        assert_eq!(cases.len(), 8);
        let record = routing();
        let inputs = crate::Inputs {
            grant_capabilities: record.capabilities.clone(),
            catalog: crate::request::pinned_catalog(&record).unwrap(),
            record,
            markdown: read("source.md"),
            facts: serde_json::from_str(&read("facts.json")).unwrap(),
            facts_version: "settings-7".into(),
            previous: None,
        };
        let draft = serde_json::from_str(&read("draft.json")).unwrap();
        let saved = crate::check_saved(&draft, &cases, &inputs, &Default::default())
            .unwrap_or_else(|refusal| panic!("{refusal:?}"));
        assert_eq!(saved.artifact.content.playbook.id.as_str(), "aq.routing");
    }

    /// The published 2.5.0 delivery example seals as a playbook of this record.
    #[cfg(all(feature = "delivery_playbook", feature = "project_environments"))]
    #[test]
    fn the_environment_delivery_example_seals_as_the_delivery_record() {
        const DIR: &str = "../../vendor/aq-host-catalog-2.5.0/examples/deliver-with-environments";
        let read = |name: &str| {
            std::fs::read_to_string(format!("{}/{DIR}/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
        };
        let record = delivery(true);
        let catalog = crate::request::pinned_catalog(&record).unwrap();
        let ws = draft::Workspace {
            markdown: read("source.md"),
            record: crate::request::engine_record(&record, "settings-1"),
            catalog: crate::request::catalog_for(&record, &catalog).unwrap(),
            facts: serde_json::from_str(&read("facts.json")).unwrap(),
            draft: serde_json::from_str(&read("draft.json")).unwrap(),
        };
        let artifact = draft::seal(&ws).unwrap_or_else(|faults| panic!("{faults:?}"));
        let playbook = &artifact.content.playbook;
        assert_eq!(playbook.id.as_str(), "aq.delivery");
        assert!(
            playbook
                .required_capabilities
                .is_subset(&record.capabilities)
        );
    }
}
