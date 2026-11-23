//! Host9: AQ host catalog 2.2.0 (spec 0013 §6; plan tasks B2 to B5).
use aq_playbook_host::{
    DomainHost, HostMode, MemoryStorage, Owner, Request,
    model::{CommandContract, Registry, digest},
    runner::{Authority, DispatchResult, Host, Intent},
};
use aq_server::playbooks::{
    coding::{CodingPolicy, ROOT},
    domain::GrantScope,
    review_domain::{
        CatalogProfile, HOST7_CAPABILITIES, HOST8_FINGERPRINT, HOST9_CAPABILITIES, HOST9_COMMANDS,
        HOST9_FINGERPRINT, HOST9_VERSION,
    },
};
use serde_json::Value;
use std::{collections::BTreeSet, time::Duration};
use uuid::Uuid;

mod fixture;

const DEADLINE: Duration = Duration::from_secs(5);
const ADDED: [&str; 4] = [
    "work.read",
    "work.route",
    "work.unroutable",
    "judgment.assess",
];

#[test]
fn host9_registry_matches_the_pinned_manifest() {
    let registry = CatalogProfile::Host9.registry();
    let manifest: Value = serde_json::from_str(include_str!(
        "../../../../vendor/aq-host-catalog-2.2.0/manifest.json"
    ))
    .unwrap();
    assert_eq!(manifest["version"], HOST9_VERSION);
    assert_eq!(manifest["fingerprint"], HOST9_FINGERPRINT);
    assert_eq!(manifest["supersedes"]["fingerprint"], HOST8_FINGERPRINT);
    assert_eq!(
        digest(&(HOST9_VERSION, &registry)).unwrap(),
        HOST9_FINGERPRINT
    );
    assert_eq!(CatalogProfile::Host9.number(), 9);
    assert!(CatalogProfile::Host9.is_host2());
    let commands: BTreeSet<&str> = registry.commands.keys().map(String::as_str).collect();
    assert_eq!(commands, BTreeSet::from(HOST9_COMMANDS));
    for name in manifest["commands_added"].as_array().unwrap() {
        assert!(ADDED.contains(&name.as_str().unwrap()));
    }
    for event in ["aq.judgment.assessed", "aq.work.route_needed"] {
        assert!(registry.events.contains_key(event), "{event}");
    }
    // 2.2.0 only adds: every 2.1.0 contract is unchanged.
    let host8 = CatalogProfile::Host8.registry();
    for (name, contract) in &host8.commands {
        assert_eq!(registry.commands.get(name), Some(contract), "{name}");
    }
    for (name, contract) in &host8.events {
        assert_eq!(registry.events.get(name), Some(contract), "{name}");
    }
    // The pin lists the vendored files byte for byte.
    let pin: Value = serde_json::from_str(include_str!(
        "../../../../vendor/aq-host-catalog-2.2.0-source.json"
    ))
    .unwrap();
    fixture::assert_pinned_files(&pin, HOST9_FINGERPRINT, "aq-host-catalog-2.2.0");
}

struct Idle;
impl Host for Idle {
    fn authorize(&self, _: &Authority, _: &CommandContract) -> bool {
        true
    }
    fn dispatch(&mut self, _: &Intent) -> DispatchResult {
        DispatchResult::Unknown {
            reason: "no adapter in this test".into(),
        }
    }
    fn reconcile(&mut self, intent: &Intent) -> DispatchResult {
        self.dispatch(intent)
    }
}
impl DomainHost for Idle {
    fn set_mode(&mut self, _: HostMode) {}
}

#[tokio::test]
async fn a_host8_coding_artifact_installs_in_a_host9_namespace() {
    let registry: Registry = CatalogProfile::Host9.registry();
    let owner = Owner::start_registered(
        aq_playbook_host::Registration::new(
            "host9-coding".into(),
            aq_playbook_host::Limits::default(),
            registry,
        ),
        || Ok(MemoryStorage::default()),
        |_| Ok(Idle),
    )
    .await
    .unwrap();
    // Sealed against the Host8 registry (catalog 2.1.0).
    let artifacts = CodingPolicy::project_default(vec![]).build().unwrap();
    for artifact in artifacts.install_order() {
        let artifact = Box::new(artifact.clone());
        (owner.call(Request::Install { artifact }, DEADLINE).await).unwrap();
    }
    let authority = Authority {
        actor: "owner".into(),
        scope: "project".into(),
        capabilities: HOST7_CAPABILITIES.iter().map(|c| c.to_string()).collect(),
    };
    let activate = Request::Activate {
        binding: ROOT.into(),
        artifact: artifacts.root.digest.clone(),
        authority,
    };
    owner.call(activate, DEADLINE).await.unwrap();
    owner.call(Request::Stop, DEADLINE).await.unwrap();
}

#[test]
fn host9_scope_admits_the_new_capabilities_and_nothing_else() {
    let scope = |capabilities: &[&str]| GrantScope {
        workspace_id: Uuid::new_v4(),
        project_id: Uuid::new_v4(),
        recipients: vec![],
        capabilities: capabilities.iter().map(|c| c.to_string()).collect(),
    };
    assert_eq!(HOST9_CAPABILITIES.len(), 16);
    assert_eq!(HOST9_CAPABILITIES[..12], HOST7_CAPABILITIES);
    assert_eq!(HOST9_CAPABILITIES[12..], ADDED);
    let host9 = CatalogProfile::Host9;
    assert!(host9.validate_scope(&scope(&HOST9_CAPABILITIES)).is_ok());
    for one in ADDED {
        assert!(host9.validate_scope(&scope(&[one])).is_ok(), "{one}");
        // Host8 keeps its twelve.
        assert!(
            CatalogProfile::Host8
                .validate_scope(&scope(&[one]))
                .is_err()
        );
    }
    for other in ["work.delete", "judgment.request", ""] {
        assert!(host9.validate_scope(&scope(&[other])).is_err(), "{other}");
    }
    assert!(host9.validate_scope(&scope(&[])).is_err());
    // Every catalog capability is one the profile admits, and no more.
    let declared: BTreeSet<String> = (host9.registry().commands.values())
        .flat_map(|c| c.capabilities.iter().cloned())
        .collect();
    let admitted: BTreeSet<String> = HOST9_CAPABILITIES.iter().map(|c| c.to_string()).collect();
    assert_eq!(declared, admitted);
}

#[test]
fn a_namespace_of_another_profile_is_refused_with_what_to_do() {
    let host9 = CatalogProfile::Host9;
    assert_eq!(host9.namespace_refusal(1, None), None);
    assert_eq!(host9.namespace_refusal(1, Some((9, 1))), None);
    for (number, name) in [
        (2, "human_review_v2"),
        (6, "monitoring_v6"),
        (8, "host_v8"),
        (11, "host_v11"),
    ] {
        assert_eq!(
            CatalogProfile::from_number(number).unwrap().config_name(),
            name
        );
        let why = host9.namespace_refusal(1, Some((number, 1))).unwrap();
        assert!(
            why.contains(&format!("exists with profile \"{name}\"")),
            "{why}"
        );
        assert!(why.contains("profile=\"host_v9\"") && why.contains("new namespace_id"));
    }
    // A generation change alone is refused the same way.
    assert!(host9.namespace_refusal(2, Some((9, 1))).is_some());
    assert_eq!(CatalogProfile::from_number(12), None);
}

// ------------------------------------------------------------------------
// B3 to B5: the Host9 adapters on disposable PostgreSQL through the
// production bridge service. The graph, the routing port and the judgment
// provider are in-memory fakes of the injected ports.
mod adapters;

// ------------------------------------------------------------------------
// Epic 0 (spec 0016 section 6): host catalog 2.3.0 (profile 10), the event
// `aq.work.result_ready` and the result facts of `work.read`.
#[cfg(feature = "delivery_playbook")]
mod delivery;

#[cfg(feature = "delivery_playbook")]
pub(crate) use delivery::commands::GraphMergeHost;
